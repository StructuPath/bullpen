//! GitHub operations, by shelling out to the [gh] CLI.
//!
//! One tool, raw `gh` arguments as an array — no shell in between, so
//! nothing needs quoting and nothing can be injected. The tool adds what
//! gh cannot know: the runtime decides parallel safety from the verb (a
//! `view` can ride alongside anything; a `merge` cannot), a sandbox that
//! denies network refuses the call outright, and a missing or logged-out
//! binary is a clear error with the setup hint.
//!
//! gh runs with the user's own login and acts with their authority on
//! remote state — which the workspace sandbox's *write* confinement
//! deliberately does not govern: it fences the filesystem, not GitHub.
//!
//! [gh]: https://cli.github.com

use std::process::Stdio;
use std::time::Duration;

use bullpen_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Tool, ToolCtx, ToolError, truncate_middle};

const TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 100_000;

/// Second-position verbs that only read remote state. `api` is absent on
/// purpose: it can carry any method, so it stays serial. Defaults closed.
const READ_VERBS: &[&str] = &["view", "list", "diff", "checks", "status"];

pub struct GitHub {
    binary: String,
}

impl GitHub {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            binary: "gh".into(),
        }
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

fn parse_args(input: &Value) -> Result<Vec<String>, ToolError> {
    let args: Vec<String> = input
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if args.is_empty() {
        return Err(ToolError::InvalidInput(
            "`args` must be a non-empty array of gh arguments, e.g. \
             [\"pr\", \"view\", \"18\"]"
                .into(),
        ));
    }
    Ok(args)
}

#[async_trait::async_trait]
impl Tool for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "github".into(),
            description: "Run a GitHub CLI (gh) command with the user's login: \
                          `args` is the argument array, e.g. [\"pr\", \"list\"], \
                          [\"pr\", \"view\", \"18\", \"--comments\"], [\"run\", \
                          \"view\", \"--log-failed\"], [\"issue\", \"create\", \
                          \"--title\", ...]. No shell is involved — pass each \
                          argument as its own array element. Acts as the user on \
                          GitHub, so mutations (create, merge, close) are real; \
                          prefer read commands unless the task calls for a \
                          change. Needs gh installed and authenticated."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "gh arguments, one element each"
                    }
                },
                "required": ["args"]
            }),
        }
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        // A read (`pr view`, `run list`, `search …`) can run alongside
        // anything; everything else — mutations and `api` — stays serial.
        match parse_args(input) {
            Ok(args) => {
                args.first().is_some_and(|first| first == "search")
                    || args
                        .get(1)
                        .is_some_and(|verb| READ_VERBS.contains(&verb.as_str()))
            }
            Err(_) => false,
        }
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        if let Some(sandbox) = &ctx.sandbox
            && !sandbox.capabilities().allow_network
        {
            return Err(ToolError::Failed(
                "sandbox: network access is disabled, so GitHub is out of reach".into(),
            ));
        }
        let args = parse_args(&input)?;

        let child = tokio::process::Command::new(&self.binary)
            .args(&args)
            .current_dir(&ctx.workspace)
            // Never fall into an interactive prompt a headless run cannot
            // answer.
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                ToolError::Failed(format!(
                    "failed to run {}: {e} — install the GitHub CLI from \
                     https://cli.github.com and run `gh auth login`",
                    self.binary
                ))
            })?;
        let out = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), child.wait_with_output())
            .await
            .map_err(|_| ToolError::Timeout(TIMEOUT_SECS))?
            .map_err(|e| ToolError::Failed(format!("failed to collect output: {e}")))?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            return Err(ToolError::Failed(format!(
                "gh {} exited with {}:\n{}",
                args.join(" "),
                out.status.code().unwrap_or(-1),
                truncate_middle(format!("{}{}", stderr.trim(), stdout.trim()), 4_000)
            )));
        }
        let text = if stdout.trim().is_empty() {
            // Some gh commands (e.g. `run watch`) narrate on stderr.
            stderr.into_owned()
        } else {
            stdout.into_owned()
        };
        Ok(if text.trim().is_empty() {
            "(no output)".into()
        } else {
            truncate_middle(text, MAX_OUTPUT_BYTES)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(dir: &tempfile::TempDir) -> ToolCtx {
        ToolCtx::new(dir.path().to_path_buf())
    }

    /// A stand-in gh: records its arguments, prints a canned payload.
    fn stub(dir: &tempfile::TempDir, payload: &str, exit: i32) -> PathBuf {
        let path = dir.path().join("fake-gh");
        let script = format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" > {}/args.txt\n\
             printf '%s\\n' \"{payload}\"\nexit {exit}\n",
            dir.path().display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // See ast.rs: exec'ing a just-written script can hit ETXTBSY under
        // parallel test load; probe until spawnable.
        for _ in 0..200 {
            if std::process::Command::new(&path).arg("-h").output().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        path
    }

    fn tool(path: &std::path::Path) -> GitHub {
        GitHub::with_binary(path.display().to_string())
    }

    #[tokio::test]
    async fn runs_gh_with_the_args_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(&dir, "18 OPEN feat: things", 0);
        let out = tool(&bin)
            .run(
                &ctx(&dir),
                "t",
                json!({"args": ["pr", "view", "18", "--comments"]}),
            )
            .await
            .unwrap();
        assert_eq!(out.trim(), "18 OPEN feat: things");
        let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        // The -h element is the warm-up probe's leftover only if the real
        // call never overwrote it; the real call must have.
        assert_eq!(args.trim(), "pr view 18 --comments");
    }

    #[tokio::test]
    async fn failures_carry_the_exit_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(&dir, "auth required", 4);
        let err = tool(&bin)
            .run(&ctx(&dir), "t", json!({"args": ["pr", "list"]}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exited with 4"), "{msg}");
        assert!(msg.contains("auth required"), "{msg}");
    }

    #[tokio::test]
    async fn a_missing_binary_hints_at_setup() {
        let dir = tempfile::tempdir().unwrap();
        let err = GitHub::with_binary("/nonexistent/gh")
            .run(&ctx(&dir), "t", json!({"args": ["pr", "list"]}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cli.github.com"), "{msg}");
        assert!(msg.contains("gh auth login"), "{msg}");
    }

    #[tokio::test]
    async fn a_network_denying_sandbox_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = std::sync::Arc::new(bullpen_sandbox::Sandbox::strict(dir.path()));
        let c = ToolCtx::new(dir.path()).with_sandbox(sandbox);
        let err = GitHub::new()
            .run(&c, "t", json!({"args": ["pr", "list"]}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("network"), "{err}");
    }

    #[tokio::test]
    async fn empty_args_are_invalid() {
        let dir = tempfile::tempdir().unwrap();
        for input in [json!({}), json!({"args": []})] {
            let err = GitHub::new().run(&ctx(&dir), "t", input).await.unwrap_err();
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
        }
    }

    #[test]
    fn only_reads_are_parallel_safe() {
        let gh = GitHub::new();
        for read in [
            json!({"args": ["pr", "view", "18"]}),
            json!({"args": ["pr", "list"]}),
            json!({"args": ["run", "list"]}),
            json!({"args": ["pr", "checks", "18"]}),
            json!({"args": ["pr", "diff", "18"]}),
            json!({"args": ["repo", "view"]}),
            json!({"args": ["search", "code", "foo"]}),
        ] {
            assert!(gh.parallel_safe(&read), "{read}");
        }
        for write in [
            json!({"args": ["pr", "create"]}),
            json!({"args": ["pr", "merge", "18"]}),
            json!({"args": ["issue", "close", "3"]}),
            json!({"args": ["api", "repos/o/r/pulls"]}),
            json!({"args": []}),
        ] {
            assert!(!gh.parallel_safe(&write), "{write}");
        }
        assert!(!gh.replay_safe());
    }
}

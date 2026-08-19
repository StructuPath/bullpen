//! Structural code search and rewrite, by shelling out to [ast-grep].
//!
//! Two tools share one adapter: `ast_grep` finds pattern matches as
//! `file:line` results, `ast_edit` rewrites them — previewing by default
//! and applying only on `apply: true`, so the model sees the diff before
//! anything changes on disk. The binary is discovered at call time
//! (`ast-grep`, then `sg`, verified by `--version`), and its absence is a
//! clear error with an install hint, never a degraded fallback to text
//! search.
//!
//! Applying under a sandbox runs the binary through the same shell
//! wrapper as `bash`, so on macOS the rewrite is Seatbelt-confined with
//! everything else; elsewhere it carries bash's documented caveat.
//!
//! [ast-grep]: https://ast-grep.github.io

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use bullpen_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Tool, ToolCtx, ToolError, required_str, truncate_middle};

const TIMEOUT_SECS: u64 = 120;
const MAX_MATCHES: usize = 200;
const MAX_OUTPUT_BYTES: usize = 100_000;

/// Locate a working ast-grep binary: an explicit override, else `ast-grep`
/// or `sg` on PATH — accepted only if `--version` says it is ast-grep,
/// because `sg` is also the Unix shell-group utility.
async fn find_binary(explicit: Option<&PathBuf>) -> Result<String, ToolError> {
    let candidates: Vec<String> = match explicit {
        Some(path) => vec![path.display().to_string()],
        None => vec!["ast-grep".into(), "sg".into()],
    };
    for candidate in &candidates {
        let probe = tokio::process::Command::new(candidate)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        if let Ok(out) = probe
            && out.status.success()
            && String::from_utf8_lossy(&out.stdout).starts_with("ast-grep")
        {
            return Ok(candidate.clone());
        }
    }
    Err(ToolError::Failed(format!(
        "no ast-grep binary found (tried {}) — install it from \
         https://ast-grep.github.io (e.g. `cargo install ast-grep`) to use \
         structural search",
        candidates.join(", ")
    )))
}

/// Shared inputs: the pattern, an optional language, and a path (default
/// the workspace root).
fn common_args(input: &Value) -> Result<(String, Vec<String>), ToolError> {
    let pattern = required_str(input, "pattern")?.to_string();
    let mut args = vec!["run".to_string(), "--pattern".to_string(), pattern];
    if let Some(lang) = input.get("lang").and_then(Value::as_str) {
        args.push("--lang".into());
        args.push(lang.into());
    }
    let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
    Ok((path.to_string(), args))
}

async fn run_binary(
    ctx: &ToolCtx,
    program: &str,
    args: &[String],
) -> Result<std::process::Output, ToolError> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(&ctx.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError::Failed(format!("failed to run {program}: {e}")))?;
    tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), child.wait_with_output())
        .await
        .map_err(|_| ToolError::Timeout(TIMEOUT_SECS))?
        .map_err(|e| ToolError::Failed(format!("failed to collect output: {e}")))
}

fn failed(program: &str, out: &std::process::Output) -> ToolError {
    ToolError::Failed(format!(
        "{program} exited with {}:\n{}",
        out.status.code().unwrap_or(-1),
        truncate_middle(String::from_utf8_lossy(&out.stderr).into_owned(), 2_000)
    ))
}

/// `ast_grep`: structural search over the workspace.
pub struct AstGrep {
    binary: Option<PathBuf>,
}

impl AstGrep {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { binary: None }
    }

    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self {
            binary: Some(path.into()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for AstGrep {
    fn name(&self) -> &'static str {
        "ast_grep"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ast_grep".into(),
            description: "Structural code search via ast-grep: `pattern` is \
                          matched against the syntax tree, not text, with \
                          metavariables like $NAME and $$$ARGS (e.g. \
                          `foo($$$ARGS)` finds every call to foo). Results \
                          are `file:line` plus the matched text. `lang` \
                          forces a language; `path` narrows the search. \
                          Needs the ast-grep binary installed."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Structural pattern, e.g. `foo($$$ARGS)`"},
                    "lang": {"type": "string", "description": "Language to parse as (e.g. rust, ts, python); inferred from extensions when omitted"},
                    "path": {"type": "string", "description": "File or directory to search (default: workspace root)"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn parallel_safe(&self, _input: &Value) -> bool {
        true
    }

    fn replay_safe(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let program = find_binary(self.binary.as_ref()).await?;
        let (path, mut args) = common_args(&input)?;
        args.push("--json=stream".into());
        args.push(path);

        let out = run_binary(ctx, &program, &args).await?;
        if !out.status.success() {
            return Err(failed(&program, &out));
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let matches: Vec<String> = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|m| {
                let file = m["file"].as_str().unwrap_or("?").to_string();
                // ast-grep reports 0-based lines; the workspace convention
                // (read_file, grep) is 1-based.
                let line = m["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
                let text = m["text"].as_str().unwrap_or("").to_string();
                let (first, rest) = match text.split_once('\n') {
                    Some((first, _)) => (first, " …"),
                    None => (text.as_str(), ""),
                };
                format!("{file}:{line}\t{first}{rest}")
            })
            .collect();

        if matches.is_empty() {
            return Ok("no matches".into());
        }
        let shown = matches
            .iter()
            .take(MAX_MATCHES)
            .cloned()
            .collect::<Vec<_>>();
        let mut result = format!("{} match(es):\n{}", matches.len(), shown.join("\n"));
        if matches.len() > MAX_MATCHES {
            result.push_str(&format!("\n[stopped at {MAX_MATCHES} matches]"));
        }
        Ok(truncate_middle(result, MAX_OUTPUT_BYTES))
    }
}

/// `ast_edit`: structural rewrite, previewed by default.
pub struct AstEdit {
    binary: Option<PathBuf>,
}

impl AstEdit {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { binary: None }
    }

    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self {
            binary: Some(path.into()),
        }
    }
}

/// Quote one argument for `bash -c`, for the sandbox-wrapped apply path.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[async_trait::async_trait]
impl Tool for AstEdit {
    fn name(&self) -> &'static str {
        "ast_edit"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ast_edit".into(),
            description: "Structural rewrite via ast-grep: every match of \
                          `pattern` becomes `rewrite`, with metavariables \
                          carried over (e.g. `foo($A)` → `bar($A)`). By \
                          default nothing is written — the call returns the \
                          diff to review; call again with `apply: true` to \
                          write it. `lang` and `path` as in ast_grep. Needs \
                          the ast-grep binary installed."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Structural pattern to match"},
                    "rewrite": {"type": "string", "description": "Replacement, using the pattern's metavariables"},
                    "lang": {"type": "string", "description": "Language to parse as; inferred from extensions when omitted"},
                    "path": {"type": "string", "description": "File or directory to rewrite (default: workspace root)"},
                    "apply": {"type": "boolean", "description": "Write the changes (default: false — preview the diff)"}
                },
                "required": ["pattern", "rewrite"]
            }),
        }
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        // A preview is a read; an apply mutates the workspace.
        !input.get("apply").and_then(Value::as_bool).unwrap_or(false)
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let program = find_binary(self.binary.as_ref()).await?;
        let rewrite = required_str(&input, "rewrite")?.to_string();
        let apply = input.get("apply").and_then(Value::as_bool).unwrap_or(false);
        let (path, mut args) = common_args(&input)?;
        args.push("--rewrite".into());
        args.push(rewrite);
        if apply {
            args.push("--update-all".into());
        }
        args.push(path);

        let out = if apply && let Some(sandbox) = &ctx.sandbox {
            // The same posture as `bash`: the rewrite runs through the
            // sandbox's shell wrapper, Seatbelt-confined where the OS
            // enforces it.
            let command = std::iter::once(program.as_str())
                .chain(args.iter().map(String::as_str))
                .map(shell_quote)
                .collect::<Vec<_>>()
                .join(" ");
            let (wrapped, wrapped_args) = sandbox.wrap_bash(&command);
            run_binary(ctx, &wrapped, &wrapped_args).await?
        } else {
            run_binary(ctx, &program, &args).await?
        };
        if !out.status.success() {
            return Err(failed(&program, &out));
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        if apply {
            // ast-grep reports "Applied N changes" on stderr, not stdout.
            let stderr = String::from_utf8_lossy(&out.stderr);
            let summary = format!("{}\n{}", stdout.trim(), stderr.trim());
            let summary = summary.trim();
            return Ok(if summary.is_empty() {
                "no matches — nothing changed".into()
            } else {
                truncate_middle(summary.to_string(), MAX_OUTPUT_BYTES)
            });
        }
        if stdout.trim().is_empty() {
            return Ok("no matches — nothing to rewrite".into());
        }
        Ok(truncate_middle(
            format!(
                "preview only — nothing written; call again with `apply: true` \
                 to write this:\n\n{stdout}"
            ),
            MAX_OUTPUT_BYTES,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &tempfile::TempDir) -> ToolCtx {
        ToolCtx::new(dir.path().to_path_buf())
    }

    /// A stand-in binary: passes the version probe, records its arguments,
    /// and prints a canned payload.
    fn stub(dir: &tempfile::TempDir, payload: &str) -> PathBuf {
        let path = dir.path().join("fake-ast-grep");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo ast-grep 0.0.0-stub; exit 0; fi\n\
             printf '%s ' \"$@\" > {}/args.txt\ncat <<'PAYLOAD'\n{payload}\nPAYLOAD\n",
            dir.path().display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Exec'ing a just-written script races concurrent tests' forks on
        // Linux (ETXTBSY: a forked child still holds the write fd until its
        // own exec completes). Probe until the script is spawnable.
        for _ in 0..200 {
            if std::process::Command::new(&path)
                .arg("--version")
                .output()
                .is_ok()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        path
    }

    fn recorded_args(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("args.txt")).unwrap()
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_clear_error_with_an_install_hint() {
        let dir = tempfile::tempdir().unwrap();
        let err = AstGrep::with_binary("/nonexistent/ast-grep")
            .run(&ctx(&dir), "t", json!({"pattern": "foo($A)"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no ast-grep binary"), "{msg}");
        assert!(msg.contains("install"), "{msg}");
    }

    #[tokio::test]
    async fn search_renders_matches_one_indexed_and_passes_the_right_flags() {
        let dir = tempfile::tempdir().unwrap();
        let payload = r#"{"text":"foo(1)","file":"a.rs","range":{"start":{"line":1}}}
{"text":"foo(2)\nmore","file":"b/c.rs","range":{"start":{"line":41}}}"#;
        let bin = stub(&dir, payload);

        let out = AstGrep::with_binary(&bin)
            .run(
                &ctx(&dir),
                "t",
                json!({"pattern": "foo($A)", "lang": "rust", "path": "src"}),
            )
            .await
            .unwrap();
        assert!(out.contains("2 match(es):"), "{out}");
        assert!(out.contains("a.rs:2\tfoo(1)"), "{out}");
        // Multi-line match text keeps only its first line.
        assert!(out.contains("b/c.rs:42\tfoo(2) …"), "{out}");

        let args = recorded_args(&dir);
        assert!(args.contains("run --pattern foo($A) --lang rust"), "{args}");
        assert!(args.contains("--json=stream src"), "{args}");
    }

    #[tokio::test]
    async fn edit_previews_by_default_and_applies_only_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(&dir, "a.rs\n1│-foo(1)\n1│+bar(1)");

        let out = AstEdit::with_binary(&bin)
            .run(
                &ctx(&dir),
                "t",
                json!({"pattern": "foo($A)", "rewrite": "bar($A)"}),
            )
            .await
            .unwrap();
        assert!(out.contains("preview only"), "{out}");
        assert!(out.contains("apply: true"), "{out}");
        let args = recorded_args(&dir);
        assert!(args.contains("--rewrite bar($A)"), "{args}");
        assert!(!args.contains("--update-all"), "{args}");

        let bin = stub(&dir, "Applied 2 changes");
        let out = AstEdit::with_binary(&bin)
            .run(
                &ctx(&dir),
                "t",
                json!({"pattern": "foo($A)", "rewrite": "bar($A)", "apply": true}),
            )
            .await
            .unwrap();
        assert_eq!(out, "Applied 2 changes");
        assert!(recorded_args(&dir).contains("--update-all"), "{}", "");
    }

    #[tokio::test]
    async fn missing_required_fields_are_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(&dir, "");
        let err = AstGrep::with_binary(&bin)
            .run(&ctx(&dir), "t", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
        let err = AstEdit::with_binary(&bin)
            .run(&ctx(&dir), "t", json!({"pattern": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn previews_and_searches_are_parallel_safe_applies_are_not() {
        let grep = AstGrep::new();
        let edit = AstEdit::new();
        assert!(grep.parallel_safe(&json!({"pattern": "x"})));
        assert!(grep.replay_safe());
        assert!(edit.parallel_safe(&json!({"pattern": "x", "rewrite": "y"})));
        assert!(!edit.parallel_safe(&json!({"pattern": "x", "rewrite": "y", "apply": true})));
        assert!(!edit.replay_safe());
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(shell_quote("fo'o"), "'fo'\\''o'");
    }

    /// End-to-end against a real binary, when one is reachable: honored in
    /// local runs with ast-grep installed (or `AST_GREP_BIN` set); skipped
    /// where it is absent, like CI.
    #[tokio::test]
    async fn real_binary_roundtrip_when_available() {
        let explicit = std::env::var("AST_GREP_BIN").ok().map(PathBuf::from);
        if find_binary(explicit.as_ref()).await.is_err() {
            eprintln!("skipping: no real ast-grep binary available");
            return;
        }
        let make = |dir: &tempfile::TempDir| {
            std::fs::write(
                dir.path().join("a.rs"),
                "fn main() {\n    let x = foo(1);\n}\n",
            )
            .unwrap();
        };
        let tool_pair = || match &explicit {
            Some(bin) => (AstGrep::with_binary(bin), AstEdit::with_binary(bin)),
            None => (AstGrep::new(), AstEdit::new()),
        };

        let dir = tempfile::tempdir().unwrap();
        make(&dir);
        let (grep, edit) = tool_pair();
        let out = grep
            .run(&ctx(&dir), "t", json!({"pattern": "foo($A)"}))
            .await
            .unwrap();
        assert!(out.contains("a.rs:2\tfoo(1)"), "{out}");

        let out = edit
            .run(
                &ctx(&dir),
                "t",
                json!({"pattern": "foo($A)", "rewrite": "bar($A)"}),
            )
            .await
            .unwrap();
        assert!(out.contains("preview only"), "{out}");
        // The preview wrote nothing.
        let content = std::fs::read_to_string(dir.path().join("a.rs")).unwrap();
        assert!(content.contains("foo(1)"), "{content}");

        let out = edit
            .run(
                &ctx(&dir),
                "t",
                json!({"pattern": "foo($A)", "rewrite": "bar($A)", "apply": true}),
            )
            .await
            .unwrap();
        assert!(out.contains("Applied"), "{out}");
        let content = std::fs::read_to_string(dir.path().join("a.rs")).unwrap();
        assert!(content.contains("bar(1)"), "{content}");
    }
}

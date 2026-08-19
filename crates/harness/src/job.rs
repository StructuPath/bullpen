//! The job tool: the coordination plane, exposed to the model.
//!
//! `bullpen agents` reads sessions from the store and never talks to the
//! processes running them; `job` gives the model the same read-and-signal
//! view over its own pen children. `list` derives each child's state from
//! its stored status plus process liveness — the store is the source of
//! truth, so a `job` call after a crash sees reality, not a stale
//! in-process handle. `wait` polls that truth to a terminal state and
//! returns the child's recorded answer. `cancel` signals a background
//! child dispatched by this process; the child finishes as failed and
//! stays resumable, because cancellation is an outcome, not an erasure.

use std::path::PathBuf;
use std::time::Duration;

use bullpen_llm::{Role, ToolSpec};
use bullpen_store::status::{AgentStatus, for_session, pid_alive};
use bullpen_store::{Session, Store, StoreError};
use bullpen_tools::{Tool, ToolCtx, ToolError};
use serde_json::{Value, json};

use crate::pen::Cancels;

const DEFAULT_WAIT_SECS: u64 = 900;
const MAX_WAIT_SECS: u64 = 3600;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Built by [`crate::PenTool::job_tool`], sharing the pen's cancellation
/// registry.
pub struct JobTool {
    store_path: PathBuf,
    session_id: String,
    cancels: Cancels,
}

impl JobTool {
    pub(crate) fn new(
        store_path: PathBuf,
        session_id: impl Into<String>,
        cancels: Cancels,
    ) -> Self {
        Self {
            store_path,
            session_id: session_id.into(),
            cancels,
        }
    }

    fn children(&self, store: &Store) -> Result<Vec<Session>, ToolError> {
        store
            .list_children(&self.session_id)
            .map_err(|e| ToolError::Failed(format!("store: {e}")))
    }

    /// Resolve a child by id prefix, mirroring session/todo resolution.
    fn resolve(&self, store: &Store, prefix: &str) -> Result<Session, ToolError> {
        let matches: Vec<Session> = self
            .children(store)?
            .into_iter()
            .filter(|s| s.id.starts_with(prefix))
            .collect();
        match matches.len() {
            0 => Err(ToolError::InvalidInput(format!(
                "no child of this session matches `{prefix}`"
            ))),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => Err(ToolError::InvalidInput(format!(
                "child id `{prefix}` is ambiguous — use a longer prefix from `list`"
            ))),
        }
    }
}

fn derived(session: &Session) -> AgentStatus {
    for_session(session, session.pid.is_some_and(pid_alive))
}

fn render(children: &[Session]) -> String {
    if children.is_empty() {
        return "No children dispatched in this session.".into();
    }
    let mut out = format!("Children ({}):\n", children.len());
    for child in children {
        let title = if child.title.is_empty() {
            "(no task recorded)"
        } else {
            &child.title
        };
        out.push_str(&format!(
            "  {} {:9} {title}{}\n",
            &child.id[..8],
            derived(child).label(),
            if child.worktree_path.is_some() {
                " · worktree"
            } else {
                ""
            }
        ));
    }
    out
}

/// The child's final report: the last assistant message on its path.
fn answer_of(store: &Store, child_id: &str) -> Result<String, StoreError> {
    Ok(store
        .path_messages(child_id)?
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text())
        .unwrap_or_default())
}

#[async_trait::async_trait]
impl Tool for JobTool {
    fn name(&self) -> &'static str {
        "job"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job".into(),
            description: format!(
                "Coordinate this session's child agents. `list` shows every \
                 child and its state (Working, Completed, Failed, Idle). \
                 `wait` blocks until one child finishes and returns its final \
                 report (default timeout {DEFAULT_WAIT_SECS}s, maximum \
                 {MAX_WAIT_SECS}s). `cancel` stops a background child \
                 dispatched in this process; its session is kept and \
                 resumable. Children are addressed by the id prefix `list` \
                 shows. Pair with `agent` + `background: true`: dispatch \
                 several, then wait on each."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "wait", "cancel"],
                        "description": "What to do"
                    },
                    "id": {
                        "type": "string",
                        "description": "For `wait`/`cancel`: the child's id (any unique prefix)"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "For `wait`: how long to block before giving up"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        // `list` and `wait` only read the store, so several waits can block
        // side by side — that is how a fan-out joins. `cancel` signals a
        // running child and stays serial.
        matches!(
            input.get("action").and_then(Value::as_str),
            Some("list") | Some("wait")
        )
    }

    async fn run(&self, _ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let action = input.get("action").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidInput("missing required string field `action`".into())
        })?;
        let store =
            Store::open(&self.store_path).map_err(|e| ToolError::Failed(format!("store: {e}")))?;
        let prefix = || {
            input
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput(format!("`{action}` needs an `id` prefix")))
        };

        match action {
            "list" => Ok(render(&self.children(&store)?)),
            "wait" => {
                let child = self.resolve(&store, prefix()?)?;
                let timeout = input
                    .get("timeout_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_WAIT_SECS)
                    .min(MAX_WAIT_SECS);
                let short = &child.id[..8];
                let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
                loop {
                    let current = store
                        .get_session(&child.id)
                        .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
                    match derived(&current) {
                        AgentStatus::Completed => {
                            let answer = answer_of(&store, &child.id)
                                .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
                            return Ok(format!("{answer}\n\n[child {short} · completed]"));
                        }
                        AgentStatus::Failed => {
                            return Err(ToolError::Failed(format!(
                                "child {short} failed or was interrupted; its \
                                 session is saved — `agent` with the same task \
                                 continues it"
                            )));
                        }
                        // Idle covers the dispatch race: created but not yet
                        // started. Keep polling; the deadline bounds it.
                        AgentStatus::Working | AgentStatus::Idle => {}
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(ToolError::Timeout(timeout));
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
            "cancel" => {
                let child = self.resolve(&store, prefix()?)?;
                let short = &child.id[..8];
                match self.cancels.lock().unwrap().remove(&child.id) {
                    Some(cancel) => {
                        // A dropped receiver means the child just finished on
                        // its own — that is a success for `cancel` too.
                        let _ = cancel.send(());
                        Ok(format!(
                            "cancel signalled for child {short}; its session is \
                             saved and resumable"
                        ))
                    }
                    None => match derived(&child) {
                        AgentStatus::Working => Err(ToolError::Failed(format!(
                            "child {short} is running in another process (pid \
                             {}); this session did not dispatch it, so cancel \
                             it there",
                            child.pid.unwrap_or(0)
                        ))),
                        _ => Err(ToolError::InvalidInput(format!(
                            "child {short} is not running ({})",
                            derived(&child).label()
                        ))),
                    },
                }
            }
            other => Err(ToolError::InvalidInput(format!(
                "unknown action `{other}` (expected list, wait, or cancel)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pen::PenTool;
    use crate::testutil::{FakeProvider, text_response};
    use crate::{PenConfig, prepare_session};

    fn setup(dir: &tempfile::TempDir) -> (PenTool, JobTool, String) {
        let store_path = dir.path().join("t.db");
        let parent = Store::open(&store_path)
            .unwrap()
            .create_session("/tmp", "fake", "m")
            .unwrap()
            .id;
        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("child answer")]),
            &parent,
            PenConfig::new(
                store_path,
                std::env::temp_dir(),
                "fake",
                "test-model",
                "base system",
            ),
        );
        let job = pen.job_tool();
        (pen, job, parent)
    }

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn empty_list_and_bad_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let (_pen, job, _) = setup(&dir);

        let out = job
            .run(&ctx(), "j", json!({"action": "list"}))
            .await
            .unwrap();
        assert_eq!(out, "No children dispatched in this session.");

        for input in [
            json!({}),
            json!({"action": "wait"}),
            json!({"action": "cancel", "id": "zzz"}),
            json!({"action": "nope"}),
        ] {
            let err = job.run(&ctx(), "j", input).await.unwrap_err();
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
        }
    }

    #[tokio::test]
    async fn list_shows_the_completed_child_and_wait_returns_its_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (pen, job, parent) = setup(&dir);
        pen.run(&ctx(), "call_1", json!({"prompt": "go"}))
            .await
            .unwrap();

        let listed = job
            .run(&ctx(), "j1", json!({"action": "list"}))
            .await
            .unwrap();
        let short = &crate::pen::child_session_id(&parent, "call_1")[..8];
        assert!(listed.contains(short), "{listed}");
        assert!(listed.contains("Completed"), "{listed}");

        // Waiting on an already-finished child returns immediately.
        let waited = job
            .run(&ctx(), "j2", json!({"action": "wait", "id": short}))
            .await
            .unwrap();
        assert!(waited.contains("child answer"), "{waited}");

        // A finished child cannot be cancelled.
        let err = job
            .run(&ctx(), "j3", json!({"action": "cancel", "id": short}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");
    }

    #[tokio::test]
    async fn a_crashed_child_reads_as_failed_not_working() {
        let dir = tempfile::tempdir().unwrap();
        let (_pen, job, parent) = setup(&dir);
        let child_id = crate::pen::child_session_id(&parent, "call_1");

        // Fabricate a child whose process died mid-run: status running, a
        // pid that cannot be alive.
        let mut store = Store::open(&dir.path().join("t.db")).unwrap();
        store
            .create_child_session(&child_id, &parent, "/tmp", "fake", "m")
            .unwrap();
        store.start_operation(&child_id, &json!({})).unwrap();
        store.start_worker(&child_id, i32::MAX as i64 - 1).unwrap();

        let listed = job
            .run(&ctx(), "j1", json!({"action": "list"}))
            .await
            .unwrap();
        assert!(listed.contains("Failed"), "{listed}");

        let err = job
            .run(
                &ctx(),
                "j2",
                json!({"action": "wait", "id": &child_id[..8], "timeout_seconds": 5}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed or was interrupted"),
            "{err}"
        );

        // And the promised continuation path actually recovers it.
        let (_, recovery) = prepare_session(&mut store, &child_id).unwrap();
        assert!(recovery.is_some());
    }

    #[test]
    fn reads_are_parallel_safe_cancel_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let (_pen, job, _) = setup(&dir);
        assert!(job.parallel_safe(&json!({"action": "list"})));
        assert!(job.parallel_safe(&json!({"action": "wait", "id": "a"})));
        assert!(!job.parallel_safe(&json!({"action": "cancel", "id": "a"})));
    }
}

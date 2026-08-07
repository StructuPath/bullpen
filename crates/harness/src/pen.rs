//! The pen: durable subagents.
//!
//! A child is an ordinary session in the same store, linked to its parent
//! and named **deterministically** from the invocation:
//! `child_id = uuidv5(parent_session_id, tool_call_id)`. That single choice
//! does most of the work:
//!
//! - a replayed spawn (same tool call) *reattaches* to the same child
//!   instead of spawning a twin — a completed child returns its recorded
//!   answer without touching the provider;
//! - a child whose process died mid-run is recovered by the same machinery
//!   as any session, then *continued* rather than restarted;
//! - children survive process exit and are inspectable with
//!   `bullpen sessions` / resumable with `bullpen run -r` like any session.
//!
//! Budgets are durable too: the child count is a database count, not an
//! in-process counter, so a crash-restart loop cannot reset it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bullpen_agent::{Agent, AgentConfig};
use bullpen_llm::{Provider, Role, ToolSpec};
use bullpen_store::Store;
use bullpen_tools::{Glob, Grep, ReadFile, Registry, Tool, ToolCtx, ToolError};
use serde_json::{Value, json};

use crate::{StoreJournal, prepare_session};

#[derive(Clone)]
pub struct PenConfig {
    pub store_path: PathBuf,
    pub workspace: PathBuf,
    /// Recorded on child session rows (matches the parent's provider).
    pub provider_name: String,
    pub model: String,
    /// Base system prompt; the pen appends the relief-agent role text.
    pub system: String,
    pub max_children: u64,
    pub child_timeout: Duration,
    pub child_max_turns: u32,
}

impl PenConfig {
    pub fn new(
        store_path: PathBuf,
        workspace: PathBuf,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        system: impl Into<String>,
    ) -> Self {
        Self {
            store_path,
            workspace,
            provider_name: provider_name.into(),
            model: model.into(),
            system: system.into(),
            max_children: 20,
            child_timeout: Duration::from_secs(900),
            child_max_turns: 200,
        }
    }
}

/// The `agent` tool: delegate a bounded task to a durable child agent.
pub struct PenTool {
    provider: Arc<dyn Provider>,
    parent_session: String,
    config: PenConfig,
}

impl PenTool {
    pub fn new(provider: Arc<dyn Provider>, parent_session: impl Into<String>, config: PenConfig) -> Self {
        Self {
            provider,
            parent_session: parent_session.into(),
            config,
        }
    }
}

/// Deterministic child identity: same parent + same tool call → same child.
pub fn child_session_id(parent_session: &str, tool_call_id: &str) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("bullpen:{parent_session}:{tool_call_id}").as_bytes(),
    )
    .to_string()
}

fn registry_for_mode(mode: &str) -> Result<Registry, ToolError> {
    match mode {
        // Read-only capability set; no bash, no writes, and never a nested pen.
        "inspect" => {
            let mut r = Registry::new();
            r.register(Arc::new(ReadFile));
            r.register(Arc::new(Grep));
            r.register(Arc::new(Glob));
            Ok(r)
        }
        // Full workspace tools — still no nested pen.
        "work" => Ok(Registry::standard()),
        other => Err(ToolError::InvalidInput(format!(
            "unknown mode `{other}` (expected \"inspect\" or \"work\")"
        ))),
    }
}

#[async_trait::async_trait]
impl Tool for PenTool {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent".into(),
            description: "Delegate a bounded task to a child agent with its own \
                          context window. Mode `inspect` (default) gives it \
                          read-only tools (read_file, grep, glob); mode `work` \
                          adds bash and file editing. The child runs to \
                          completion and returns its final report. Use for \
                          research across many files or self-contained subtasks."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Complete, self-contained task description — the child cannot see this conversation"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["inspect", "work"],
                        "description": "Capability set for the child (default: inspect)"
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn run(&self, _ctx: &ToolCtx, call_id: &str, input: Value) -> Result<String, ToolError> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing required string field `prompt`".into()))?;
        let mode = input.get("mode").and_then(Value::as_str).unwrap_or("inspect");
        let registry = registry_for_mode(mode)?;

        let child_id = child_session_id(&self.parent_session, call_id);
        let short = &child_id[..8];
        let mut store = Store::open(&self.config.store_path)
            .map_err(|e| ToolError::Failed(format!("store: {e}")))?;

        // Budget applies to *new* children only; reattaching to an existing
        // child (a replay) is always allowed.
        let exists = store.get_session(&child_id).is_ok();
        if !exists {
            let children = store
                .count_children(&self.parent_session)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
            if children >= self.config.max_children {
                return Err(ToolError::Failed(format!(
                    "child budget exhausted ({} of {} used for this session)",
                    children, self.config.max_children
                )));
            }
        }

        let child = store
            .create_child_session(
                &child_id,
                &self.parent_session,
                &self.config.workspace.display().to_string(),
                &self.config.provider_name,
                &self.config.model,
            )
            .map_err(|e| ToolError::Failed(format!("store: {e}")))?;

        // Reattach: the child already finished — return its recorded answer
        // without another provider call.
        if exists
            && store
                .last_run_outcome(&child_id)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?
                .as_deref()
                == Some("completed")
        {
            let messages = store
                .path_messages(&child_id)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
            let answer = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.text())
                .unwrap_or_default();
            return Ok(format!(
                "{answer}\n\n[child {short} · reattached to completed run]"
            ));
        }

        // Fresh or interrupted child: recover if needed, then run.
        let (transcript, recovery) = prepare_session(&mut store, &child_id)
            .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
        let task = if transcript.is_empty() {
            prompt.to_string()
        } else {
            "The previous attempt above was interrupted. Continue and complete the \
             original task, then give your final report."
                .to_string()
        };
        drop(store);

        let system = format!(
            "{}\n\nYou are a bullpen relief agent handling one delegated task. \
             Work it to completion and end with a final report — your last \
             message goes back to the coordinating agent, which cannot see \
             your intermediate steps.{}",
            self.config.system,
            if mode == "inspect" {
                " You have read-only tools."
            } else {
                ""
            }
        );

        let journal = StoreJournal::new(
            Store::open(&self.config.store_path)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?,
            &child_id,
        );
        let mut agent = Agent::new(
            self.provider.clone(),
            registry,
            ToolCtx {
                workspace: self.config.workspace.clone(),
            },
            AgentConfig {
                model: self.config.model.clone(),
                system,
                max_turns: self.config.child_max_turns,
                ..Default::default()
            },
        )
        .with_transcript(transcript, child.usage)
        .with_journal(Box::new(journal));

        let result = tokio::time::timeout(self.config.child_timeout, agent.send(&task)).await;
        let usage = agent.usage();
        match result {
            Ok(Ok(answer)) => Ok(format!(
                "{answer}\n\n[child {short} · mode {mode} · {} in / {} out tokens{}]",
                usage.input_tokens,
                usage.output_tokens,
                if recovery.is_some() { " · recovered" } else { "" },
            )),
            Ok(Err(e)) => Err(ToolError::Failed(format!(
                "child {short} failed: {e} (session is saved and resumable)"
            ))),
            Err(_) => Err(ToolError::Failed(format!(
                "child {short} timed out after {}s; its session is saved and \
                 recoverable — calling agent again with the same task will continue it",
                self.config.child_timeout.as_secs()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeProvider, response, text_response};
    use bullpen_llm::{ContentBlock, StopReason};

    fn config(dir: &tempfile::TempDir) -> PenConfig {
        PenConfig::new(
            dir.path().join("t.db"),
            std::env::temp_dir(),
            "fake",
            "test-model",
            "base system",
        )
    }

    fn parent(dir: &tempfile::TempDir) -> String {
        Store::open(&dir.path().join("t.db"))
            .unwrap()
            .create_session("/tmp", "fake", "test-model")
            .unwrap()
            .id
    }

    fn tool_ctx() -> ToolCtx {
        ToolCtx {
            workspace: std::env::temp_dir(),
        }
    }

    #[test]
    fn child_ids_are_deterministic() {
        let a = child_session_id("parent-1", "call-1");
        assert_eq!(a, child_session_id("parent-1", "call-1"));
        assert_ne!(a, child_session_id("parent-1", "call-2"));
        assert_ne!(a, child_session_id("parent-2", "call-1"));
    }

    #[tokio::test]
    async fn child_runs_and_is_durably_linked() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("child answer")]),
            &parent,
            config(&dir),
        );

        let out = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "look into it"}))
            .await
            .unwrap();
        assert!(out.contains("child answer"), "{out}");

        let store = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(store.count_children(&parent).unwrap(), 1);
        let child = store.get_session(&child_session_id(&parent, "call_1")).unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.as_str()));
        assert_eq!(store.last_run_outcome(&child.id).unwrap().as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn replay_reattaches_without_provider_calls() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("first answer")]),
            &parent,
            config(&dir),
        );
        pen.run(&tool_ctx(), "call_1", json!({"prompt": "task"}))
            .await
            .unwrap();

        // Empty script: any provider call would fail the run. Reattach must
        // return the recorded answer instead.
        let pen = PenTool::new(FakeProvider::new(vec![]), &parent, config(&dir));
        let out = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "task"}))
            .await
            .unwrap();
        assert!(out.contains("first answer"), "{out}");
        assert!(out.contains("reattached"), "{out}");
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(store.count_children(&parent).unwrap(), 1);
    }

    #[tokio::test]
    async fn child_budget_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let mut cfg = config(&dir);
        cfg.max_children = 1;

        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("one")]),
            &parent,
            cfg.clone(),
        );
        pen.run(&tool_ctx(), "call_1", json!({"prompt": "a"}))
            .await
            .unwrap();

        // A NEW pen instance (fresh process) still sees the budget spent.
        let pen = PenTool::new(FakeProvider::new(vec![text_response("two")]), &parent, cfg);
        let err = pen
            .run(&tool_ctx(), "call_2", json!({"prompt": "b"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("budget exhausted"), "{err}");
    }

    #[tokio::test]
    async fn interrupted_child_is_recovered_and_continued() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let child_id = child_session_id(&parent, "call_1");

        // Fabricate the durable state of a child that crashed mid-run.
        {
            let mut store = Store::open(&dir.path().join("t.db")).unwrap();
            store
                .create_child_session(&child_id, &parent, "/tmp", "fake", "test-model")
                .unwrap();
            store.start_operation(&child_id, &json!({})).unwrap();
            store
                .append_entry(
                    &child_id,
                    "e-user",
                    "message",
                    &serde_json::to_value(bullpen_llm::Message::user_text("original task")).unwrap(),
                )
                .unwrap();
        }

        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("finished it")]),
            &parent,
            config(&dir),
        );
        let out = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "original task"}))
            .await
            .unwrap();
        assert!(out.contains("finished it"), "{out}");
        assert!(out.contains("recovered"), "{out}");

        // The child's transcript shows the whole story: original attempt,
        // recovery closure, continuation.
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let messages = store.path_messages(&child_id).unwrap();
        assert!(messages.iter().any(|m| m.text().contains("interrupted")));
        assert_eq!(store.last_run_outcome(&child_id).unwrap().as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn inspect_mode_has_no_bash() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![
                response(
                    vec![ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "rm -rf /"}),
                    }],
                    StopReason::ToolUse,
                ),
                text_response("could not run that"),
            ]),
            &parent,
            config(&dir),
        );

        let out = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "task", "mode": "inspect"}))
            .await
            .unwrap();
        assert!(out.contains("could not run that"), "{out}");

        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let child_id = child_session_id(&parent, "call_1");
        let messages = store.path_messages(&child_id).unwrap();
        // The bash attempt got an unknown-tool error result, not execution.
        assert!(
            messages.iter().any(|m| m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("unknown tool")))),
        );
    }

    #[tokio::test]
    async fn bad_mode_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(FakeProvider::new(vec![]), &parent, config(&dir));
        let err = pen
            .run(&tool_ctx(), "c", json!({"prompt": "x", "mode": "yolo"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}

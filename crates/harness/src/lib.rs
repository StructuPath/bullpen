//! Durable execution: the composition of the agent loop and the store.
//!
//! [`StoreJournal`] implements the loop's [`Journal`] durability protocol
//! against SQLite, following the durability rule from ARCHITECTURE.md:
//! intent records before effects, results at provisioned ids, everything
//! idempotent. [`prepare_session`] is the front door: recover if a previous
//! process died mid-run, then hand back the rebuilt transcript.
//!
//! This crate is the future home of the pen (durable subagents).

pub mod pen;
#[cfg(test)]
pub(crate) mod testutil;

pub use pen::{PenConfig, PenTool};

use bullpen_agent::{Journal, JournalError, RunOutcome, ToolIntent};
use bullpen_llm::{Message, Usage};
use bullpen_store::{Recovery, Store, StoreError, recover, title_from};
use serde_json::json;

/// Recover a session if it has a crashed run, then return the transcript
/// and cumulative usage to seed the agent with.
pub fn prepare_session(
    store: &mut Store,
    session_id: &str,
) -> Result<(Vec<Message>, Option<Recovery>), StoreError> {
    let recovery = recover(store, session_id)?;
    let messages = store.path_messages(session_id)?;
    Ok((messages, recovery))
}

/// SQLite-backed [`Journal`]. Owns the store for the duration of a run.
pub struct StoreJournal {
    store: std::sync::Mutex<Store>,
    session_id: String,
    run_id: Option<String>,
    /// Provisioned id for the current tool batch's grouped results entry,
    /// allocated at intent time so recovery can pair intents to it.
    pending_results_entry: Option<String>,
}

impl StoreJournal {
    pub fn new(store: Store, session_id: impl Into<String>) -> Self {
        Self {
            store: std::sync::Mutex::new(store),
            session_id: session_id.into(),
            run_id: None,
            pending_results_entry: None,
        }
    }

    /// `&mut self` everywhere means the mutex is never contended.
    fn store(&mut self) -> &mut Store {
        self.store.get_mut().expect("journal mutex poisoned")
    }

    fn run_id(&self) -> Result<&str, JournalError> {
        self.run_id
            .as_deref()
            .ok_or_else(|| JournalError("journal used before run_started".into()))
    }
}

fn jerr(e: StoreError) -> JournalError {
    JournalError(e.to_string())
}

#[async_trait::async_trait]
impl Journal for StoreJournal {
    async fn run_started(&mut self, user: &Message) -> Result<(), JournalError> {
        let sid = self.session_id.clone();
        let run_id = self
            .store()
            .start_operation(&sid, &json!({}))
            .map_err(jerr)?;
        self.store()
            .append_entry(
                &sid,
                &uuid::Uuid::new_v4().to_string(),
                "message",
                &serde_json::to_value(user).map_err(|e| JournalError(e.to_string()))?,
            )
            .map_err(jerr)?;
        self.run_id = Some(run_id);
        Ok(())
    }

    async fn step_attempt(&mut self, attempt: u32) -> Result<(), JournalError> {
        let sid = self.session_id.clone();
        let run_id = self.run_id()?.to_string();
        self.store()
            .append_record(
                &sid,
                &format!("{run_id}:step:{attempt}"),
                &run_id,
                "step_attempt",
                &json!({ "attempt": attempt }),
            )
            .map_err(jerr)
    }

    async fn assistant_message(&mut self, message: &Message) -> Result<(), JournalError> {
        let sid = self.session_id.clone();
        self.run_id()?;
        self.store()
            .append_entry(
                &sid,
                &uuid::Uuid::new_v4().to_string(),
                "message",
                &serde_json::to_value(message).map_err(|e| JournalError(e.to_string()))?,
            )
            .map_err(jerr)
    }

    async fn tool_batch(&mut self, intents: &[ToolIntent]) -> Result<(), JournalError> {
        let sid = self.session_id.clone();
        let run_id = self.run_id()?.to_string();
        let results_entry_id = uuid::Uuid::new_v4().to_string();
        for intent in intents {
            // Deterministic intent id: a retried batch write stays idempotent.
            self.store()
                .append_record(
                    &sid,
                    &format!("{run_id}:tool:{}", intent.tool_use_id),
                    &run_id,
                    "tool_started",
                    &json!({
                        "tool_use_id": intent.tool_use_id,
                        "name": intent.name,
                        "input": intent.input,
                        "replay_safe": intent.replay_safe,
                        "results_entry_id": results_entry_id,
                    }),
                )
                .map_err(jerr)?;
        }
        self.pending_results_entry = Some(results_entry_id);
        Ok(())
    }

    async fn tool_results(&mut self, results: &Message) -> Result<(), JournalError> {
        let sid = self.session_id.clone();
        let entry_id = self
            .pending_results_entry
            .take()
            .ok_or_else(|| JournalError("tool_results without a pending tool_batch".into()))?;
        self.store()
            .append_entry(
                &sid,
                &entry_id,
                "message",
                &serde_json::to_value(results).map_err(|e| JournalError(e.to_string()))?,
            )
            .map_err(jerr)
    }

    async fn run_finished(&mut self, outcome: RunOutcome, usage: Usage) -> Result<(), JournalError> {
        let sid = self.session_id.clone();
        let run_id = self.run_id()?.to_string();
        self.store()
            .finish_operation(&sid, &run_id, outcome.as_str())
            .map_err(jerr)?;
        let path = self.store().path_messages(&sid).map_err(jerr)?;
        let title = title_from(&path);
        self.store()
            .update_session_meta(&sid, usage, &title)
            .map_err(jerr)?;
        self.run_id = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bullpen_agent::{Agent, AgentConfig};
    use bullpen_llm::{
        ContentBlock, Provider, ProviderError, Request, Response, Role, StopReason, ToolSpec,
    };
    use bullpen_tools::{Registry, Tool, ToolCtx};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    struct FakeProvider {
        script: Mutex<Vec<Response>>,
    }

    impl FakeProvider {
        fn new(mut responses: Vec<Response>) -> Arc<Self> {
            responses.reverse();
            Arc::new(Self {
                script: Mutex::new(responses),
            })
        }
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }
        async fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
            self.script
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| ProviderError::Malformed("script exhausted".into()))
        }
    }

    struct Echo;

    #[async_trait::async_trait]
    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn run(&self, _: &ToolCtx, _: &str, input: Value) -> Result<String, bullpen_tools::ToolError> {
            Ok(format!("echo: {}", input["value"]))
        }
    }

    fn response(blocks: Vec<ContentBlock>, stop: StopReason) -> Response {
        Response {
            content: blocks,
            stop_reason: stop,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }
    }

    fn agent_with(store: Store, session_id: &str, provider: Arc<FakeProvider>) -> Agent {
        let mut registry = Registry::new();
        registry.register(Arc::new(Echo));
        Agent::new(
            provider,
            registry,
            ToolCtx {
                workspace: std::env::temp_dir(),
            },
            AgentConfig {
                model: "test".into(),
                system: "test".into(),
                ..Default::default()
            },
        )
        .with_journal(Box::new(StoreJournal::new(store, session_id)))
    }

    fn open_store(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("t.db")).unwrap()
    }

    #[tokio::test]
    async fn full_run_is_durable_and_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir);
        let session = store.create_session("/tmp", "fake", "test").unwrap();

        let provider = FakeProvider::new(vec![
            response(
                vec![ContentBlock::ToolUse {
                    id: "tu_1".into(),
                    name: "echo".into(),
                    input: json!({"value": 42}),
                }],
                StopReason::ToolUse,
            ),
            response(
                vec![ContentBlock::Text { text: "done".into() }],
                StopReason::EndTurn,
            ),
        ]);
        let mut agent = agent_with(open_store(&dir), &session.id, provider);
        let answer = agent.send("go").await.unwrap();
        assert_eq!(answer, "done");

        // The durable transcript equals the in-memory one, the operation is
        // closed, and usage rolled up.
        let (messages, recovery) = prepare_session(&mut open_store(&dir), &session.id).unwrap();
        assert!(recovery.is_none());
        assert_eq!(messages, agent.messages());
        let store = open_store(&dir);
        assert!(store.open_run(&session.id).unwrap().is_none());
        let session = store.get_session(&session.id).unwrap();
        assert_eq!(session.usage.output_tokens, 10); // two steps × 5
        assert_eq!(session.title, "go");

        // A follow-up run seeded from the durable transcript works.
        let provider = FakeProvider::new(vec![response(
            vec![ContentBlock::Text { text: "again".into() }],
            StopReason::EndTurn,
        )]);
        let (messages, _) = prepare_session(&mut open_store(&dir), &session.id).unwrap();
        let mut agent = agent_with(open_store(&dir), &session.id, provider);
        agent = agent.with_transcript(messages, session.usage);
        assert_eq!(agent.send("more").await.unwrap(), "again");
        assert_eq!(open_store(&dir).path_messages(&session.id).unwrap().len(), 6);
    }

    #[tokio::test]
    async fn provider_failure_leaves_recoverable_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir);
        let session = store.create_session("/tmp", "fake", "test").unwrap();

        // Script exhausts immediately → provider error mid-run.
        let provider = FakeProvider::new(vec![]);
        let mut agent = agent_with(open_store(&dir), &session.id, provider);
        assert!(agent.send("go").await.is_err());

        // run_finished(Failed) closed the operation; the user message is
        // durable; recovery finds nothing left to do but the transcript
        // never ends on a dangling user message after preparation.
        let mut store = open_store(&dir);
        assert!(store.open_run(&session.id).unwrap().is_none());
        let (messages, recovery) = prepare_session(&mut store, &session.id).unwrap();
        assert!(recovery.is_none());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
    }

    #[tokio::test]
    async fn simulated_crash_mid_batch_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir);
        let session = store.create_session("/tmp", "fake", "test").unwrap();

        // A journal that "crashes the process" after intents are durable but
        // before results land: it errors on tool_results, and we treat the
        // run as killed (run_finished never gets to write — simulate by a
        // journal that also fails there).
        struct CrashAfterIntents {
            inner: StoreJournal,
        }
        #[async_trait::async_trait]
        impl Journal for CrashAfterIntents {
            async fn run_started(&mut self, m: &Message) -> Result<(), JournalError> {
                self.inner.run_started(m).await
            }
            async fn step_attempt(&mut self, n: u32) -> Result<(), JournalError> {
                self.inner.step_attempt(n).await
            }
            async fn assistant_message(&mut self, m: &Message) -> Result<(), JournalError> {
                self.inner.assistant_message(m).await
            }
            async fn tool_batch(&mut self, i: &[ToolIntent]) -> Result<(), JournalError> {
                self.inner.tool_batch(i).await
            }
            async fn tool_results(&mut self, _: &Message) -> Result<(), JournalError> {
                Err(JournalError("simulated crash".into()))
            }
            async fn run_finished(&mut self, _: RunOutcome, _: Usage) -> Result<(), JournalError> {
                Err(JournalError("simulated crash".into()))
            }
        }

        let provider = FakeProvider::new(vec![response(
            vec![ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "echo".into(),
                input: json!({"value": 1}),
            }],
            StopReason::ToolUse,
        )]);
        let mut registry = Registry::new();
        registry.register(Arc::new(Echo));
        let mut agent = Agent::new(
            provider,
            registry,
            ToolCtx {
                workspace: std::env::temp_dir(),
            },
            AgentConfig {
                model: "test".into(),
                system: "test".into(),
                ..Default::default()
            },
        )
        .with_journal(Box::new(CrashAfterIntents {
            inner: StoreJournal::new(open_store(&dir), &session.id),
        }));
        assert!(agent.send("go").await.is_err());

        // The operation is still open — exactly what a killed process leaves.
        let mut store = open_store(&dir);
        assert!(store.open_run(&session.id).unwrap().is_some());

        // Preparation recovers: synthetic interrupted result paired to the
        // intent, closing assistant note, operation closed.
        let (messages, recovery) = prepare_session(&mut store, &session.id).unwrap();
        let recovery = recovery.expect("recovery ran");
        assert_eq!(recovery.interrupted_tools, 1);
        let ContentBlock::ToolResult { tool_use_id, is_error, .. } = &messages[2].content[0]
        else {
            panic!("expected synthetic result, got {:?}", messages[2]);
        };
        assert_eq!(tool_use_id, "tu_1");
        assert!(is_error);
        assert_eq!(messages.last().unwrap().role, Role::Assistant);
        assert!(store.open_run(&session.id).unwrap().is_none());
    }
}

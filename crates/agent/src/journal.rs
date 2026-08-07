//! The durability protocol.
//!
//! The loop reports every step of a run through this trait *in intent-order*:
//! tool intents are journaled before tools execute, results after. The loop
//! knows the protocol; it does not know the storage — `NullJournal` runs
//! everything in memory, `bullpen-harness` provides the SQLite-backed
//! implementation. A journal failure fails the run: durability is not
//! best-effort.

use bullpen_llm::{Message, Usage};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("journal error: {0}")]
pub struct JournalError(pub String);

/// One tool call the loop is about to execute, with the replay-safety
/// declaration snapshotted at intent time.
#[derive(Debug, Clone)]
pub struct ToolIntent {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
    pub replay_safe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    /// Provider stopped at max_tokens; partial output preserved.
    Truncated,
    Failed,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Truncated => "truncated",
            Self::Failed => "failed",
        }
    }
}

// `Send + Sync` because agent futures hold `&Agent` across awaits inside
// Send futures. Implementations over non-Sync resources (SQLite) wrap them
// in a Mutex; with `&mut self` methods, `get_mut()` makes that lock-free.
#[async_trait::async_trait]
pub trait Journal: Send + Sync {
    /// A run was accepted: record the operation and the user message.
    async fn run_started(&mut self, user: &Message) -> Result<(), JournalError>;
    /// About to make provider request number `attempt` (1-based).
    async fn step_attempt(&mut self, attempt: u32) -> Result<(), JournalError>;
    /// The provider produced an assistant message; make it durable.
    async fn assistant_message(&mut self, message: &Message) -> Result<(), JournalError>;
    /// About to execute these tools. MUST be durable before any effect runs.
    async fn tool_batch(&mut self, intents: &[ToolIntent]) -> Result<(), JournalError>;
    /// The batch finished; `results` is the grouped tool-results message.
    async fn tool_results(&mut self, results: &Message) -> Result<(), JournalError>;
    /// The run ended. Called exactly once on every path, including failures.
    async fn run_finished(&mut self, outcome: RunOutcome, usage: Usage)
    -> Result<(), JournalError>;
}

/// In-memory execution: every hook is a no-op.
pub struct NullJournal;

#[async_trait::async_trait]
impl Journal for NullJournal {
    async fn run_started(&mut self, _: &Message) -> Result<(), JournalError> {
        Ok(())
    }
    async fn step_attempt(&mut self, _: u32) -> Result<(), JournalError> {
        Ok(())
    }
    async fn assistant_message(&mut self, _: &Message) -> Result<(), JournalError> {
        Ok(())
    }
    async fn tool_batch(&mut self, _: &[ToolIntent]) -> Result<(), JournalError> {
        Ok(())
    }
    async fn tool_results(&mut self, _: &Message) -> Result<(), JournalError> {
        Ok(())
    }
    async fn run_finished(&mut self, _: RunOutcome, _: Usage) -> Result<(), JournalError> {
        Ok(())
    }
}

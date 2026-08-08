//! Provider-neutral conversation protocol.
//!
//! This crate is the boundary between bullpen and model vendors. It defines
//! the shared message, tool, and usage types plus the [`Provider`] trait.
//! Adapters translate these into vendor wire formats at the edge; nothing
//! above this crate knows what vendor is in use.

pub mod anthropic;
pub mod chatcompletions;
pub mod codex;
mod retry;
mod sse;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// One block of message content. The transcript is a sequence of messages,
/// each holding ordered blocks. Tool calls and their results are first-class
/// blocks so the pairing invariant can be checked structurally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Provider-specific data that must be replayed verbatim on later turns
    /// (e.g. Codex encrypted reasoning items). Tagged with the producing
    /// provider so each adapter replays only its own blocks; everything else
    /// skips them, which is what makes cross-provider session resume safe.
    Opaque {
        provider: String,
        data: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        debug_assert!(
            results
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
        );
        Self {
            role: Role::User,
            content: results,
        }
    }

    /// Concatenated text blocks, ignoring tool traffic.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl Response {
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("api error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error("malformed response: {0}")]
    Malformed(String),
    #[error("{0}")]
    Failure(String),
}

/// Incremental assistant-text output. Providers send text fragments as they
/// arrive; tool calls are still assembled and returned in the final response.
/// A gone receiver never stops generation.
pub type TextSink = tokio::sync::mpsc::UnboundedSender<String>;

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// Buffered completion: the whole response arrives at once.
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError>;

    /// Streaming completion: assistant text is sent to `deltas` as it is
    /// generated, and the fully assembled response is returned at the end.
    /// The default buffers via [`Provider::complete`] and emits the final
    /// text as one delta, so every provider streams *something* even before
    /// it implements true streaming.
    async fn complete_streaming(
        &self,
        req: &Request,
        deltas: &TextSink,
    ) -> Result<Response, ProviderError> {
        let response = self.complete(req).await?;
        for block in &response.content {
            if let ContentBlock::Text { text } = block {
                let _ = deltas.send(text.clone());
            }
        }
        Ok(response)
    }
}

//! Anthropic Messages API adapter.
//!
//! Translates the shared conversation types into Anthropic wire format,
//! normalizes stop reasons and usage, and applies the shared retry policy.
//! Wire conversion is kept in pure functions so it can be tested without a
//! network.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::retry;
use crate::{
    ContentBlock, Provider, ProviderError, Request, Response, StopReason, ToolSpec, Usage,
};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
pub const DEFAULT_MODEL: &str = "claude-sonnet-5";

pub struct Anthropic {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
            api_key: api_key.into(),
            base_url: API_URL.to_string(),
        }
    }

    /// Override the endpoint; used by tests against a local server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[async_trait::async_trait]
impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = to_wire(req);
        let mut attempt = 0u32;
        loop {
            let result = self
                .client
                .post(&self.base_url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body)
                .send()
                .await;

            let retry_after = |resp: &reqwest::Response| {
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs)
            };

            match result {
                Ok(resp) if resp.status().is_success() => {
                    let wire: WireResponse = resp
                        .json()
                        .await
                        .map_err(|e| ProviderError::Malformed(e.to_string()))?;
                    return from_wire(wire);
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let after = retry_after(&resp);
                    if retry::retryable_status(status) && attempt + 1 < retry::MAX_ATTEMPTS {
                        let wait = retry::delay(attempt, after);
                        tracing::warn!(status, attempt, ?wait, "retrying provider call");
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    let message = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::Api { status, message });
                }
                Err(e) => {
                    let transient = e.is_timeout() || e.is_connect() || e.is_request();
                    if transient && attempt + 1 < retry::MAX_ATTEMPTS {
                        let wait = retry::delay(attempt, None);
                        tracing::warn!(error = %e, attempt, ?wait, "retrying after transport error");
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(ProviderError::Transport(e));
                }
            }
        }
    }
}

// ── Wire types ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "str::is_empty")]
    system: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
}

#[derive(Serialize, Deserialize)]
struct WireMessage {
    role: String,
    content: Vec<WireBlock>,
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock {
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
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// Blocks this adapter does not model (e.g. server tool results) are
    /// preserved for deserialization but dropped from the shared response.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct WireResponse {
    content: Vec<WireBlock>,
    stop_reason: Option<String>,
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
}

fn to_wire(req: &Request) -> Value {
    let messages = req
        .messages
        .iter()
        .map(|m| WireMessage {
            role: match m.role {
                crate::Role::User => "user".to_string(),
                crate::Role::Assistant => "assistant".to_string(),
            },
            content: m.content.iter().map(block_to_wire).collect(),
        })
        .collect();

    let wire = WireRequest {
        model: &req.model,
        max_tokens: req.max_tokens,
        system: &req.system,
        messages,
        tools: req.tools.iter().map(tool_to_wire).collect(),
    };
    serde_json::to_value(&wire).expect("wire request serializes")
}

fn block_to_wire(block: &ContentBlock) -> WireBlock {
    match block {
        ContentBlock::Text { text } => WireBlock::Text { text: text.clone() },
        ContentBlock::ToolUse { id, name, input } => WireBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => WireBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        },
    }
}

fn tool_to_wire(spec: &ToolSpec) -> WireTool<'_> {
    WireTool {
        name: &spec.name,
        description: &spec.description,
        input_schema: &spec.input_schema,
    }
}

fn from_wire(wire: WireResponse) -> Result<Response, ProviderError> {
    let content = wire
        .content
        .into_iter()
        .filter_map(|b| match b {
            WireBlock::Text { text } => Some(ContentBlock::Text { text }),
            WireBlock::ToolUse { id, name, input } => {
                Some(ContentBlock::ToolUse { id, name, input })
            }
            WireBlock::ToolResult { .. } | WireBlock::Unknown => None,
        })
        .collect();

    let stop_reason = match wire.stop_reason.as_deref() {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None => {
            return Err(ProviderError::Malformed(
                "response missing stop_reason".into(),
            ));
        }
    };

    Ok(Response {
        content,
        stop_reason,
        usage: Usage {
            input_tokens: wire.usage.input_tokens,
            output_tokens: wire.usage.output_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Role};
    use serde_json::json;

    fn sample_request() -> Request {
        Request {
            model: "claude-sonnet-5".into(),
            system: "be terse".into(),
            messages: vec![
                Message::user_text("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text { text: "checking".into() },
                        ContentBlock::ToolUse {
                            id: "tu_1".into(),
                            name: "bash".into(),
                            input: json!({"command": "ls"}),
                        },
                    ],
                },
                Message::tool_results(vec![ContentBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "README.md".into(),
                    is_error: false,
                }]),
            ],
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: "run a command".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 512,
        }
    }

    #[test]
    fn request_wire_shape() {
        let wire = to_wire(&sample_request());
        assert_eq!(wire["model"], "claude-sonnet-5");
        assert_eq!(wire["system"], "be terse");
        assert_eq!(wire["messages"][0]["role"], "user");
        assert_eq!(wire["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(wire["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(wire["messages"][2]["content"][0]["tool_use_id"], "tu_1");
        // is_error: false is elided from the wire
        assert!(wire["messages"][2]["content"][0].get("is_error").is_none());
        assert_eq!(wire["tools"][0]["name"], "bash");
    }

    #[test]
    fn response_parses_tool_use() {
        let wire: WireResponse = serde_json::from_value(json!({
            "content": [
                {"type": "text", "text": "running"},
                {"type": "tool_use", "id": "tu_9", "name": "bash", "input": {"command": "pwd"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        }))
        .unwrap();
        let resp = from_wire(wire).unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.usage.output_tokens, 20);
    }

    #[test]
    fn response_unknown_blocks_dropped() {
        let wire: WireResponse = serde_json::from_value(json!({
            "content": [
                {"type": "server_tool_use", "id": "x", "name": "web_search", "input": {}},
                {"type": "text", "text": "done"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }))
        .unwrap();
        let resp = from_wire(wire).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn response_missing_stop_reason_is_malformed() {
        let wire: WireResponse = serde_json::from_value(json!({
            "content": [],
            "stop_reason": null,
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }))
        .unwrap();
        assert!(from_wire(wire).is_err());
    }
}

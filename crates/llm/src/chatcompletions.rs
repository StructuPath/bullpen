//! OpenAI-compatible Chat Completions adapter.
//!
//! Used for OpenRouter today; any chat-completions-compatible host works by
//! pointing `base_url` elsewhere. Wire conversion is pure and tested without
//! a network.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::retry;
use crate::{
    ContentBlock, Message, Provider, ProviderError, Request, Response, Role, StopReason, Usage,
};

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// OpenRouter's auto-router: picks a model per request. Override with -m.
pub const OPENROUTER_DEFAULT_MODEL: &str = "openrouter/auto";

pub struct ChatCompletions {
    client: reqwest::Client,
    name: String,
    base_url: String,
    api_key: String,
}

impl ChatCompletions {
    pub fn openrouter(api_key: impl Into<String>) -> Self {
        Self::new("openrouter", OPENROUTER_BASE_URL, api_key)
    }

    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
            name: name.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for ChatCompletions {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = to_wire(req);
        let url = format!("{}/chat/completions", self.base_url);
        let mut attempt = 0u32;
        loop {
            let result = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                // OpenRouter app attribution headers; harmless elsewhere.
                .header("HTTP-Referer", "https://github.com/StructuPath/bullpen")
                .header("X-Title", "bullpen")
                .json(&body)
                .send()
                .await;

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
                    let after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(Duration::from_secs);
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
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String, // "function"
    function: WireFunction,
}

#[derive(Serialize, Deserialize)]
struct WireFunction {
    name: String,
    /// JSON-encoded string, per the chat-completions contract.
    arguments: String,
}

#[derive(Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

fn to_wire(req: &Request) -> Value {
    let mut messages: Vec<WireMessage> = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        messages.push(WireMessage {
            role: "system",
            content: Some(req.system.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }

    for m in &req.messages {
        match m.role {
            Role::Assistant => {
                let mut text = Vec::new();
                let mut tool_calls = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text: t } => text.push(t.as_str()),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(WireToolCall {
                                id: id.clone(),
                                kind: "function".into(),
                                function: WireFunction {
                                    name: name.clone(),
                                    arguments: input.to_string(),
                                },
                            })
                        }
                        ContentBlock::ToolResult { .. } | ContentBlock::Opaque { .. } => {}
                    }
                }
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                messages.push(WireMessage {
                    role: "assistant",
                    content: if text.is_empty() {
                        None
                    } else {
                        Some(text.join("\n"))
                    },
                    tool_calls,
                    tool_call_id: None,
                });
            }
            Role::User => {
                // Tool results become role:"tool" messages, which must directly
                // follow the assistant tool_calls message — emit them first.
                let mut text = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => messages.push(WireMessage {
                            role: "tool",
                            content: Some(if *is_error {
                                format!("ERROR: {content}")
                            } else {
                                content.clone()
                            }),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(tool_use_id.clone()),
                        }),
                        ContentBlock::Text { text: t } => text.push(t.as_str()),
                        ContentBlock::ToolUse { .. } | ContentBlock::Opaque { .. } => {}
                    }
                }
                if !text.is_empty() {
                    messages.push(WireMessage {
                        role: "user",
                        content: Some(text.join("\n")),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    });
                }
            }
        }
    }

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_tokens,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

fn from_wire(wire: WireResponse) -> Result<Response, ProviderError> {
    let choice = wire
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::Malformed("response has no choices".into()))?;

    let mut content = Vec::new();
    if let Some(text) = choice.message.content
        && !text.is_empty()
    {
        content.push(ContentBlock::Text { text });
    }
    let saw_tool_calls = !choice.message.tool_calls.is_empty();
    for call in choice.message.tool_calls {
        let input: Value = if call.function.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&call.function.arguments).map_err(|e| {
                ProviderError::Malformed(format!(
                    "tool arguments for {} are not JSON: {e}",
                    call.function.name
                ))
            })?
        };
        content.push(ContentBlock::ToolUse {
            id: call.id,
            name: call.function.name,
            input,
        });
    }

    let stop_reason = if saw_tool_calls {
        StopReason::ToolUse
    } else {
        match choice.finish_reason.as_deref() {
            Some("stop") | None => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            Some("tool_calls") => StopReason::ToolUse,
            Some(other) => StopReason::Other(other.to_string()),
        }
    };

    Ok(Response {
        content,
        stop_reason,
        usage: wire
            .usage
            .map(|u| Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            })
            .unwrap_or_default(),
    })
}

/// Suppress unused warnings for Message import used only in tests on some paths.
#[allow(unused)]
fn _t(_: &Message) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolSpec;

    fn sample_request() -> Request {
        Request {
            model: "openrouter/auto".into(),
            system: "be terse".into(),
            messages: vec![
                Message::user_text("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "checking".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "bash".into(),
                            input: json!({"command": "ls"}),
                        },
                        ContentBlock::Opaque {
                            provider: "codex".into(),
                            data: json!({"type": "reasoning"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call_1".into(),
                            content: "README.md".into(),
                            is_error: false,
                        },
                        ContentBlock::Text {
                            text: "now continue".into(),
                        },
                    ],
                },
            ],
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: "run".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 512,
        }
    }

    #[test]
    fn request_wire_shape() {
        let wire = to_wire(&sample_request());
        let msgs = wire["messages"].as_array().unwrap();
        // system, user, assistant(+tool_calls), tool, user
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        // opaque block from another provider is not serialized anywhere
        assert!(!wire.to_string().contains("reasoning"));
        // tool result precedes the follow-up user text and carries the call id
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "now continue");
        assert_eq!(wire["tools"][0]["function"]["name"], "bash");
    }

    #[test]
    fn response_with_tool_calls() {
        let wire: WireResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "bash", "arguments": "{\"command\":\"pwd\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3}
        }))
        .unwrap();
        let resp = from_wire(wire).unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        let ContentBlock::ToolUse { id, name, input } = &resp.content[0] else {
            panic!("expected tool use");
        };
        assert_eq!(id, "call_9");
        assert_eq!(name, "bash");
        assert_eq!(input["command"], "pwd");
        assert_eq!(resp.usage.input_tokens, 7);
    }

    #[test]
    fn response_plain_text() {
        let wire: WireResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {"content": "hello"},
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        let resp = from_wire(wire).unwrap();
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(
            resp.content,
            vec![ContentBlock::Text {
                text: "hello".into()
            }]
        );
    }

    #[test]
    fn length_finish_maps_to_max_tokens() {
        let wire: WireResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": "cut"}, "finish_reason": "length"}]
        }))
        .unwrap();
        assert_eq!(from_wire(wire).unwrap().stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn empty_choices_malformed() {
        let wire: WireResponse = serde_json::from_value(json!({"choices": []})).unwrap();
        assert!(from_wire(wire).is_err());
    }
}

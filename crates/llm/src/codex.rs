//! ChatGPT/Codex subscription adapter.
//!
//! Speaks the OpenAI Responses shape against the Codex subscription backend
//! (`chatgpt.com/backend-api/codex/responses`), which requires OAuth bearer
//! auth plus a `chatgpt-account-id` header and streams results over SSE.
//!
//! Two quirks inherited from the wire contract (both verified against neo's
//! implementation):
//! - the backend omits `output` from the terminal `response.completed` event,
//!   so content is assembled from per-item `response.output_item.done` events;
//! - encrypted `reasoning` items must be replayed verbatim on later turns.
//!   They are carried through the transcript as [`ContentBlock::Opaque`]
//!   blocks tagged `provider: "codex"` and replayed only by this adapter.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::retry;
use crate::sse;
use crate::{
    ContentBlock, Provider, ProviderError, Request, Response, Role, StopReason, Usage,
};

const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
/// Fallback when neither `-m` nor a Codex CLI config supplies a model.
/// (`gpt-5-codex` is rejected for ChatGPT accounts — verified live 2026-08-07.)
pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-sol";
pub const OPAQUE_PROVIDER: &str = "codex";

/// Yields a valid subscription access token and its ChatGPT account id,
/// refreshing as needed. Implemented by `bullpen-auth`.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<CodexToken, ProviderError>;
}

#[derive(Debug, Clone)]
pub struct CodexToken {
    pub access_token: String,
    pub account_id: String,
}

pub struct Codex {
    client: reqwest::Client,
    source: Arc<dyn TokenSource>,
    endpoint: String,
}

impl Codex {
    pub fn new(source: Arc<dyn TokenSource>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
            source,
            endpoint: CODEX_ENDPOINT.to_string(),
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

#[async_trait::async_trait]
impl Provider for Codex {
    fn name(&self) -> &str {
        "codex"
    }

    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = build_request(req);
        let mut attempt = 0u32;
        loop {
            let token = self.source.token().await?;
            let result = self
                .client
                .post(&self.endpoint)
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream")
                .header("Authorization", format!("Bearer {}", token.access_token))
                .header("chatgpt-account-id", &token.account_id)
                .header("originator", "bullpen")
                .header("OpenAI-Beta", "responses=experimental")
                .json(&body)
                .send()
                .await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp
                        .text()
                        .await
                        .map_err(|e| ProviderError::Malformed(e.to_string()))?;
                    return parse_stream(&text);
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
                        tracing::warn!(status, attempt, ?wait, "retrying codex call");
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

// ── Request build ───────────────────────────────────────────────────────────

fn build_request(req: &Request) -> Value {
    let instructions = if req.system.is_empty() {
        "You are a helpful assistant.".to_string()
    } else {
        req.system.clone()
    };

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })
        })
        .collect();

    // No max_output_tokens: the subscription backend rejects the parameter
    // outright (verified live 2026-08-07), unlike the public Responses API.
    json!({
        "model": req.model,
        "instructions": instructions,
        "input": to_input(req),
        "tools": tools,
        "tool_choice": "auto",
        "include": ["reasoning.encrypted_content"],
        "store": false,
        "stream": true,
    })
}

fn to_input(req: &Request) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::Assistant => {
                let mut parts: Vec<Value> = Vec::new();
                let flush = |parts: &mut Vec<Value>, out: &mut Vec<Value>| {
                    if !parts.is_empty() {
                        out.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": std::mem::take(parts),
                        }));
                    }
                };
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            parts.push(json!({"type": "output_text", "text": text}));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            flush(&mut parts, &mut out);
                            out.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": input.to_string(),
                            }));
                        }
                        ContentBlock::Opaque { provider, data }
                            if provider == OPAQUE_PROVIDER && replayable_reasoning(data) =>
                        {
                            flush(&mut parts, &mut out);
                            out.push(data.clone());
                        }
                        _ => {}
                    }
                }
                flush(&mut parts, &mut out);
            }
            Role::User => {
                let mut parts: Vec<Value> = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let output = if *is_error {
                                format!("ERROR: {content}")
                            } else {
                                content.clone()
                            };
                            // `output` is required on every function_call_output
                            // item, even when empty.
                            out.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }));
                        }
                        ContentBlock::Text { text } if !text.is_empty() => {
                            parts.push(json!({"type": "input_text", "text": text}));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(json!({"type": "message", "role": "user", "content": parts}));
                }
            }
        }
    }
    out
}

/// Whether an opaque item is a reasoning block the backend will accept as
/// replay: type "reasoning", non-empty id and encrypted_content, and a
/// summary that is either absent or a real array.
fn replayable_reasoning(data: &Value) -> bool {
    let nonempty = |key: &str| data.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    if data.get("type").and_then(Value::as_str) != Some("reasoning")
        || !nonempty("id")
        || !nonempty("encrypted_content")
    {
        return false;
    }
    match data.get("summary") {
        None => true,
        Some(Value::Array(_)) => true,
        Some(_) => false,
    }
}

// ── Response parse ──────────────────────────────────────────────────────────

fn parse_stream(raw: &str) -> Result<Response, ProviderError> {
    if !raw.contains("data:") {
        let body: Value = serde_json::from_str(raw)
            .map_err(|e| ProviderError::Malformed(format!("decode: {e} (body: {raw})")))?;
        return to_response(&body);
    }

    let mut completed: Option<Value> = None;
    let mut items: Vec<Value> = Vec::new();
    for data in sse::data_lines(raw) {
        let Ok(ev) = serde_json::from_str::<Map<String, Value>>(data) else {
            continue; // ignore malformed event lines
        };
        match ev.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_item.done" => {
                if let Some(item) = ev.get("item") {
                    items.push(item.clone());
                }
            }
            "response.completed" | "response.incomplete" => {
                completed = ev.get("response").cloned();
            }
            "response.failed" | "error" => {
                let message = ev
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| ev.get("message").and_then(Value::as_str))
                    .or_else(|| {
                        ev.get("response")
                            .and_then(|r| r.get("error"))
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("request failed")
                    .to_string();
                return Err(ProviderError::Failure(format!("codex: {message}")));
            }
            _ => {}
        }
    }

    let mut completed = completed.ok_or_else(|| {
        ProviderError::Failure("codex: stream ended without a completed response".into())
    })?;
    // The Codex backend leaves `output` empty on response.completed; fill it
    // from the per-item done events. If the server included output (public
    // API shape), prefer it.
    let has_output = completed
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if !has_output {
        completed["output"] = Value::Array(items);
    }
    to_response(&completed)
}

fn to_response(body: &Value) -> Result<Response, ProviderError> {
    if let Some(message) = body
        .get("error")
        .filter(|e| !e.is_null())
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return Err(ProviderError::Failure(format!("codex: {message}")));
    }

    let mut content = Vec::new();
    let mut saw_tool_call = false;
    for item in body.get("output").and_then(Value::as_array).into_iter().flatten() {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                let mut text = String::new();
                for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                    match part.get("type").and_then(Value::as_str).unwrap_or("") {
                        "output_text" => {
                            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""))
                        }
                        "refusal" => {
                            let refusal = part
                                .get("refusal")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .unwrap_or("The model refused to provide a response.");
                            text.push_str(refusal);
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    content.push(ContentBlock::Text { text });
                }
            }
            "function_call" => {
                saw_tool_call = true;
                let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                let input: Value = if arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(arguments).map_err(|e| {
                        ProviderError::Malformed(format!("tool arguments are not JSON: {e}"))
                    })?
                };
                content.push(ContentBlock::ToolUse {
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input,
                });
            }
            "reasoning" => {
                content.push(ContentBlock::Opaque {
                    provider: OPAQUE_PROVIDER.to_string(),
                    data: item.clone(),
                });
            }
            _ => {}
        }
    }

    let stop_reason = if saw_tool_call {
        StopReason::ToolUse
    } else if body.get("status").and_then(Value::as_str) == Some("incomplete")
        && body
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            == Some("max_output_tokens")
    {
        StopReason::MaxTokens
    } else {
        StopReason::EndTurn
    };

    let usage = body
        .get("usage")
        .map(|u| Usage {
            input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        })
        .unwrap_or_default();

    Ok(Response {
        content,
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolSpec};

    fn sample_request() -> Request {
        Request {
            model: "gpt-5-codex".into(),
            system: "be terse".into(),
            messages: vec![
                Message::user_text("go"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Opaque {
                            provider: "codex".into(),
                            data: json!({
                                "type": "reasoning",
                                "id": "rs_1",
                                "encrypted_content": "abc",
                                "summary": []
                            }),
                        },
                        ContentBlock::Text { text: "running".into() },
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "bash".into(),
                            input: json!({"command": "ls"}),
                        },
                    ],
                },
                Message::tool_results(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: String::new(),
                    is_error: false,
                }]),
            ],
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: "run".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 256,
        }
    }

    #[test]
    fn request_shape() {
        let wire = build_request(&sample_request());
        assert_eq!(wire["instructions"], "be terse");
        assert_eq!(wire["store"], false);
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["include"][0], "reasoning.encrypted_content");
        // Responses tools are flat, not nested under "function".
        assert_eq!(wire["tools"][0]["name"], "bash");

        let input = wire["input"].as_array().unwrap();
        // user message, reasoning replay, assistant text, function_call, output
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["arguments"], "{\"command\":\"ls\"}");
        // empty output must still be present
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["output"], "");
    }

    #[test]
    fn foreign_and_invalid_opaque_not_replayed() {
        let mut req = sample_request();
        req.messages[1].content[0] = ContentBlock::Opaque {
            provider: "someone-else".into(),
            data: json!({"type": "reasoning", "id": "x", "encrypted_content": "y"}),
        };
        let wire = build_request(&req);
        assert!(!wire["input"].as_array().unwrap().iter().any(|i| i["type"] == "reasoning"));

        // summary present but not an array → not replayable
        assert!(!replayable_reasoning(&json!({
            "type": "reasoning", "id": "x", "encrypted_content": "y", "summary": null
        })));
        assert!(replayable_reasoning(&json!({
            "type": "reasoning", "id": "x", "encrypted_content": "y"
        })));
    }

    #[test]
    fn parses_codex_sse_stream() {
        let stream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"zzz\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"checking\"}]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_7\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"pwd\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":22}}}\n\n",
        );
        let resp = parse_stream(stream).unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 11);
        assert_eq!(resp.content.len(), 3);
        assert!(matches!(&resp.content[0], ContentBlock::Opaque { provider, .. } if provider == "codex"));
        assert!(matches!(&resp.content[1], ContentBlock::Text { text } if text == "checking"));
        let ContentBlock::ToolUse { id, name, input } = &resp.content[2] else {
            panic!("expected tool use");
        };
        assert_eq!((id.as_str(), name.as_str()), ("call_7", "bash"));
        assert_eq!(input["command"], "pwd");
    }

    #[test]
    fn failed_event_is_error() {
        let stream = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"quota exhausted\"}}}\n";
        let err = parse_stream(stream).unwrap_err();
        assert!(err.to_string().contains("quota exhausted"));
    }

    #[test]
    fn incomplete_max_tokens_maps() {
        let stream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"cut\"}]}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[]}}\n\n",
        );
        let resp = parse_stream(stream).unwrap();
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn non_sse_body_parses_directly() {
        let body = r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":1,"output_tokens":2}}"#;
        let resp = parse_stream(body).unwrap();
        assert_eq!(resp.content, vec![ContentBlock::Text { text: "hi".into() }]);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn stream_without_completed_is_error() {
        let stream = "data: {\"type\":\"response.created\"}\n";
        assert!(parse_stream(stream).is_err());
    }
}

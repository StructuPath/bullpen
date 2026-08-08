//! Anthropic Messages API adapter.
//!
//! Translates the shared conversation types into Anthropic wire format,
//! normalizes stop reasons and usage, and applies the shared retry policy.
//! Wire conversion is kept in pure functions so it can be tested without a
//! network.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use futures::StreamExt;

use crate::retry;
use crate::{
    ContentBlock, Provider, ProviderError, Request, Response, StopReason, TextSink, ToolSpec, Usage,
};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
pub const DEFAULT_MODEL: &str = "claude-sonnet-5";

// GLM (Z.ai) and Kimi (Moonshot) both expose Anthropic-compatible Messages
// endpoints, so they reuse this adapter's wire conversion entirely — only the
// endpoint, auth header, and default model differ. These URLs and models are
// from published docs as of the 2026-01 knowledge cutoff and are NOT verified
// live here (no keys in the build env); confirm against current provider docs.
const GLM_URL: &str = "https://api.z.ai/api/anthropic/v1/messages";
pub const GLM_DEFAULT_MODEL: &str = "glm-4.6";
const KIMI_URL: &str = "https://api.moonshot.ai/anthropic/v1/messages";
pub const KIMI_DEFAULT_MODEL: &str = "kimi-k2-0905-preview";

/// How the API key is presented. Anthropic proper uses `x-api-key`;
/// Moonshot's Anthropic-compatible endpoint uses a bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthStyle {
    XApiKey,
    Bearer,
}

pub struct Anthropic {
    client: reqwest::Client,
    name: &'static str,
    api_key: String,
    base_url: String,
    auth: AuthStyle,
    /// Whether to advertise prompt-cache breakpoints on the wire.
    caching: bool,
    /// Whether real SSE streaming is used; off for compatible endpoints
    /// whose streaming shape isn't confirmed (they buffer instead).
    streaming: bool,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
            name: "anthropic",
            api_key: api_key.into(),
            base_url: API_URL.to_string(),
            auth: AuthStyle::XApiKey,
            caching: true,
            streaming: true,
        }
    }

    /// GLM (Z.ai) via its Anthropic-compatible endpoint.
    pub fn glm(api_key: impl Into<String>) -> Self {
        let mut p = Self::new(api_key);
        p.name = "glm";
        p.base_url = GLM_URL.to_string();
        p.streaming = false;
        // Z.ai's Anthropic endpoint accepts x-api-key; caching semantics are
        // not guaranteed, so it stays off until confirmed.
        p.caching = false;
        p
    }

    /// Kimi (Moonshot) via its Anthropic-compatible endpoint.
    pub fn kimi(api_key: impl Into<String>) -> Self {
        let mut p = Self::new(api_key);
        p.name = "kimi";
        p.base_url = KIMI_URL.to_string();
        p.auth = AuthStyle::Bearer;
        p.streaming = false;
        p.caching = false;
        p
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
        self.name
    }

    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = to_wire(req, self.caching);
        let mut attempt = 0u32;
        loop {
            let mut builder = self
                .client
                .post(&self.base_url)
                .header("anthropic-version", API_VERSION);
            builder = match self.auth {
                AuthStyle::XApiKey => builder.header("x-api-key", &self.api_key),
                AuthStyle::Bearer => builder.bearer_auth(&self.api_key),
            };
            let result = builder.json(&body).send().await;

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

    async fn complete_streaming(
        &self,
        req: &Request,
        deltas: &TextSink,
    ) -> Result<Response, ProviderError> {
        if !self.streaming {
            // Compatible endpoints whose SSE shape isn't confirmed: buffer,
            // then emit the whole text as one delta (the trait default).
            let response = self.complete(req).await?;
            for block in &response.content {
                if let ContentBlock::Text { text } = block {
                    let _ = deltas.send(text.clone());
                }
            }
            return Ok(response);
        }

        let mut body = to_wire(req, self.caching);
        body["stream"] = Value::Bool(true);

        // Retries apply only to establishing the stream. Once bytes flow we
        // never retry mid-stream — a partial stream is abandoned, not
        // persisted (see ARCHITECTURE.md "never persist partial streams").
        let mut attempt = 0u32;
        let resp = loop {
            let mut builder = self
                .client
                .post(&self.base_url)
                .header("anthropic-version", API_VERSION)
                .header("accept", "text/event-stream");
            builder = match self.auth {
                AuthStyle::XApiKey => builder.header("x-api-key", &self.api_key),
                AuthStyle::Bearer => builder.bearer_auth(&self.api_key),
            };
            match builder.json(&body).send().await {
                Ok(resp) if resp.status().is_success() => break resp,
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    if retry::retryable_status(status) && attempt + 1 < retry::MAX_ATTEMPTS {
                        tokio::time::sleep(retry::delay(attempt, after)).await;
                        attempt += 1;
                        continue;
                    }
                    let message = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::Api { status, message });
                }
                Err(e) => {
                    let transient = e.is_timeout() || e.is_connect() || e.is_request();
                    if transient && attempt + 1 < retry::MAX_ATTEMPTS {
                        tokio::time::sleep(retry::delay(attempt, None)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(ProviderError::Transport(e));
                }
            }
        };

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut acc = StreamAccumulator::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(ProviderError::Transport)?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // SSE events are separated by a blank line.
            while let Some(pos) = buf.find("\n\n") {
                let frame: String = buf.drain(..pos + 2).collect();
                for line in frame.lines() {
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if data.is_empty() {
                            continue;
                        }
                        if let Ok(event) = serde_json::from_str::<Value>(data) {
                            acc.handle(&event, deltas);
                        }
                    }
                }
            }
        }
        acc.finish()
    }
}

/// Assembles a streamed Anthropic message from its SSE events. Text deltas
/// are forwarded live; tool-call arguments arrive as partial JSON fragments
/// accumulated per content-block index and parsed at the end.
#[derive(Default)]
struct StreamAccumulator {
    blocks: Vec<StreamBlock>,
    stop_reason: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
}

enum StreamBlock {
    Text(String),
    ToolUse { id: String, name: String, json: String },
    Ignored,
}

impl StreamAccumulator {
    fn handle(&mut self, event: &Value, deltas: &TextSink) {
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(u) = event.get("message").and_then(|m| m.get("usage")) {
                    self.input_tokens = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                }
            }
            "content_block_start" => {
                let block = event.get("content_block");
                let kind = block.and_then(|b| b.get("type")).and_then(Value::as_str);
                self.blocks.push(match kind {
                    Some("text") => StreamBlock::Text(
                        block
                            .and_then(|b| b.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Some("tool_use") => StreamBlock::ToolUse {
                        id: block
                            .and_then(|b| b.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .and_then(|b| b.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        json: String::new(),
                    },
                    _ => StreamBlock::Ignored,
                });
            }
            "content_block_delta" => {
                let idx = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = event.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.and_then(|d| d.get("text")).and_then(Value::as_str)
                        {
                            let _ = deltas.send(text.to_string());
                            if let Some(StreamBlock::Text(acc)) = self.blocks.get_mut(idx) {
                                acc.push_str(text);
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(StreamBlock::ToolUse { json, .. }), Some(partial)) = (
                            self.blocks.get_mut(idx),
                            delta
                                .and_then(|d| d.get("partial_json"))
                                .and_then(Value::as_str),
                        ) {
                            json.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(ot) = event
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.output_tokens = ot;
                }
            }
            "error" => {
                self.stop_reason = Some("error".to_string());
            }
            _ => {}
        }
    }

    fn finish(self) -> Result<Response, ProviderError> {
        let mut content = Vec::new();
        for block in self.blocks {
            match block {
                StreamBlock::Text(text) if !text.is_empty() => {
                    content.push(ContentBlock::Text { text })
                }
                StreamBlock::ToolUse { id, name, json } => {
                    let input = if json.trim().is_empty() {
                        Value::Object(Default::default())
                    } else {
                        serde_json::from_str(&json).map_err(|e| {
                            ProviderError::Malformed(format!("streamed tool json for {name}: {e}"))
                        })?
                    };
                    content.push(ContentBlock::ToolUse { id, name, input });
                }
                _ => {}
            }
        }
        let stop_reason = match self.stop_reason.as_deref() {
            Some("end_turn") | Some("stop_sequence") | None => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some(other) => StopReason::Other(other.to_string()),
        };
        Ok(Response {
            content,
            stop_reason,
            usage: Usage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            },
        })
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

fn to_wire(req: &Request, caching: bool) -> Value {
    let messages = req
        .messages
        .iter()
        .filter_map(|m| {
            let content: Vec<WireBlock> = m.content.iter().filter_map(block_to_wire).collect();
            // A message left empty after dropping foreign opaque blocks would
            // be rejected by the API; skip it entirely.
            if content.is_empty() {
                return None;
            }
            Some(WireMessage {
                role: match m.role {
                    crate::Role::User => "user".to_string(),
                    crate::Role::Assistant => "assistant".to_string(),
                },
                content,
            })
        })
        .collect();

    let wire = WireRequest {
        model: &req.model,
        max_tokens: req.max_tokens,
        system: &req.system,
        messages,
        tools: req.tools.iter().map(tool_to_wire).collect(),
    };
    let mut body = serde_json::to_value(&wire).expect("wire request serializes");

    // Prompt caching: mark the (stable) system prompt as an ephemeral cache
    // breakpoint. The Anthropic `system` field accepts an array of blocks;
    // caching the system prompt is the highest-leverage, lowest-risk marker
    // because it is identical across every turn of a session.
    if caching && !req.system.is_empty() {
        body["system"] = serde_json::json!([{
            "type": "text",
            "text": req.system,
            "cache_control": { "type": "ephemeral" }
        }]);
    }
    body
}

fn block_to_wire(block: &ContentBlock) -> Option<WireBlock> {
    match block {
        ContentBlock::Text { text } => Some(WireBlock::Text { text: text.clone() }),
        ContentBlock::ToolUse { id, name, input } => Some(WireBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Some(WireBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        }),
        // Another provider's replay data; this adapter neither produces nor
        // sends opaque blocks.
        ContentBlock::Opaque { .. } => None,
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
        let wire = to_wire(&sample_request(), false);
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
    fn caching_marks_system_prompt() {
        let plain = to_wire(&sample_request(), false);
        assert_eq!(plain["system"], "be terse");

        let cached = to_wire(&sample_request(), true);
        assert_eq!(cached["system"][0]["type"], "text");
        assert_eq!(cached["system"][0]["text"], "be terse");
        assert_eq!(cached["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn sse_accumulator_assembles_text_and_tool_call() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = StreamAccumulator::default();
        let events = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 12}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Look"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ing"}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tu_1", "name": "bash"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        ];
        for e in &events {
            acc.handle(e, &tx);
        }
        let resp = acc.finish().unwrap();

        // Text deltas were forwarded live, in order.
        let mut streamed = String::new();
        while let Ok(d) = rx.try_recv() {
            streamed.push_str(&d);
        }
        assert_eq!(streamed, "Looking");

        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 7);
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Looking"));
        let ContentBlock::ToolUse { id, name, input } = &resp.content[1] else {
            panic!("expected tool use");
        };
        assert_eq!((id.as_str(), name.as_str()), ("tu_1", "bash"));
        assert_eq!(input["command"], "ls");
    }

    #[test]
    fn glm_and_kimi_reuse_the_wire() {
        let glm = Anthropic::glm("k");
        assert_eq!(glm.name(), "glm");
        assert_eq!(glm.auth, AuthStyle::XApiKey);
        let kimi = Anthropic::kimi("k");
        assert_eq!(kimi.name(), "kimi");
        assert_eq!(kimi.auth, AuthStyle::Bearer);
        // Both produce identical wire bodies to Anthropic proper.
        assert_eq!(to_wire(&sample_request(), false)["messages"].as_array().unwrap().len(), 3);
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

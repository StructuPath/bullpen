//! The agent loop.
//!
//! This crate owns the transcript, provider calls, tool-use continuation, and
//! the event stream — and nothing else. It knows nothing about configuration
//! files, sessions, skills, terminal UIs, or specific vendors. Everything
//! else composes around it.
//!
//! # Transcript invariants
//!
//! - Every `tool_use` block gets exactly one matching `tool_result`, in the
//!   model's request order, even when a tool fails or is unknown.
//! - An assistant message and its tool results are appended together, never
//!   separately, so the transcript is always structurally valid for the next
//!   provider call.

use std::sync::Arc;

use bullpen_llm::{
    ContentBlock, Message, Provider, ProviderError, Request, Role, StopReason, Usage,
};
use bullpen_tools::{Registry, ToolCtx};
use tokio::sync::mpsc::UnboundedSender;

/// Cap on any single tool result entering the transcript.
const MAX_TOOL_RESULT_BYTES: usize = 262_144; // 256 KiB

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub system: String,
    pub max_tokens: u32,
    /// Runaway-loop fuse, not a work budget.
    pub max_turns: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: String::new(),
            max_tokens: 8_192,
            max_turns: 500,
        }
    }
}

/// Everything observable about a running turn. Rendering lives outside the
/// loop; headless callers can collect metrics from the same stream.
#[derive(Debug, Clone)]
pub enum Event {
    AssistantText {
        text: String,
    },
    ToolStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolEnd {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    TurnDone {
        usage: Usage,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("provider stopped at max_tokens; partial output preserved")]
    Truncated { partial: String },
    #[error("exceeded {0} provider turns (runaway fuse)")]
    MaxTurns(u32),
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Registry,
    tool_ctx: ToolCtx,
    config: AgentConfig,
    messages: Vec<Message>,
    usage: Usage,
    events: Option<UnboundedSender<Event>>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Registry,
        tool_ctx: ToolCtx,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            registry,
            tool_ctx,
            config,
            messages: Vec::new(),
            usage: Usage::default(),
            events: None,
        }
    }

    /// Restore a transcript (resume). The caller is responsible for handing
    /// back messages that came from [`Agent::messages`].
    pub fn with_transcript(mut self, messages: Vec<Message>, usage: Usage) -> Self {
        self.messages = messages;
        self.usage = usage;
        self
    }

    /// Subscribe to turn events. One subscriber; call before `send`.
    pub fn with_events(mut self, tx: UnboundedSender<Event>) -> Self {
        self.events = Some(tx);
        self
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    fn emit(&self, event: Event) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event); // a gone subscriber never stops the loop
        }
    }

    /// Run one full turn: append the user text, then alternate provider calls
    /// and tool execution until the provider ends its turn. Returns the final
    /// assistant text.
    pub async fn send(&mut self, text: impl Into<String>) -> Result<String, AgentError> {
        self.messages.push(Message::user_text(text));

        for _ in 0..self.config.max_turns {
            let request = Request {
                model: self.config.model.clone(),
                system: self.config.system.clone(),
                messages: self.messages.clone(),
                tools: self.registry.specs(),
                max_tokens: self.config.max_tokens,
            };

            let response = self.provider.complete(&request).await?;
            self.usage += response.usage;

            let assistant_text = response
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !assistant_text.is_empty() {
                self.emit(Event::AssistantText {
                    text: assistant_text.clone(),
                });
            }

            let tool_uses: Vec<(String, String, serde_json::Value)> = response
                .tool_uses()
                .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
                .collect();

            if tool_uses.is_empty() {
                self.messages.push(Message {
                    role: Role::Assistant,
                    content: response.content,
                });
                self.emit(Event::TurnDone { usage: self.usage });
                return match response.stop_reason {
                    StopReason::MaxTokens => Err(AgentError::Truncated {
                        partial: assistant_text,
                    }),
                    _ => Ok(assistant_text),
                };
            }

            // Execute every requested tool and build results in request order
            // BEFORE committing anything to the transcript.
            let mut results = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in &tool_uses {
                self.emit(Event::ToolStart {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                let (output, is_error) = self.run_tool(name, input.clone()).await;
                self.emit(Event::ToolEnd {
                    id: id.clone(),
                    name: name.clone(),
                    output: output.clone(),
                    is_error,
                });
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: cap_result(output),
                    is_error,
                });
            }

            self.messages.push(Message {
                role: Role::Assistant,
                content: response.content,
            });
            self.messages.push(Message::tool_results(results));
        }

        Err(AgentError::MaxTurns(self.config.max_turns))
    }

    async fn run_tool(&self, name: &str, input: serde_json::Value) -> (String, bool) {
        let Some(tool) = self.registry.get(name) else {
            return (format!("unknown tool: {name}"), true);
        };
        match tool.run(&self.tool_ctx, input).await {
            Ok(output) => (output, false),
            Err(e) => (e.to_string(), true),
        }
    }
}

fn cap_result(mut s: String) -> String {
    if s.len() > MAX_TOOL_RESULT_BYTES {
        let mut end = MAX_TOOL_RESULT_BYTES;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n[result truncated at 256 KiB]");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use bullpen_llm::{Response, ToolSpec};
    use serde_json::json;
    use std::sync::Mutex;

    /// Scripted provider: pops pre-built responses and records requests.
    struct FakeProvider {
        script: Mutex<Vec<Response>>,
        seen: Mutex<Vec<Request>>,
    }

    impl FakeProvider {
        fn new(mut responses: Vec<Response>) -> Arc<Self> {
            responses.reverse();
            Arc::new(Self {
                script: Mutex::new(responses),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }
        async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
            self.seen.lock().unwrap().push(req.clone());
            self.script
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| ProviderError::Malformed("script exhausted".into()))
        }
    }

    struct Echo;

    #[async_trait::async_trait]
    impl bullpen_tools::Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo input".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn run(
            &self,
            _ctx: &ToolCtx,
            input: serde_json::Value,
        ) -> Result<String, bullpen_tools::ToolError> {
            Ok(format!("echo: {}", input["value"]))
        }
    }

    fn agent(provider: Arc<FakeProvider>) -> Agent {
        let mut registry = Registry::new();
        registry.register(Arc::new(Echo));
        Agent::new(
            provider,
            registry,
            ToolCtx {
                workspace: std::env::temp_dir(),
            },
            AgentConfig {
                model: "test-model".into(),
                system: "test".into(),
                ..Default::default()
            },
        )
    }

    fn text_response(text: &str, stop: StopReason) -> Response {
        Response {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: stop,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }
    }

    fn tool_response(id: &str, name: &str, input: serde_json::Value) -> Response {
        Response {
            content: vec![
                ContentBlock::Text {
                    text: "using tool".into(),
                },
                ContentBlock::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input,
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }
    }

    #[tokio::test]
    async fn simple_text_turn() {
        let provider = FakeProvider::new(vec![text_response("hello", StopReason::EndTurn)]);
        let mut agent = agent(provider);
        let out = agent.send("hi").await.unwrap();
        assert_eq!(out, "hello");
        assert_eq!(agent.messages().len(), 2);
        assert_eq!(agent.usage().output_tokens, 5);
    }

    #[tokio::test]
    async fn tool_roundtrip_pairs_result_with_use() {
        let provider = FakeProvider::new(vec![
            tool_response("tu_1", "echo", json!({"value": 42})),
            text_response("done", StopReason::EndTurn),
        ]);
        let mut agent = agent(provider.clone());
        let out = agent.send("go").await.unwrap();
        assert_eq!(out, "done");

        // user, assistant(tool_use), user(tool_result), assistant
        let msgs = agent.messages();
        assert_eq!(msgs.len(), 4);
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = &msgs[2].content[0]
        else {
            panic!("expected tool result, got {:?}", msgs[2]);
        };
        assert_eq!(tool_use_id, "tu_1");
        assert_eq!(content, "echo: 42");
        assert!(!is_error);

        // The second provider call must have seen the paired result.
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[1].messages.len(), 3);
    }

    #[tokio::test]
    async fn unknown_tool_yields_error_result_and_loop_continues() {
        let provider = FakeProvider::new(vec![
            tool_response("tu_1", "nonexistent", json!({})),
            text_response("recovered", StopReason::EndTurn),
        ]);
        let mut agent = agent(provider);
        let out = agent.send("go").await.unwrap();
        assert_eq!(out, "recovered");
        let ContentBlock::ToolResult { is_error, content, .. } = &agent.messages()[2].content[0]
        else {
            panic!("expected tool result");
        };
        assert!(is_error);
        assert!(content.contains("unknown tool"));
    }

    #[tokio::test]
    async fn max_tokens_stop_is_distinct_error_with_partial() {
        let provider = FakeProvider::new(vec![text_response("cut off", StopReason::MaxTokens)]);
        let mut agent = agent(provider);
        let err = agent.send("go").await.unwrap_err();
        let AgentError::Truncated { partial } = err else {
            panic!("expected Truncated, got {err:?}");
        };
        assert_eq!(partial, "cut off");
    }

    #[tokio::test]
    async fn max_turns_fuse_trips() {
        // A provider that always asks for another tool call.
        let responses: Vec<Response> = (0..3)
            .map(|i| tool_response(&format!("tu_{i}"), "echo", json!({"value": i})))
            .collect();
        let provider = FakeProvider::new(responses);
        let mut a = agent(provider);
        a.config.max_turns = 3;
        let err = a.send("go").await.unwrap_err();
        assert!(matches!(err, AgentError::MaxTurns(3)));
        // Fuse or not, the transcript stays structurally valid: every
        // assistant tool_use message is followed by its results message.
        let msgs = a.messages();
        for pair in msgs.windows(2) {
            if pair[0]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
            {
                assert!(
                    pair[1]
                        .content
                        .iter()
                        .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
                );
            }
        }
    }

    #[tokio::test]
    async fn events_stream_covers_tool_lifecycle() {
        let provider = FakeProvider::new(vec![
            tool_response("tu_1", "echo", json!({"value": 1})),
            text_response("done", StopReason::EndTurn),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut agent = agent(provider).with_events(tx);
        agent.send("go").await.unwrap();

        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(match ev {
                Event::AssistantText { .. } => "text",
                Event::ToolStart { .. } => "tool_start",
                Event::ToolEnd { .. } => "tool_end",
                Event::TurnDone { .. } => "done",
            });
        }
        assert_eq!(kinds, vec!["text", "tool_start", "tool_end", "text", "done"]);
    }
}

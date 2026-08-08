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

mod journal;

pub use journal::{Journal, JournalError, NullJournal, RunOutcome, ToolIntent};

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
    /// Concurrency cap for adjacent parallel-safe tool calls in one batch.
    pub max_parallel_tools: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: String::new(),
            max_tokens: 8_192,
            max_turns: 500,
            max_parallel_tools: 8,
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
    #[error(transparent)]
    Journal(#[from] JournalError),
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Registry,
    tool_ctx: ToolCtx,
    config: AgentConfig,
    messages: Vec<Message>,
    usage: Usage,
    events: Option<UnboundedSender<Event>>,
    delta_sink: Option<bullpen_llm::TextSink>,
    journal: Box<dyn Journal>,
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
            delta_sink: None,
            journal: Box::new(NullJournal),
        }
    }

    /// Stream assistant-text deltas to `sink` as they are generated. Without
    /// one, the loop uses buffered completion.
    pub fn with_delta_sink(mut self, sink: bullpen_llm::TextSink) -> Self {
        self.delta_sink = Some(sink);
        self
    }

    /// Attach a durability journal. Without one, execution is in-memory only.
    pub fn with_journal(mut self, journal: Box<dyn Journal>) -> Self {
        self.journal = journal;
        self
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
    ///
    /// Durability protocol (see [`Journal`]): the run start, every assistant
    /// message, and every tool intent are journaled *before* the next effect
    /// runs. On error paths `run_finished(Failed)` is attempted best-effort —
    /// if that final write also fails, the operation simply stays open and
    /// recovery closes it.
    pub async fn send(&mut self, text: impl Into<String>) -> Result<String, AgentError> {
        let user = Message::user_text(text);
        self.journal.run_started(&user).await?;
        self.messages.push(user);

        match self.drive().await {
            Ok(answer) => Ok(answer),
            Err(e) => {
                let outcome = match &e {
                    AgentError::Truncated { .. } => RunOutcome::Truncated,
                    _ => RunOutcome::Failed,
                };
                let _ = self.journal.run_finished(outcome, self.usage).await;
                Err(e)
            }
        }
    }

    async fn drive(&mut self) -> Result<String, AgentError> {
        for attempt in 1..=self.config.max_turns {
            self.journal.step_attempt(attempt).await?;
            let request = Request {
                model: self.config.model.clone(),
                system: self.config.system.clone(),
                messages: self.messages.clone(),
                tools: self.registry.specs(),
                max_tokens: self.config.max_tokens,
            };

            let response = match &self.delta_sink {
                Some(sink) => self.provider.complete_streaming(&request, sink).await?,
                None => self.provider.complete(&request).await?,
            };
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

            let assistant = Message {
                role: Role::Assistant,
                content: response.content,
            };
            self.journal.assistant_message(&assistant).await?;

            let intents: Vec<ToolIntent> = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => Some(ToolIntent {
                        tool_use_id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        replay_safe: self.registry.get(name).is_some_and(|t| t.replay_safe()),
                    }),
                    _ => None,
                })
                .collect();

            if intents.is_empty() {
                self.messages.push(assistant);
                self.emit(Event::TurnDone { usage: self.usage });
                return match response.stop_reason {
                    StopReason::MaxTokens => Err(AgentError::Truncated {
                        partial: assistant_text,
                    }),
                    _ => {
                        self.journal
                            .run_finished(RunOutcome::Completed, self.usage)
                            .await?;
                        Ok(assistant_text)
                    }
                };
            }

            // Intents become durable before any tool effect starts —
            // regardless of how the batch is scheduled below.
            self.journal.tool_batch(&intents).await?;

            let outputs = self.execute_batch(&intents).await;
            let results: Vec<ContentBlock> = intents
                .iter()
                .zip(outputs)
                .map(|(intent, (output, is_error))| ContentBlock::ToolResult {
                    tool_use_id: intent.tool_use_id.clone(),
                    content: cap_result(output),
                    is_error,
                })
                .collect();

            let results = Message::tool_results(results);
            self.journal.tool_results(&results).await?;
            self.messages.push(assistant);
            self.messages.push(results);
        }

        Err(AgentError::MaxTurns(self.config.max_turns))
    }

    /// Execute one batch of tool calls and return outputs in request order.
    ///
    /// Scheduling follows the rule the runtime owns: maximal *adjacent* runs
    /// of parallel-safe calls (per [`bullpen_tools::Tool::parallel_safe`],
    /// judged against the concrete input) execute concurrently under a
    /// semaphore; everything else — mutating tools, shell, unknown tools —
    /// runs serially in place. Events fire as execution happens (completion
    /// order); results always return in request order, which is the order
    /// they enter the transcript.
    async fn execute_batch(&self, intents: &[ToolIntent]) -> Vec<(String, bool)> {
        let parallel: Vec<bool> = intents
            .iter()
            .map(|i| {
                self.registry
                    .get(&i.name)
                    .is_some_and(|t| t.parallel_safe(&i.input))
            })
            .collect();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
            self.config.max_parallel_tools.max(1),
        ));

        let mut outputs: Vec<(String, bool)> = Vec::with_capacity(intents.len());
        let mut idx = 0;
        while idx < intents.len() {
            if parallel[idx] {
                let mut end = idx + 1;
                while end < intents.len() && parallel[end] {
                    end += 1;
                }
                let group = intents[idx..end].iter().map(|intent| {
                    let semaphore = semaphore.clone();
                    async move {
                        let _permit = semaphore.acquire().await.expect("semaphore open");
                        self.run_tool_with_events(intent).await
                    }
                });
                outputs.extend(futures::future::join_all(group).await);
                idx = end;
            } else {
                outputs.push(self.run_tool_with_events(&intents[idx]).await);
                idx += 1;
            }
        }
        outputs
    }

    async fn run_tool_with_events(&self, intent: &ToolIntent) -> (String, bool) {
        self.emit(Event::ToolStart {
            id: intent.tool_use_id.clone(),
            name: intent.name.clone(),
            input: intent.input.clone(),
        });
        let (output, is_error) = self
            .run_tool(&intent.name, &intent.tool_use_id, intent.input.clone())
            .await;
        self.emit(Event::ToolEnd {
            id: intent.tool_use_id.clone(),
            name: intent.name.clone(),
            output: output.clone(),
            is_error,
        });
        (output, is_error)
    }

    async fn run_tool(
        &self,
        name: &str,
        call_id: &str,
        input: serde_json::Value,
    ) -> (String, bool) {
        let Some(tool) = self.registry.get(name) else {
            return (format!("unknown tool: {name}"), true);
        };
        match tool.run(&self.tool_ctx, call_id, input).await {
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
            _call_id: &str,
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
            ToolCtx::new(std::env::temp_dir()),
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
        let ContentBlock::ToolResult {
            is_error, content, ..
        } = &agent.messages()[2].content[0]
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

    /// Sleeps, and records how many instances ran at once.
    struct SlowTool {
        safe: bool,
        delay_ms: u64,
        current: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl bullpen_tools::Tool for SlowTool {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "slow".into(),
                description: "slow".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        fn parallel_safe(&self, _input: &serde_json::Value) -> bool {
            self.safe
        }
        async fn run(
            &self,
            _ctx: &ToolCtx,
            _call_id: &str,
            input: serde_json::Value,
        ) -> Result<String, bullpen_tools::ToolError> {
            use std::sync::atomic::Ordering;
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok(format!("slow:{}", input["tag"]))
        }
    }

    /// One assistant response requesting several `slow` calls.
    fn multi_tool_response(tags: &[u64]) -> Response {
        Response {
            content: tags
                .iter()
                .map(|t| ContentBlock::ToolUse {
                    id: format!("tu_{t}"),
                    name: "slow".into(),
                    input: json!({"tag": t}),
                })
                .collect(),
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
        }
    }

    fn slow_agent(
        provider: Arc<FakeProvider>,
        safe: bool,
        max_parallel: usize,
    ) -> (Agent, Arc<std::sync::atomic::AtomicUsize>) {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = Registry::new();
        registry.register(Arc::new(SlowTool {
            safe,
            delay_ms: 40,
            current: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: peak.clone(),
        }));
        let agent = Agent::new(
            provider,
            registry,
            ToolCtx::new(std::env::temp_dir()),
            AgentConfig {
                model: "test".into(),
                system: "test".into(),
                max_parallel_tools: max_parallel,
                ..Default::default()
            },
        );
        (agent, peak)
    }

    fn batch_results(agent: &Agent) -> Vec<String> {
        agent.messages()[2]
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::ToolResult { content, .. } => content.clone(),
                other => panic!("expected result, got {other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn parallel_safe_batch_overlaps_and_keeps_request_order() {
        let provider = FakeProvider::new(vec![
            multi_tool_response(&[1, 2, 3]),
            text_response("done", StopReason::EndTurn),
        ]);
        let (mut agent, peak) = slow_agent(provider, true, 8);
        agent.send("go").await.unwrap();

        assert!(
            peak.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "parallel-safe calls never overlapped"
        );
        assert_eq!(batch_results(&agent), vec!["slow:1", "slow:2", "slow:3"]);
    }

    #[tokio::test]
    async fn unsafe_batch_stays_serial() {
        let provider = FakeProvider::new(vec![
            multi_tool_response(&[1, 2, 3]),
            text_response("done", StopReason::EndTurn),
        ]);
        let (mut agent, peak) = slow_agent(provider, false, 8);
        agent.send("go").await.unwrap();

        assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(batch_results(&agent), vec!["slow:1", "slow:2", "slow:3"]);
    }

    #[tokio::test]
    async fn semaphore_bounds_parallelism() {
        let provider = FakeProvider::new(vec![
            multi_tool_response(&[1, 2, 3, 4]),
            text_response("done", StopReason::EndTurn),
        ]);
        let (mut agent, peak) = slow_agent(provider, true, 1);
        agent.send("go").await.unwrap();
        assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Records the journal protocol calls in order.
    #[derive(Clone, Default)]
    struct RecordingJournal(Arc<Mutex<Vec<String>>>);

    #[async_trait::async_trait]
    impl Journal for RecordingJournal {
        async fn run_started(&mut self, _: &Message) -> Result<(), JournalError> {
            self.0.lock().unwrap().push("run_started".into());
            Ok(())
        }
        async fn step_attempt(&mut self, n: u32) -> Result<(), JournalError> {
            self.0.lock().unwrap().push(format!("step:{n}"));
            Ok(())
        }
        async fn assistant_message(&mut self, _: &Message) -> Result<(), JournalError> {
            self.0.lock().unwrap().push("assistant".into());
            Ok(())
        }
        async fn tool_batch(&mut self, intents: &[ToolIntent]) -> Result<(), JournalError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("intents:{}", intents.len()));
            Ok(())
        }
        async fn tool_results(&mut self, _: &Message) -> Result<(), JournalError> {
            self.0.lock().unwrap().push("results".into());
            Ok(())
        }
        async fn run_finished(
            &mut self,
            outcome: RunOutcome,
            _: Usage,
        ) -> Result<(), JournalError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("finished:{}", outcome.as_str()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn journal_sees_protocol_in_intent_order() {
        let provider = FakeProvider::new(vec![
            tool_response("tu_1", "echo", json!({"value": 1})),
            text_response("done", StopReason::EndTurn),
        ]);
        let recorder = RecordingJournal::default();
        let calls = recorder.0.clone();
        let mut agent = agent(provider).with_journal(Box::new(recorder));
        agent.send("go").await.unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "run_started",
                "step:1",
                "assistant",
                "intents:1",
                "results",
                "step:2",
                "assistant",
                "finished:completed",
            ]
        );
    }

    #[tokio::test]
    async fn journal_failure_faults_the_run() {
        struct FailingJournal;
        #[async_trait::async_trait]
        impl Journal for FailingJournal {
            async fn run_started(&mut self, _: &Message) -> Result<(), JournalError> {
                Err(JournalError("disk full".into()))
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

        let provider = FakeProvider::new(vec![text_response("hi", StopReason::EndTurn)]);
        let mut agent = agent(provider).with_journal(Box::new(FailingJournal));
        let err = agent.send("go").await.unwrap_err();
        assert!(matches!(err, AgentError::Journal(_)));
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
        assert_eq!(
            kinds,
            vec!["text", "tool_start", "tool_end", "text", "done"]
        );
    }
}

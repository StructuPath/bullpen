---
type: crate
title: bullpen-agent — the policy-free core loop
description: The transcript, provider calls, tool-use continuation, the event stream, the max-turns fuse, parallel batch scheduling, the 256 KiB tool-result cap, and the Journal durability protocol (trait only).
tags: [agent, core-loop, transcript, tool-continuation, journal, events, scheduling]
---

# `bullpen-agent`

`crates/agent/Cargo.toml` dependencies: `bullpen-llm`, `bullpen-tools`,
`async-trait`, `futures`, `tokio`, `tracing`, `serde_json`, `thiserror`,
`tempfile` (dev). **Not** `bullpen-store`, `bullpen-harness`, `bullpen-auth`,
or the CLI — the loop is policy-free.

This crate owns the transcript, provider calls, tool-use continuation, and the
event stream — and nothing else. It knows nothing about configuration files,
sessions, skills, terminal UIs, or specific vendors. Everything else composes
around it.

## Public surface

```rust
pub struct AgentConfig {
    pub model: String,
    pub system: String,
    pub max_tokens: u32,            // default 8192
    pub max_turns: u32,             // runaway-loop fuse, not a work budget; default 500
    pub max_parallel_tools: usize,  // default 8
}

pub enum Event {
    AssistantText { text },
    ToolStart { id, name, input },
    ToolEnd { id, name, output, is_error },
    TurnDone { usage },
}

pub struct Agent { provider, registry, tool_ctx, config, messages, usage, events, delta_sink, journal }
```

Construction is a builder: `Agent::new(provider, registry, tool_ctx, config)`
then `.with_delta_sink(TextSink)` (live text streaming), `.with_journal(Box<dyn
Journal>)` (durability; defaults to `NullJournal`), `.with_transcript(messages,
usage)` (resume), `.with_events(tx)` (one subscriber; call before `send`).

`MAX_TOOL_RESULT_BYTES = 262_144` (256 KiB) caps any single tool result the loop
hands on — to the transcript and to anything downstream that re-emits event
payloads. `cap_result` truncates on a char boundary with an inline marker.

## The run: `send(text) -> Result<String, AgentError>`

`send` appends the user message, journals `run_started`, then calls `drive()`.
On error it attempts `run_finished(Failed)` best-effort (if that final write
also fails, the operation simply stays open and [recovery](recovery.md) closes
it). `AgentError` is `Provider(ProviderError) | Truncated { partial } |
MaxTurns(u32) | Journal(JournalError)`. `Truncated` carries the partial text
and is a distinct error from a clean end — a `max_tokens` stop is never
silently continued.

`drive()` is the loop, `1..=max_turns`:

1. `journal.step_attempt(attempt)`.
2. Build the `Request` from the current `messages`, `system`, `tools`
   (`registry.specs()`), `max_tokens`.
3. Call `provider.complete_streaming(req, &delta_sink)` if a delta sink is
   attached, else `complete`. Accumulate `usage`.
4. Extract assistant text, emit `AssistantText` if non-empty.
5. Build the assistant `Message`; `journal.assistant_message(&assistant)`.
6. Collect `ContentBlock::ToolUse` blocks into `ToolIntent`s, snapshotting
   `replay_safe` from the registry (`t.replay_safe()`).
7. If no intents: push the assistant message, emit `TurnDone`, and return —
   `StopReason::MaxTokens` → `Err(Truncated { partial })`, else
   `run_finished(Completed)` and `Ok(assistant_text)`.
8. If there are intents: `journal.tool_batch(&intents)` (durable before any
   effect), `execute_batch(&intents)`, build `ContentBlock::ToolResult`s
   (capped via `cap_result`), build the tool-results `Message`,
   `journal.tool_results(&results)`, push **both** the assistant message and
   the results message together, loop.

The transcript invariants are enforced structurally here (see
[transcript invariants](../concepts/transcript-invariants.md)): every
`tool_use` gets exactly one `tool_result` in request order, including unknown
tools and failed tools (which produce error-shaped results); an assistant
message and its tool results are appended together, never separately, so the
transcript is always structurally valid for the next provider call — even
after the max-turns fuse trips.

## Batch scheduling: `execute_batch`

```text
let parallel[i] = registry.get(name).is_some_and(|t| t.parallel_safe(&input))
```

Scheduling follows the rule the runtime owns: maximal *adjacent* runs of
parallel-safe calls execute concurrently under a `Semaphore::new(max_parallel_tools.max(1))`;
everything else — mutating tools, shell, unknown tools — runs serially in
place. Events fire as execution happens (completion order); results always
return in **request order**, which is the order they enter the transcript.
`run_tool_with_events` emits `ToolStart`/`ToolEnd` around each call. Unknown
tools → `("unknown tool: {name}", true)` error result; known tools →
`(output, false)` or `(e.to_string(), true)`.

This is the [tools](tools.md) `parallel_safe` hook in action: read/grep/glob
are parallel-safe; bash and writes are serial; the `agent` (pen) tool is
parallel-safe only in `inspect` mode.

## The `Journal` trait (`journal.rs`)

The durability protocol, as a trait — the loop knows the protocol, not the
storage. `NullJournal` runs everything in memory; `bullpen-harness` provides
the SQLite-backed `StoreJournal` (see [harness](harness.md)).

```rust
#[async_trait]
pub trait Journal: Send + Sync {
    async fn run_started(&mut self, user: &Message) -> Result<(), JournalError>;
    async fn step_attempt(&mut self, attempt: u32) -> Result<(), JournalError>;
    async fn assistant_message(&mut self, message: &Message) -> Result<(), JournalError>;
    async fn tool_batch(&mut self, intents: &[ToolIntent]) -> Result<(), JournalError>;  // before any effect
    async fn tool_results(&mut self, results: &Message) -> Result<(), JournalError>;
    async fn run_finished(&mut self, outcome: RunOutcome, usage: Usage) -> Result<(), JournalError>;
}
```

`ToolIntent { tool_use_id, name, input, replay_safe }` is the snapshot journaled
at intent time. `RunOutcome` is `Completed | Truncated | Failed` with
`as_str()` for the wire. A journal write failure fails the run — durability is
not best-effort. See [durable execution](../architecture/durable-execution.md)
for the full protocol and the intent-before-effect rule.

## Focused tests

The unit suite uses a `FakeProvider` (scripted `Vec<Response>` popped in order,
records seen `Request`s) and an `Echo` tool. Helpers: `text_response`,
`tool_response`, `agent(provider)`.

- `simple_text_turn` — one text response → answer, 2 messages, usage
  accumulated.
- `tool_roundtrip_pairs_result_with_use` — a tool_use then a text response →
  4 messages (user, assistant tool_use, user tool_result, assistant); the
  result is paired to the right `tool_use_id`; the second provider call saw
  the paired result (3 messages in its request).
- `unknown_tool_yields_error_result_and_loop_continues` — an unknown tool
  produces an error-shaped result and the loop continues to recover.
- `max_tokens_stop_is_distinct_error_with_partial` — a `MaxTokens` stop →
  `AgentError::Truncated { partial }`, distinct from a clean end.
- `max_turns_fuse_trips` — a provider that always asks for a tool call hits
  `MaxTurns(3)`; the transcript stays structurally valid (every assistant
  tool_use message is followed by an all-tool_results message).
- Parallel scheduling: `SlowTool` (records peak concurrency via atomics) drives
  `parallel_safe_batch_overlaps_and_keeps_request_order` (peak ≥ 2 for three
  parallel-safe calls, results in request order `["slow:1","slow:2","slow:3"]`),
  `unsafe_batch_stays_serial` (peak == 1 for non-parallel-safe calls, same
  request-order results), and `semaphore_bounds_parallelism` (a `max_parallel_tools`
  of 1 caps peak at 1 even with four parallel-safe calls).
- Journal protocol (the loop's half; the store half is [harness](harness.md)):
  `journal_sees_protocol_in_intent_order` records the exact call sequence for a
  one-tool turn — `run_started, step:1, assistant, intents:1, results, step:2,
  assistant, finished:completed` — proving intents are journaled before `results`
  and `run_finished` runs once; `journal_failure_faults_the_run` makes a
  `FailingJournal` error from `run_started` and asserts the run faults with
  `AgentError::Journal` (durability is not best-effort).

See [the harness](harness.md) for the SQLite `Journal` implementation and the
end-to-end durability tests, and [event and streaming model](../concepts/event-and-streaming-model.md)
for how the `Event`/`TextSink` split renders.

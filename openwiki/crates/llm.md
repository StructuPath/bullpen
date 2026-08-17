---
type: crate
title: bullpen-llm — provider-neutral conversation protocol and wire adapters
description: The shared message/tool/usage types, the Provider trait with complete and complete_streaming, the TextSink live-text channel, the shared retry policy, and the SSE parser used by all wire-format adapters.
tags: [llm, provider, wire-format, streaming, retry]
---

# `bullpen-llm`

`bullpen-llm` is the boundary between bullpen and model vendors. It defines the
shared conversation types and the [`Provider`] trait; concrete adapters
translate these into vendor wire formats at the edge. Nothing above this crate
knows what vendor is in use — the [agent loop](agent.md) and [harness](harness.md)
operate purely on `Request`/`Response`.

`crates/llm/Cargo.toml` dependencies: `reqwest` (rustls, streaming), `serde`,
`tokio`, `async-trait`, `tracing`. Dev-dep `tokio` test-util.

## Module layout

- `lib.rs` — the neutral types and the `Provider` trait
- [`anthropic.rs`](anthropic-adapter.md) — Anthropic Messages adapter (true SSE streaming)
- [`codex.rs`](codex-adapter.md) — Codex (ChatGPT subscription) Responses/SSE adapter
- [`chatcompletions.rs`](chatcompletions-adapter.md) — OpenAI chat-completions adapter (OpenRouter)
- `retry.rs` — shared transient-failure retry policy
- `sse.rs` — minimal buffered SSE `data:` line parser

## Conversation types

`ContentBlock` is the central type — tool calls and their results are
first-class blocks so the transcript pairing invariant can be checked
structurally:

```rust
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
    Opaque { provider: String, data: Value }, // cross-provider replay
}
```

`ContentBlock::Opaque { provider, data }` is the cross-provider-replay
mechanism: each adapter replays only its own opaque blocks and every other
adapter skips them, which is what makes cross-provider session resume safe
(the motivating case is Codex encrypted `reasoning` items — see the
[codex adapter](codex-adapter.md)).

`Message { role: Role, content: Vec<ContentBlock> }` provides `user_text` and
`tool_results` constructors; `tool_results` debug-asserts every block is a
`ToolResult`. `Message::text()` concatenates text blocks, ignoring tool
traffic.

`Request { model, system, messages, tools, max_tokens }` and `Response
{ content, stop_reason, usage }` are the request/response shapes the loop
hands across. `StopReason` is `EndTurn | ToolUse | MaxTokens | Other(String)`.
`Usage { input_tokens, output_tokens }` is `Copy`/`Default` and `AddAssign` so
the loop can accumulate it. `ToolSpec { name, description, input_schema }` is
the provider-facing tool declaration built by the [registry](tools.md).

## The `Provider` trait

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError>;
    async fn complete_streaming(&self, req: &Request, deltas: &TextSink)
        -> Result<Response, ProviderError> { /* buffering default */ }
}
```

`complete_streaming` has a buffering default that calls `complete` and emits
the final text as one delta — so every provider streams *something* even
before it implements true streaming. Adapters that implement true streaming
(Anthropic today) forward text deltas to the `TextSink` as they arrive and
return the fully assembled response at the end.

`TextSink = tokio::sync::mpsc::UnboundedSender<String>` is the live
assistant-text channel, **distinct from the `Event` stream** the loop emits
for tool activity — see [event and streaming model](../concepts/event-and-streaming-model.md).
A gone receiver never stops generation (sends are best-effort).

`ProviderError` is `Transport(reqwest::Error) | Api { status, message } |
Malformed(String) | Failure(String)`. The [agent loop](agent.md) maps a
`Malformed`/`Api`/`Transport` into `AgentError::Provider`, and a `MaxTokens`
stop into `AgentError::Truncated`.

## Shared retry policy (`retry.rs`)

`MAX_ATTEMPTS = 5`. `retryable_status` returns true for `429` (rate limit),
`529` (Anthropic overloaded), and `500..600`. `delay(attempt, retry_after)`
is exponential backoff with a 500 ms base and 16 s cap; a `Retry-After`
header (when present) wins, capped at 4× the max delay. Adapters apply this to
*establishing* a response/stream; a partial stream is never retried mid-flight
(see the Anthropic adapter).

## SSE parser (`sse.rs`)

`data_lines(body)` yields the JSON payload of every `data:` line, skipping
blanks and `[DONE]`. It is a fully-buffered parser today (bullpen presents
blocking turn results); its shape allows an incremental upgrade without
changing callers. The Codex adapter uses it directly; the Anthropic adapter
implements its own streaming frame accumulator because it forwards text
deltas live.

## Tests

- `retry.rs` — `statuses` (retryable matrix), `backoff_caps` (cap + Retry-After)
- `sse.rs` — `extracts_data_skips_noise`
- Per-adapter tests live on their own pages.

## Adapters by wire format

Adapters are organized by **wire format**, not vendor — three formats cover
every supported provider, and compatible hosts become config:

| Wire format | Adapter | Providers |
|---|---|---|
| Anthropic messages | [`anthropic`](anthropic-adapter.md) | `anthropic`, `glm` (Z.ai), `kimi` (Moonshot) |
| OpenAI Responses/SSE | [`codex`](codex-adapter.md) | `codex` (ChatGPT subscription) |
| OpenAI chat-completions | [`chatcompletions`](chatcompletions-adapter.md) | `openrouter` |

See [providers and auth](../concepts/providers-and-auth.md) for the provider
matrix and how credentials map to adapters.

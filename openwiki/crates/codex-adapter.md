---
type: crate
title: bullpen-llm codex adapter — OpenAI Responses over SSE against the ChatGPT subscription backend
description: The Codex (ChatGPT subscription) adapter speaking OpenAI Responses over SSE, the TokenSource trait, and the verified wire quirks including encrypted reasoning replay as ContentBlock::Opaque.
tags: [llm, codex, openai-responses, sse, reasoning-replay, subscription]
---

# Codex adapter (`bullpen-llm::codex`)

`crates/llm/src/codex.rs`. Speaks the OpenAI Responses shape against the Codex
subscription backend at `https://chatgpt.com/backend-api/codex/responses`,
which requires OAuth bearer auth plus a `chatgpt-account-id` header and streams
results over SSE. Default model `gpt-5.6-sol` (`DEFAULT_CODEX_MODEL`) —
`gpt-5-codex` is rejected for ChatGPT accounts (verified live 2026-08-07).

`OPAQUE_PROVIDER = "codex"`. The adapter carries encrypted `reasoning` items
through the transcript as `ContentBlock::Opaque { provider: "codex", data }`
and replays only its own opaque blocks on later turns.

## The `TokenSource` trait

```rust
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<CodexToken, ProviderError>;
}
pub struct CodexToken { pub access_token: String, pub account_id: String }
```

Implemented twice by [`bullpen-auth`](auth.md):

- `StoredCodex` — bullpen's own login; refreshes proactively and persists
  rotations to the auth store.
- `CodexCliBorrow` — read-only reuse of the Codex CLI's `~/.codex/auth.json`;
  **never refreshes** (a rotation could invalidate the CLI's own session).

The CLI prefers a stored bullpen login, falling back to the borrow if
`~/.codex/auth.json` exists (see [the CLI](cli.md) `build_provider`).

## Request build (`build_request`)

```json
{
  "model": "...",
  "instructions": "<system or 'You are a helpful assistant.'>",
  "input": to_input(req),
  "tools": [{ "type": "function", "name", "description", "parameters" }],
  "tool_choice": "auto",
  "include": ["reasoning.encrypted_content"],
  "store": false,
  "stream": true
}
```

Notable: **no `max_output_tokens`** — the subscription backend rejects the
parameter outright (verified live), unlike the public Responses API.
`include: ["reasoning.encrypted_content"]` requests the encrypted reasoning
items that must be replayed verbatim on later turns.

`to_input` flattens the message list into the Responses `input` array:
- Assistant text → `{ type: "message", role: "assistant", content: [{ output_text }] }`,
  flushed when a tool_use/opaque item interrupts the run of text.
- `ContentBlock::ToolUse` → `{ type: "function_call", call_id, name, arguments: <json string> }`.
- `ContentBlock::Opaque { provider: "codex", data }` → replayed as `data`
  **only if** `replayable_reasoning(data)` is true (type `reasoning`, non-empty
  `id` and `encrypted_content`, `summary` absent or a real array). Foreign
  opaque blocks and invalid reasoning are dropped — this is the
  cross-provider-replay safety net.
- User text → `{ type: "message", role: "user", content: [{ input_text }] }`.
- `ContentBlock::ToolResult` → `{ type: "function_call_output", call_id,
  output }` (error results prefixed `ERROR: `; `output` is required even when
  empty).

## Response parse (`parse_stream`)

If the body contains no `data:`, it is parsed as a single JSON `Response`
(public API shape). Otherwise it is an SSE stream parsed via
[`sse::data_lines`](llm.md):

- `response.output_item.done` → collect each `item`
- `response.completed` / `response.incomplete` → capture the `response`
- `response.failed` / `error` → `ProviderError::Failure("codex: ...")`

Then `to_response`: if the completed response's `output` is empty (the Codex
backend **omits `output` from `response.completed`** — verified live), fill it
from the collected per-item `done` events; if the server included output
(public API shape), prefer it.

`to_response` walks `output` items: `message` → concatenate `output_text` /
`refusal` parts into a `ContentBlock::Text`; `function_call` → parse
`arguments` JSON into `ContentBlock::ToolUse { id: call_id, name, input }`
(empty arguments → `{}`); `reasoning` → `ContentBlock::Opaque { provider:
"codex", data: item }`. Stop reason is `ToolUse` if any tool call was seen,
`MaxTokens` if `status == "incomplete"` with `incomplete_details.reason ==
"max_output_tokens"`, else `EndTurn`.

## Retry behavior

Same shared retry policy: retryable status (`429`/`529`/`5xx`) with attempts
remaining → sleep `retry::delay` (honoring `Retry-After`) and retry; transient
transport errors likewise. The token is re-fetched on every attempt
(`self.source.token().await?`) so a refresh can land between retries.

## Focused tests

- `request_shape` — `instructions` set, `store: false`, `stream: true`,
  `include[0] == "reasoning.encrypted_content"`, tools flat (not nested under
  `function`), `input` length 5 (user message, reasoning replay, assistant
  text, function_call, function_call_output), empty `output` present.
- `foreign_and_invalid_opaque_not_replayed` — an `Opaque { provider:
  "someone-else", ... }` block does not appear in `input`; neither does an
  invalid reasoning block (missing `encrypted_content`).
- `parses_codex_sse_stream` — a full SSE stream with
  `response.output_item.done` events (reasoning, message, function_call) and a
  terminal `response.completed` whose `output` is empty parses into 3 content
  blocks (Opaque reasoning, text "checking", ToolUse `bash`) with
  `stop_reason == ToolUse` and the right usage — proving the assembly from
  per-item events.
- Live verification: tool round-trip and session resume verified live against
  the Codex subscription backend 2026-08-07
  (see [providers and auth](../concepts/providers-and-auth.md)).

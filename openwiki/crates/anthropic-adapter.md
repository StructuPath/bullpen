---
type: crate
title: bullpen-llm anthropic adapter — true SSE streaming over the Anthropic Messages API
description: The Anthropic Messages adapter with live text-delta streaming, per-block tool-call JSON accumulation, the prompt-cache system-prompt breakpoint, and GLM/Kimi compatible-endpoint construction.
tags: [llm, anthropic, streaming, sse, prompt-cache, glm, kimi]
---

# Anthropic Messages adapter (`bullpen-llm::anthropic`)

`crates/llm/src/anthropic.rs`. Endpoint `https://api.anthropic.com/v1/messages`,
API version `2023-06-01`. Default model `claude-sonnet-5`
(`DEFAULT_MODEL`). `AuthStyle::XApiKey` for Anthropic proper, `Bearer` for
Kimi's Anthropic-compatible endpoint.

The same adapter serves three providers by varying `base_url`, `auth`,
`caching`, and `streaming`:

- `Anthropic::new(key)` — Anthropic proper; `x-api-key`, prompt caching on,
  true SSE streaming on.
- `Anthropic::glm(key)` — Z.ai GLM via `https://api.z.ai/api/anthropic/v1/messages`;
  `x-api-key`, caching off, streaming off (SSE shape not confirmed live).
- `Anthropic::kimi(key)` — Moonshot Kimi via
  `https://api.moonshot.ai/anthropic/v1/messages`; bearer auth, caching off,
  streaming off. Default models `glm-4.6` / `kimi-k2-0905-preview`.

GLM/Kimi are deliberately *config-only* (no keys in the build env); their URLs
and models are from published docs and are not verified live here.

## Wire conversion (pure, network-free)

`to_wire(req, caching)` builds a `WireRequest { model, max_tokens, system,
messages, tools }` with `#[serde(skip_serializing_if)]` on empty system/tools.
`block_to_wire` maps each `ContentBlock` to a `WireBlock`; notably
`ContentBlock::Opaque { .. }` → `None` — this adapter neither produces nor
sends opaque blocks (foreign replay data is dropped, keeping cross-provider
resume safe). A message left empty after dropping opaque blocks is skipped
entirely (the API would reject an empty message).

Prompt caching marks the (stable) system prompt as an ephemeral cache
breakpoint — the highest-leverage, lowest-risk marker because it is identical
across every turn of a session:

```json
"system": [{ "type": "text", "text": "<system>", "cache_control": { "type": "ephemeral" } }]
```

`from_wire` normalizes `stop_reason` (`end_turn`/`stop_sequence` → `EndTurn`,
`tool_use` → `ToolUse`, `max_tokens` → `MaxTokens`, else `Other`), drops
`ToolResult`/`Unknown` server blocks from the response content, and errors on
a missing `stop_reason`.

## True SSE streaming (`complete_streaming`)

When `self.streaming` is false (GLM/Kimi), the adapter buffers via `complete`
and emits the whole text as one delta (the trait default behavior).

When `self.streaming` is true, it sets `stream: true`, requests
`text/event-stream`, and runs the **retry policy only for establishing the
stream** — once bytes flow it never retries mid-stream. A mid-stream failure
is abandoned, not persisted (the durability rule's "never persist partial
provider streams"). The body is read as a `bytes_stream()`; SSE frames
separated by `\n\n` are parsed inline.

`StreamAccumulator` assembles the message:

- `message_start` → input token count
- `content_block_start` → opens a `Text` or `ToolUse { id, name, json: "" }`
  block (others ignored)
- `content_block_delta` → `text_delta` is forwarded live to the `TextSink`
  **and** accumulated into the text block; `input_json_delta` is accumulated
  per-block-index as a partial-JSON string (tool-call args arrive as fragments)
- `message_delta` → `stop_reason` and `output_tokens`
- `error` → marks stop reason `error`

`finish()` parses each accumulated tool-call JSON (empty → `{}`), drops empty
text blocks, and returns the `Response`. The `is_error` flag on `ToolResult`
is elided from the wire when false.

## Retry behavior

`complete` and `complete_streaming` share the retry loop: on a retryable
status (`429`/`529`/`5xx`) with attempts remaining, sleep `retry::delay`
(honoring `Retry-After`) and retry; on a transient transport error
(timeout/connect/request) do the same. Non-retryable statuses and exhausted
attempts return `ProviderError::Api { status, message }` / `Transport`.

## Focused tests

- `request_wire_shape` — role mapping, `tool_use`/`tool_result` block types,
  `tool_use_id` carried, `is_error: false` elided, tool name present.
- `caching_marks_system_prompt` — plain system is a string; cached system is
  the array form with `cache_control: ephemeral`.
- `sse_accumulator_assembles_text_and_tool_call` — a scripted event sequence
  (message_start, two text deltas, a tool_use block with two `input_json_delta`
  fragments, message_delta with `stop_reason: tool_use`) assembles into the
  expected `Response` with the parsed `{"command": "ls"}` input.
- `glm_and_kimi_reuse_the_wire` — `Anthropic::glm` keeps `x-api-key` auth,
  `Anthropic::kimi` switches to bearer auth, both set `name()` to their vendor
  tag and `streaming`/`caching` off, and both produce the same wire body shape
  as Anthropic proper (proving compatible hosts are configuration, not code).
- Live-verification status: Anthropic is wire-level tested only (no live key
  in CI); OpenRouter and Codex are the live-verified providers
  (see [providers and auth](../concepts/providers-and-auth.md)).

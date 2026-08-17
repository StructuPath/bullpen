---
type: crate
title: bullpen-llm chat-completions adapter — OpenAI-compatible chat completions for OpenRouter
description: The OpenAI chat-completions wire adapter used for OpenRouter, mapping tool_use/tool_result to tool_calls/role:tool messages, with the openrouter/auto default model, app attribution headers, and buffered-only completion.
tags: [llm, chatcompletions, openrouter, openai, adapter]
---

# Chat-completions adapter (`bullpen-llm::chatcompletions`)

`crates/llm/src/chatcompletions.rs`. The OpenAI chat-completions wire format,
used for OpenRouter today. `OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"`;
`OPENROUTER_DEFAULT_MODEL = "openrouter/auto"` (OpenRouter's auto-router that
picks a model per request; override with `-m`).

`ChatCompletions::openrouter(key)` constructs the OpenRouter instance;
`ChatCompletions::new(name, base_url, key)` is the seam for any
chat-completions-compatible host — point `base_url` elsewhere and it works.

## Wire conversion (pure, network-free)

`to_wire(req)` builds the chat-completions body:

- A leading `role: "system"` message when `req.system` is non-empty (chat
  completions have no first-class system field).
- `Role::Assistant` messages: text blocks concatenate into `content`; each
  `ContentBlock::ToolUse` becomes a `tool_calls[]` entry of
  `{ id, type: "function", function: { name, arguments } }` where `arguments`
  is the JSON-encoded input string. `ToolResult` and `Opaque` blocks are
  dropped on this role.
- `Role::User` messages: `ContentBlock::ToolResult` blocks become
  `role: "tool"` messages (with `tool_call_id` and an `ERROR:`-prefixed
  `content` when `is_error`) emitted *before* any follow-up user text, since
  chat-completions requires a tool message to directly follow the assistant
  `tool_calls`; `ContentBlock::Text` blocks concatenate into a `role: "user"`
  message. `ToolUse`/`Opaque` blocks are dropped.
- `tools` serialized as `{ type: "function", function: { name, description, parameters } }`
  when present; `max_tokens` always set.

`from_wire` reads the first `choices[0]`:

- `message.content` (when non-empty) → `ContentBlock::Text`.
- Each `message.tool_calls[]` → `ContentBlock::ToolUse` (parsing the
  JSON-encoded `arguments`; empty arguments → `{}`).
- `stop_reason`: `saw_tool_calls` → `ToolUse`; otherwise `finish_reason`
  (`stop`/`None` → `EndTurn`, `length` → `MaxTokens`, `tool_calls` →
  `ToolUse`, else `Other`).
- `usage` from `prompt_tokens`/`completion_tokens` when present.

An empty `choices` array is `ProviderError::Malformed`.

## Request behavior

`POST {base_url}/chat/completions` with bearer auth and OpenRouter app
attribution headers (`HTTP-Referer: https://github.com/StructuPath/bullpen`,
`X-Title: bullpen`) — harmless elsewhere. Same shared retry policy as the
other adapters: retryable status (`429`/`529`/`5xx`) with attempts remaining
→ sleep `retry::delay` (honoring `Retry-After`); transient transport errors
retry; otherwise `ProviderError::Api`/`Transport`.

## Streaming status

This adapter uses the buffering `complete` path only at v0 — it does not
override `complete_streaming`, so it inherits the trait default (call
`complete`, emit the final text as one delta). Incremental streaming for
chat-completions is a fast follow that needs its own SSE delta parser; until
then the [agent loop's](agent.md) `TextSink` receives the whole text at once.

## Focused tests

Pure `to_wire`/`from_wire` conversions, no network:

- `request_wire_shape` — 5 messages: system, user, assistant(+`tool_calls`
  with JSON `arguments`), `role: "tool"` carrying `tool_call_id`, user
  follow-up; an `Opaque` block from another provider is not serialized
  anywhere; tools nest under `function`.
- `response_with_tool_calls` — `tool_calls` parse into `ContentBlock::ToolUse`
  with `stop_reason == ToolUse` and usage read.
- `response_plain_text` — `content` + `finish_reason: "stop"` → one text
  block, `EndTurn`.
- `length_finish_maps_to_max_tokens` — `finish_reason: "length"` →
  `StopReason::MaxTokens`.
- `empty_choices_malformed` — empty `choices` → `ProviderError::Malformed`.

Live verification: OpenRouter tool round-trip verified live 2026-08-07
(see [providers and auth](../concepts/providers-and-auth.md)).

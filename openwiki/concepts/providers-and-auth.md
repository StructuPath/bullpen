---
type: concept
title: Providers and auth — the five-provider selection matrix
description: How a provider is chosen at CLI build_provider time, the three wire formats (Anthropic messages, OpenAI Responses/SSE, OpenAI chat-completions) covering five providers, and the two credential shapes with their refresh semantics.
tags: [providers, auth, wire-formats, credentials, selection-matrix]
---

# Providers and auth

Adapters are organized by **wire format**, not vendor — three formats cover
every supported provider, and compatible hosts (GLM, Kimi) become
configuration, not code. The [CLI](../crates/cli.md) `build_provider` selects
the adapter and credential source at run time.

## The five providers

| Provider | Wire format | Auth | Adapter | Verified |
|---|---|---|---|---|
| `anthropic` | Anthropic messages | `ANTHROPIC_API_KEY` | [`Anthropic::new`](../crates/anthropic-adapter.md) | wire-level tests |
| `codex` | OpenAI Responses (SSE) | `bullpen login codex` (device-code) or read-only borrow of `~/.codex/auth.json` | [`Codex`](../crates/codex-adapter.md) with `StoredCodex` or `CodexCliBorrow` | live, incl. tools + resume |
| `openrouter` | OpenAI chat-completions | `bullpen login openrouter` (OAuth PKCE → API key) or `OPENROUTER_API_KEY` | [`ChatCompletions::openrouter`](../crates/chatcompletions-adapter.md) | live, incl. tools |
| `glm` | Anthropic-compatible | `GLM_API_KEY`/`ZAI_API_KEY`/`ZHIPUAI_API_KEY` | [`Anthropic::glm`](../crates/anthropic-adapter.md) | config-only |
| `kimi` | Anthropic-compatible | `KIMI_API_KEY`/`MOONSHOT_API_KEY` | [`Anthropic::kimi`](../crates/anthropic-adapter.md) | config-only |

## Credential shapes (`bullpen-auth`)

Two shapes cover every supported provider:

- `Credential::ApiKey { key }` — plain API keys (OpenRouter, and later
  GLM/Kimi/Anthropic).
- `Credential::Oauth { access_token, refresh_token, expires_at, account_id }` —
  OAuth token sets with refresh (ChatGPT/Codex).

Everything lives in `~/.bullpen/auth.json` (or `$BULLPEN_HOME/auth.json` when
set to a non-empty path), written atomically with mode 0600. See
[`AuthFile`](../crates/auth.md).

## Codex has two token sources

[`bullpen_llm::codex::TokenSource`](../crates/codex-adapter.md) is implemented
twice in [`bullpen-auth`](../crates/auth.md):

- **`StoredCodex`** — bullpen's own login. Owns its refresh token, refreshes
  proactively (within `EXPIRY_SKEW_SECS = 60` of expiry), persists rotations
  to the auth store.
- **`CodexCliBorrow`** — read-only reuse of the Codex CLI's `~/.codex/auth.json`.
  **Deliberately never refreshes** — a rotation could invalidate the CLI's own
  session — so an expired borrowed token asks the user to run either tool's
  login. `available()` checks the path exists; `jwt::expiry` gates the
  borrowed token.

## Cross-provider resume safety

`ContentBlock::Opaque { provider, data }` carries provider-specific data that
must be replayed verbatim on later turns (e.g. Codex encrypted `reasoning`
items). Tagged with the producing provider so each adapter replays only its
own opaque blocks; every other adapter skips them — which is what makes
cross-provider session resume safe. See [the Codex adapter](../crates/codex-adapter.md)
for `replayable_reasoning`.

## Codex wire-contract quirks (verified live)

- It rejects `max_output_tokens` (the adapter omits it).
- It rejects `gpt-5-codex` for ChatGPT accounts (the default comes from
  `~/.codex/config.toml` via `codex_cli_configured_model`).
- Its `response.completed` event omits `output`, so content is assembled from
  per-item `response.output_item.done` events.
- Encrypted `reasoning` items must be replayed verbatim on later turns.

See [the llm crate](../crates/llm.md) for the shared types and the
[`Provider`](../crates/llm.md) trait.

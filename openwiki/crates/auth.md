---
type: crate
title: bullpen-auth — credential acquisition, storage, and OAuth flows
description: The on-disk credential store with 0600 atomic writes, the ApiKey and Oauth credential shapes, OpenRouter OAuth PKCE, the Codex device-code flow with refresh, and the read-only Codex CLI borrow.
tags: [auth, credentials, oauth, pkce, codex, openrouter, jwt]
---

# `bullpen-auth`

`crates/auth/Cargo.toml` dependencies: `reqwest`, `serde`, `tokio`,
`async-trait`, `bullpen-llm` (for the `TokenSource` trait and `ProviderError`).
It is a leaf-ish crate that knows nothing about tools, the loop, or UI.

## Module layout

- `lib.rs` — `Credential`, `AuthFile` (the on-disk store)
- `codex.rs` — Codex device-code flow, `StoredCodex`, `CodexCliBorrow`
- `openrouter.rs` — OpenRouter OAuth PKCE flow + localhost callback
- `pkce.rs` — RFC 7636 verifier/challenge generation
- `jwt.rs` — just-enough JWT claim reading (no signature verification)

## The credential store (`lib.rs`)

`~/.bullpen/auth.json` (or `$BULLPEN_HOME/auth.json` when set to a non-empty
path). An empty `BULLPEN_HOME` counts as unset (a literal empty path would put
credentials in the process's cwd). `home_dir()` mirrors
[`bullpen_store::home_dir`](store.md).

`Credential` is a tagged enum:

```rust
pub enum Credential {
    ApiKey { key: String },
    Oauth { access_token: String, refresh_token: String,
            expires_at: u64, account_id: String },
}
```

`AuthFile::load` treats a missing file as an empty store. `set` inserts and
persists **atomically** — write to `auth.json.tmp` with mode `0600` (Unix),
`sync_all`, then rename over the target. On-disk shape is
`{ "providers": { "<name>": <Credential>, ... } }` (a `BTreeMap`, so the file
is stable across runs).

`AuthFile::default_path()` is `home_dir().join("auth.json")`. The [CLI](cli.md)
is the consumer that loads it and dispatches login flows.

## PKCE (`pkce.rs`)

RFC 7636 `Pkce::generate()` produces 32 random octets → a 43-char verifier
and its S256 challenge. `challenge_s256` is the pure derivation. Test
`rfc7636_test_vector` reproduces Appendix B of RFC 7636 exactly.

## JWT claim reading (`jwt.rs`)

No signature verification — these tokens are consumed, not trusted. `claims`
base64-decodes the payload; `expiry` extracts the `exp` unix seconds;
`chatgpt_account_id` pulls the `https://api.openai.com/auth.chatgpt_account_id`
claim required by the Codex subscription backend. `craft_unsigned` builds an
unsigned JWT for test fixtures only.

## OpenRouter OAuth PKCE (`openrouter.rs`)

OpenRouter's officially supported third-party app flow (verified 2026-08-07):
browser authorization at `https://openrouter.ai/auth`, then code-for-key
exchange at `POST /api/v1/auth/keys`. The exchange returns a plain API key —
no refresh tokens.

- `authorize_url(challenge, callback_url)` — the browser URL. With a callback,
  includes `callback_url`; without (headless), uses `key_label=bullpen` and
  shows the code on screen for pasting.
- `exchange(http, base, code, verifier)` — POST the code + verifier, extract
  `key` from the response.
- `capture_code(listener)` — serve exactly one localhost callback and return
  the `code` query value (the caller owns the listener so it can print the
  port before browsing). Browsers asking for `/favicon.ico` are ignored.

The [CLI](cli.md) `login_openrouter` wires these together, spawning the
callback listener and opening the browser (or printing the URL under
`--headless`). `open_browser(url)` (in `openrouter.rs`, reused by both login
flows) best-effort spawns `open` on macOS / `xdg-open` elsewhere.

## Codex device-code flow (`codex.rs`)

Device-code flow against `auth.openai.com` using the public Codex CLI client id
(`app_EMoamEEZ73f0CkXaXp7hrann`). `CodexAuth::login(on_code)` requests a
device code, calls `on_code` with the verification URL + user code, polls the
token endpoint (403/404 mean "not approved yet"; 15-minute timeout) until
approved, then exchanges the authorization code for tokens. `refresh` does the
refresh-token grant (a refresh response may omit a new refresh token; keep the
old one). `post_token` extracts `access_token`/`refresh_token`/`expires_in`
and pulls the `chatgpt_account_id` from the JWT.

`EXPIRY_SKEW_SECS = 60` — refresh when within a minute of expiry.

### Two `TokenSource` implementations

`StoredCodex::new(auth_path)` — bullpen's own login. `token()` loads the
store, returns the access token if not within the skew window of expiry, else
refreshes and persists the rotation. Errors point the user at
`bullpen login codex`.

`CodexCliBorrow::new(path)` — read-only reuse of the Codex CLI's
`~/.codex/auth.json` (default path `$HOME/.codex/auth.json`). `available()`
checks existence. `token()` reads `tokens.access_token` and
`tokens.account_id` (or derives the account id from the JWT), refuses empty
values, and refuses an expired token **without refreshing** — a rotation could
invalidate the Codex CLI's own session, so an expired borrow asks the user to
run either tool's login. This is a deliberate safety property.

## Focused tests

- `lib.rs` — `roundtrip_and_missing_file` (ApiKey + Oauth roundtrip through a
  reload), `store_is_owner_only` (Unix: persisted mode is `0o600`).
- `pkce.rs` — `rfc7636_test_vector` (the RFC 7636 appendix B vector) +
  `verifier_shape` (43-char verifier, S256 challenge, uniqueness).
- `jwt.rs` — `extracts_account_id_and_expiry`, `rejects_non_jwt`.
- `codex.rs` — a tiny scripted HTTP server (`serve`) routes requests by path
  substring: `full_device_flow` drives device-code → poll → exchange end to
  end and validates the credential shape; `refresh_keeps_old_token_when_omitted`
  preserves the old refresh token; `borrow_reads_fresh_cli_tokens` and
  `borrow_rejects_expired_token` cover the read-only CLI borrow; and
  `stored_source_refreshes_and_persists` covers the StoredCodex refresh+persist
  path.
- `openrouter.rs` — `authorize_url_shapes` (with/without callback), `exchange_parses_key`,
  `captures_code_and_ignores_favicon` (the localhost callback ignores
  `/favicon.ico` and returns the `code`), `denial_is_an_error` (an `error`
  query becomes `AuthError::Flow`).

See [providers and auth](../concepts/providers-and-auth.md) for how the
[CLI](cli.md) selects between these at `build_provider` time.

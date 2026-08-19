---
type: crate
title: bullpen CLI — the composition root
description: The only place that knows the full product, wiring providers, credentials, the tool registry, the store, and the agent loop; the run command, login flows, sessions, logs, and the system prompt.
tags: [cli, composition-root, run, login, sessions, provider-construction]
---

# `bullpen` CLI — the composition root

`crates/cli` (`Cargo.toml` name `bullpen`). This is the only crate that knows
the full product: it wires [providers](llm.md), [credentials](auth.md), the
[tool registry](tools.md), the [store](store.md), and the [agent loop](agent.md)
together. Core crates never reach up into here.

## Module layout

- `main.rs` — `Cli`/`Command` (clap), provider construction, login flows,
  the `run` command, `logs`, `sessions`, `TtyAsker` (the `Asker` impl)
- [`agents.rs`](cli-background.md) — the `bullpen agents` dashboard
- [`bg.rs`](cli-background.md) — background dispatch + liveness
- [`json.rs`](cli-run-json.md) — the `--json` NDJSON event stream
- [`worktree.rs`](worktree.md) — now in [`bullpen-harness`](harness.md); the
  CLI imports it as `bullpen_harness::worktree`

## Commands (`Command`)

- **`Run { prompt, provider, model, resume, verbose, json, sandbox, sandbox_strict, bg, worktree }`**
  — run one prompt headlessly and print the final answer. See the run path
  below.
- **`Agents`** — the dashboard. See [cli-background](cli-background.md).
- **`Logs { session }`** — print a background session's captured output
  (`bg::log_path`; only `--bg` sessions write a log).
- **`Login { provider: Openrouter|Codex, headless }`** — connect a provider
  account; stores credentials in `$BULLPEN_HOME/auth.json`.
- **`Sessions { json }`** — list stored sessions.

`ProviderKind` (`Anthropic | Openrouter | Codex | Glm | Kimi`) has `name()`,
`from_name()`, and `default_model()` (Anthropic → `claude-sonnet-5`,
OpenRouter → `openrouter/auto`, GLM → `glm-4.6`, Kimi → `kimi-k2-0905-preview`,
Codex → `codex_cli_configured_model()` or `DEFAULT_CODEX_MODEL`).

`codex_cli_configured_model()` reads the `model = "…"` value from
`~/.codex/config.toml` — when borrowing Codex CLI credentials, its model
choice is the one known to work with that account.

## Provider construction (`build_provider`)

`build_provider(kind) -> Arc<dyn Provider>`, reading `AuthFile::default_path()`:

- **Anthropic** — `ANTHROPIC_API_KEY` env → [`Anthropic::new`](anthropic-adapter.md).
- **OpenRouter** — `OPENROUTER_API_KEY` env (if non-empty) or the stored
  `ApiKey` → [`ChatCompletions::openrouter`](chatcompletions-adapter.md);
  else bail pointing at `bullpen login openrouter` or the env var.
- **Codex** — if the auth store has a `codex` credential, use `StoredCodex`
  (refreshes and persists); else if `CodexCliBorrow` is available
  (`~/.codex/auth.json`), use it (read-only, never refreshes) and print a
  notice; else bail pointing at `bullpen login codex` or the Codex CLI.
- **GLM** — `GLM_API_KEY`/`ZAI_API_KEY`/`ZHIPUAI_API_KEY` →
  `Anthropic::glm`.
- **Kimi** — `KIMI_API_KEY`/`MOONSHOT_API_KEY` → `Anthropic::kimi`.

`env_key(names)` returns the first non-empty value among the given env var
names.

## Login flows

- `login_openrouter(headless)` — `Pkce::generate()`, bind a localhost
  callback (or `--headless`: print the URL, paste the code), open the browser,
  `openrouter::capture_code(listener)`, `openrouter::exchange`, persist the
  `ApiKey`.
- `login_codex()` — `CodexAuth::login` with a callback that prints the
  verification URL + user code and opens the browser; persist the `Oauth`
  credential.

## The `run` path

`run(prompt, provider_kind, model, resume, verbose, json, sandbox, sandbox_strict, bg, worktree)`:

1. **Validate flags**: `--worktree` requires `--bg` and is incompatible with
   `--resume`. Record `cwd`.
2. **Background dispatch** (`bg`): resolve the repo root (if `--worktree`)
   **before** any state exists so a non-repo leaves no session/log/process;
   open the store; resolve (`--resume`) or create the session; record the
   worktree path+branch **before** `git worktree add` (a row pointing at a
   directory that failed to appear is refused on resume, while a row naming
   nothing would read as a shared-cwd session); `bg::spawn_detached`; emit
   `dispatched_json` (or print); print the worktree notice and the watch/tail/
   continue hints. Return.
3. **Foreground**: open the store; resolve (`--resume` keeps the session's
   provider/model) or create the session. `SessionWorker::acquire` +
   `start` — one process owns recovery, the in-memory transcript, and
   provider calls.
4. **Locate the run directory** via [`worktree::locate`](worktree.md): a
   recorded worktree wins over the caller's cwd; a gone-but-branch-survives
   worktree is recreated (`git worktree add --force`); a gone-and-branch-gone
   is a hard error naming both; a foreign directory at the recorded path is a
   hard error. The shared checkout is used only when no worktree was recorded.
5. **Build the sandbox** if `--sandbox`/`--sandbox-strict`: `Sandbox::strict`
   or `Sandbox::workspace`, widened with `worktree::git_write_roots(&cwd)`
   so a sandboxed agent in a worktree can still stage/commit. Print the
   non-macOS notice if `!Sandbox::os_enforced()`.
6. **Recover and rebuild** via `harness::prepare_session` (see
   [harness](harness.md)); print the recovery report if any. Read the
   session's cumulative `usage`. Drop the store handle.
7. **Build the provider** and `AgentConfig { model, system: system_prompt(cwd), ..Default::default() }`.
   `system_prompt(cwd)` is "You are bullpen, a coding agent operating in a
   repository. Working directory: {cwd}. Use the available tools…".
8. **Wire the event printer** (one task owns stdout under `--json`; verbose
   tool activity goes to stderr) and the **delta streamer** (assistant text
   to stdout as generated; dropped under `--json` but the sink stays attached
   so the agent still takes its streaming path).
9. **Build the tool ctx** (with sandbox), the `StoreJournal` (its own store
   handle; WAL-safe), and the **pen config** (with sandbox). Register
   `Registry::standard()` (bash, read/write/edit, grep, glob, ast_grep,
   ast_edit) plus the session-scoped tools: `pen.job_tool()` (the `job` tool,
   sharing the pen's cancel registry) and `pen` (the `agent` tool), then
   `TodoTool::new(Store::default_path(), &session.id)` (the `todo` tool),
   `GitHub::new()` (the `github` tool — top-level runs only), and `Ask`
   (interactive if stdin is a terminal, otherwise detached). Construct the
   `Agent` with `.with_transcript(transcript, usage)`, `.with_events(tx)`,
   `.with_delta_sink(delta_tx)`, `.with_journal(journal)`.
10. `agent.send(&prompt)`. Drop the agent (so the receiver loops can end),
    await the printer and streamer.
11. `session_worker.finish("completed"|"failed")` (records terminal state
    only for our persisted generation).
12. Under `--json`, emit the terminal `result_json` (carries the outcome
    outright). Otherwise print the session/provider/usage summary line.

`sessions_json(sessions)` is the `--json` wire shape for `sessions --json`,
owned by the CLI so the store stays free of presentation concerns (full
session ids, not the display prefix).

## `TtyAsker`

The CLI's [`bullpen_tools::Asker`](tools.md) impl for the `ask` tool: the
question goes to **stderr** (never the answer stream on stdout) and the reply
is one line from the terminal, read on `spawn_blocking` so it blocks a worker
thread, not the runtime. `Ask::interactive(Arc<TtyAsker>)` is registered when
stdin is a terminal; `Ask::detached()` otherwise (background, `--json`,
piped), which fails the call with the reason rather than blocking on input
nobody will type.

## Focused tests

- `empty_session_list_serializes_to_empty_array` /
  `json_emits_the_full_session_id_not_the_display_prefix` — the `sessions`
  JSON shape (see [cli-run-json](cli-run-json.md) for the event stream).
- `tests/background_lifecycle.rs` — see [cli-background](cli-background.md).

See [cli-background](cli-background.md) for the dashboard and background
dispatch, [cli-run-json](cli-run-json.md) for the NDJSON event stream,
[worktree](worktree.md) for per-session git worktrees, [the pen](pen.md) and
[job](job.md) and [todo](todo.md) for the session-scoped tools, and
[providers and auth](../concepts/providers-and-auth.md) for the provider/auth
selection matrix.

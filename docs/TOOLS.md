# Tools: what exists, what's next

bullpen grows its tool surface the same way it grows everything else:
durability first. A tool earns its place when it can honor the contract in
`crates/tools` — the runtime owns parallel-safety and replay-safety, effects
are idempotent on the provider-assigned `call_id`, and a crash mid-call
leaves state the next process can recover.

This document maps a best-in-class agent tool surface onto that contract:
what bullpen ships today, and in what order the rest should land.

## Shipped

| Tool | Catalog role | Notes |
|---|---|---|
| `bash` | runtime shell | Serial, sandboxable (Seatbelt on macOS), timeout-bounded. |
| `read_file` | read | One path for files, directories, SQLite databases, archives, and http(s) URLs. Files are hashline output — every line carries a `line#hash` anchor — capped head+tail; directories render sorted listings; SQLite files (detected by magic, not extension) render schema + row counts or run a read-only `query` (writes rejected by the engine via `mode=ro` + `query_only`, with an `immutable=1` fallback for live-WAL stores); zip/tar/tar.gz archives (also magic-detected) list entries or render one via `entry`, with extraction capped while streaming; URLs fetch with a streamed cap and honor the sandbox's network policy. |
| `write_file` / `edit_file` | write / edit | Sandbox write-confinement applies. `edit_file` takes exact-string replacements or hashline patches — hunks addressed by anchors, spans via `to`, with stale-anchor recovery: a moved line is followed while its content hash is unique, a changed line fails with fresh context instead of misapplying. |
| `grep` | content search | Regex over the tree, `.gitignore`-aware. |
| `glob` | path find | Pattern lookup; reach for `grep` when you need content. |
| `agent` | task fan-out | The pen: durable child sessions, deterministic ids, reattach-on-replay, durable child budget. `worktree: true` isolates a work child in its own git worktree on its own branch; `background: true` dispatches and returns immediately. Inspect, isolated, and background children all run in parallel. |
| `job` | background coordination | The coordination plane, exposed to the model: `list` children with store-derived state (status + pid liveness), `wait` polls to a terminal state and returns the child's answer, `cancel` signals a background child — which finishes as failed and stays resumable. |
| `todo` | session plan | Durable todo list in the store; replay-safe via deterministic item ids; the store enforces one item in progress at a time. |
| `ask` | follow-up questions | Transport-agnostic: interactive runs answer from the terminal, detached runs (background, `--json`, piped) fail the call with the reason instead of blocking forever. Numbered options resolve to their text. |
| `ast_grep` / `ast_edit` | structural search / rewrite | Shell out to the [ast-grep] binary, discovered at call time (`ast-grep`, then `sg`, verified by `--version`); absence is a clear error with an install hint. `ast_edit` previews the diff by default and writes only on `apply: true` — the catalog's preview-then-resolve, folded into one tool. Applies run through the sandbox's shell wrapper, like `bash`. |
| `github` | GitHub CLI ops | Raw `gh` arguments as an array, no shell in between. The runtime derives parallel safety from the verb (views/lists/diffs/checks ride together, mutations and `api` stay serial), a network-denying sandbox refuses the call, and a missing binary errors with the install + `gh auth login` hint. Registered on top-level runs only — pen children keep to the workspace. |

Two catalog entries cost nothing because they already exist under other
names: `find` is `glob`, `search` is `grep`, and `task` is the pen's
`agent` tool.

## Coordination leftovers

- **cross-process `job`** — today `cancel` reaches only background
  children dispatched by the calling process (in-process cooperative
  cancellation, so the child records its own terminal state). Signalling a
  child owned by another process is a later, deliberate step: it needs a
  protocol for the *other* process to finish its session cleanly.

## Then: files & search, deepened

- **richer `read`** — directories, URLs, SQLite, and archives are in;
  PDFs remain, an incremental, independently testable decoder behind the
  existing tool.

## Then: reaching outside the workspace

Each of these wraps a proven external surface; the work is inputs,
sandbox policy, and output discipline, not invention.

- **`web_search`** — provider-backed search (page retrieval already ships
  as URL reads). `--sandbox-strict` (network cut) must disable it cleanly,
  as it already does for URL reads.
- **`ssh`** — one remote command against a configured host; never
  implicit, always named host allowlists.

## Later: each a project of its own

Worth doing only when the layers above are solid, and each behind its own
design doc:

- **`lsp` / `debug`** — language-server navigation and DAP sessions.
  Long-lived server processes need ownership like `SessionWorker` gives
  runs: exclusive, generation-stamped, crash-detectable.
- **`eval`** — persistent Python/JavaScript cells. Kernel state is
  process state — exactly what bullpen promises survives — so cells must
  journal their inputs to be replayable into a fresh kernel.
- **`browser`** — CDP-driven tabs; the largest sandbox-policy surface.
- **memory & context** — `checkpoint` / `rewind` (transcript compaction is
  already an anticipated entry kind in the store's tree design) and a
  `retain` / `recall` memory bank.
- **media** — image inspection and generation, diagram rendering, TTS.

## The bar for a new tool

Before a tool merges it must answer, in code:

1. **What does a crash mid-call leave behind?** If the answer is "state
   the next process cannot interpret", it is not done.
2. **Is `replay_safe` honest?** Only `true` when re-execution with the
   same input and `call_id` converges — deterministic derived ids are the
   house pattern (the pen's child sessions, `todo`'s item ids).
3. **Is `parallel_safe` decided by the runtime?** Per-invocation, from the
   input, never from model self-declaration.
4. **Does the sandbox still mean something?** Write confinement and
   `--sandbox-strict` network cuts apply to the new capability or the tool
   explains, loudly, why not.

[ast-grep]: https://ast-grep.github.io

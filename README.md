<h1 align="center">bullpen</h1>

<p align="center">
  <strong>A durable agent harness in Rust.</strong><br>
  Agents as managed, resumable workers — not fire-and-forget processes.
</p>

<p align="center">
  <a href="https://github.com/StructuPath/bullpen/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/StructuPath/bullpen/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust 1.97" src="https://img.shields.io/badge/rust-1.97-orange?logo=rust&logoColor=white">
  <img alt="License MIT" src="https://img.shields.io/badge/license-MIT-blue">
  <img alt="Status v0" src="https://img.shields.io/badge/status-v0-yellow">
</p>

<p align="center">
  <img src="docs/media/bullpen-agents.gif" alt="Dispatching three agents from bullpen agents, watching them complete, peeking at one's answer, and seeing the sessions persist" width="100%">
</p>

A bullpen is a roster of warmed-up relievers you call in, pull back, and send
out again. That is the whole thesis: agent runs should survive the process
that started them.

## Why it's different

**Every step is durable the moment it happens.** Sessions live in one SQLite
database (WAL), not in the memory of whatever process launched them. Kill
bullpen mid-run — crash, `kill -9`, power loss — and the next invocation
recovers: interrupted tool calls are marked, the transcript is closed
cleanly, and the session resumes where it stopped.

**No daemon, no supervisor.** A background session is just a detached
`bullpen run` coordinating through the store. The dashboard is a read-and-
dispatch view over that store — close it and the work keeps going, because
nothing was ever supervising it.

**The sandbox is a feature, not a footnote.** `--sandbox` confines writes to
the workspace on every platform and, on macOS, runs shell commands *and their
children* under Seatbelt. `--sandbox-strict` also cuts network — including
URL fetches through the read tool.

**Edits can't land in the wrong place.** File reads are hashline: every
line carries a content-hash anchor, and patches address those anchors. An
edit against a file that drifted is detected — a moved line is followed
while its hash is unique, a changed line fails with fresh context — never
silently misapplied.

## Install

```bash
cargo install --path crates/cli
```

Rust 1.97+ (pinned in `rust-toolchain.toml`, so `rustup` fetches the right
one automatically).

## Quickstart

Point it at a provider — any one of these is enough:

```bash
export ANTHROPIC_API_KEY=sk-ant-...   # streams tokens live
bullpen login openrouter              # official OAuth PKCE
bullpen login codex                   # ChatGPT subscription, device-code flow
```

Already have the Codex CLI logged in? bullpen borrows its session from
`~/.codex/auth.json` read-only — nothing to configure, and it never
refreshes that token so it can't invalidate the other tool's login.

Then work:

```bash
cd your-project
bullpen run "find the failing test and explain why it fails"
bullpen run --sandbox "refactor the retry logic"     # confine writes
bullpen run -v "..."                                 # tool activity on stderr
bullpen run --json "..."                             # NDJSON event stream on stdout
```

Sessions are resumable by id prefix, with the provider they were created
with:

```bash
bullpen sessions                       # what have I got
bullpen sessions --json                # same, machine-readable
bullpen run -r 6ee4acc9 "now write the fix"
```

## Run many at once

```bash
bullpen run --bg "audit the auth module"   # detached, returns immediately
bullpen run --bg --worktree "refactor it"  # …in its own git worktree
bullpen agents                             # the dashboard in the GIF above
bullpen logs 6ee4acc9                      # tail a background session
```

`bullpen agents` groups sessions by state — **Working** (running, live pid),
**Failed** (running, dead pid — it crashed), **Completed**, **Idle** — and
lets you dispatch from the input line, `Space` to peek at output, `Esc` to
quit. Quitting stops nothing.

Plain `--bg` sessions share your checkout, so two of them edit the same
files. `--worktree` gives a session a git worktree of its own on a
run-unique `bullpen/<id>` branch, under `$BULLPEN_HOME/worktrees/<session>`;
the path shows up in `bullpen sessions`, in `bullpen sessions --json`, and
in the peek panel, and `bullpen run -r <id>` returns to it from anywhere. Outside a
git repository the flag fails rather than quietly sharing the checkout.
**Nothing removes a worktree or its branch** — not on success, not on
failure, not later. A worktree can hold the only copy of what an agent did,
so cleaning up is yours to decide.

## The pen

The model can delegate bounded work to child agents through the `agent`
tool: `inspect` for read-only reconnaissance, `work` for the full toolset —
optionally in the child's own git worktree (`worktree: true`), optionally
detached (`background: true`) with the `job` tool to list, wait on, and
cancel what's in flight. Children are ordinary sessions — durable,
budgeted, listed by `bullpen sessions`, resumable — with deterministic
identities, so a replayed delegation reattaches to its child instead of
running it twice.

Put together: the model can fan out several isolated work children in the
background, keep working, then join on each result — and every child stays
crash-recoverable and inspectable from the CLI the whole way.

## Tools

| Tool | What it does |
|---|---|
| `bash` | Shell in the workspace; sandboxed with its children under Seatbelt on macOS |
| `read_file` | One path for files (hashline `line#hash` anchors), directories (sorted listings), SQLite databases (schema view or read-only `query`), and http(s) URLs (streamed cap; refused when the sandbox denies network) |
| `write_file` / `edit_file` | Writes under sandbox confinement; edits by exact string or by anchored hashline patch with stale-anchor recovery |
| `grep` / `glob` | Regex content search and path patterns, `.gitignore`-aware |
| `ast_grep` / `ast_edit` | Structural search and rewrite over the syntax tree via [ast-grep](https://ast-grep.github.io) (when installed); rewrites preview by default and write only on `apply: true` |
| `github` | GitHub CLI operations with your own `gh` login (when installed) — reads run in parallel, mutations stay serial |
| `agent` | The pen: delegate to durable child agents (above) |
| `job` | The coordination plane: list children with live state, wait for a result, cancel a background child |
| `todo` | A durable session plan in the store — survives crashes and resumes; one item in progress at a time, enforced by the runtime |
| `ask` | One structured question to whoever is driving an interactive run; detached runs get the reason instead of a hang |

Parallel safety is decided per call by the runtime, never self-declared by
the model: reads, inspect children, isolated work children, and background
dispatches run concurrently; shared-checkout mutations stay serial.
[docs/TOOLS.md](docs/TOOLS.md) maps the rest of the planned surface onto
the durability contract.

## Providers

| Provider | Wire format | Auth | Verified |
|---|---|---|---|
| `anthropic` | Anthropic messages | `ANTHROPIC_API_KEY` | wire-level tests |
| `codex` | OpenAI Responses (SSE) | `bullpen login codex`, or borrow the Codex CLI | live, incl. tools + resume |
| `openrouter` | OpenAI chat-completions | `bullpen login openrouter` or `OPENROUTER_API_KEY` | live, incl. tools |
| `glm` | Anthropic-compatible | `GLM_API_KEY` | config-only |
| `kimi` | Anthropic-compatible | `KIMI_API_KEY` | config-only |

Adapters are organized by wire format rather than vendor, which is why
compatible hosts are configuration instead of code.

## Where state lives

`~/.bullpen/bullpen.db` — SQLite in WAL mode, holding an append-only entry
tree (the conversation) plus a separate execution log (the orchestration).
Delete every execution record and you still have a complete, valid
conversation.

Reading it while sessions run needs the immutable flag, since WAL databases
can't be opened read-only without their shared-memory file:

```bash
sqlite3 "file:${BULLPEN_HOME:-$HOME/.bullpen}/bullpen.db?immutable=1" "select id, status from sessions"
```

Set `BULLPEN_HOME` to move the whole directory — database, `auth.json`,
background logs, and `--worktree` checkouts (`worktrees/<session-id>`) land
directly in it, with no `.bullpen` segment appended.

## Status

**v0.** Honest about what that means:

| | |
|---|---|
| ✅ Shipped | Durable execution + crash recovery · the pen (durable subagents, worktree isolation, background dispatch + `job`) · hashline edits with anchor recovery · durable session plans (`todo`) · follow-up questions (`ask`) · write-confinement sandbox with Seatbelt on macOS · agent view (dispatch, peek, live state) · 5 providers |
| 🚧 Next | Interactive attach to a live session · needs-input state · notifications · compaction |
| 📋 Planned | Landlock confinement on Linux · a durable workflow engine (steps in SQLite, resumable from any step) |

Outside `--sandbox`, tools run with the process's full authority. Run it
somewhere you would trust the model to act.

## Design

[ARCHITECTURE.md](ARCHITECTURE.md) is the source of truth. The one-sentence
version: a policy-free core loop (`bullpen-agent`) that knows nothing about
vendors, config, or UI, with everything else composed around it at the edge
— plus a single durable store instead of per-process state.

## Develop

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

All three gate CI on Linux and macOS. The demo above is reproducible —
`docs/media/bullpen-agents.tape` drives the real binary through
[VHS](https://github.com/charmbracelet/vhs).

## License

MIT

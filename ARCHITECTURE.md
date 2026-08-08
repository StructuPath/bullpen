# Bullpen Architecture

> Status: v0 — headless vertical slice. This document is the source of truth
> for design intent; update it in the same PR as any structural change.

## What bullpen is

A durable agent harness in Rust. The name is the thesis: a bullpen is a roster
of warmed-up relievers you can call in, pull back, and send out again — agents
as managed, durable workers rather than fire-and-forget processes.

Bullpen deliberately adopts the best structural idea from
[neo](https://github.com/owainlewis/neo): a **policy-free core loop** composed
at the edge. It deliberately rejects neo's three biggest limitations, which are
bullpen's differentiators:

| # | neo limitation | bullpen answer |
|---|---|---|
| 1 | Sessions/subagent state die with the process; JSON stores lose concurrent writes | One SQLite database (WAL) for all state: sessions, agent runs, workflow steps. Cross-process safe, resumable by design |
| 2 | No sandbox; approvals are "friction, not authorization" | OS-level sandboxing (Seatbelt on macOS, Landlock on Linux) with a real capability model resolved per tool call |
| 3 | Workflow checklist is UI state only | A durable workflow engine: steps persisted, resumable, deterministic orchestration over agents |

## Crate map

Dependency direction is enforced by the workspace — a crate may only depend on
crates above it in this table:

| Crate | Owns | Must never know about |
|---|---|---|
| `bullpen-llm` | Provider-neutral conversation types, `Provider` trait, wire-format adapters (Anthropic messages, OpenAI chat-completions, Codex Responses/SSE), shared retry policy | Tools, transcripts, UI |
| `bullpen-auth` | Credential store (`~/.bullpen/auth.json`, 0600, atomic), PKCE, OpenRouter OAuth, Codex device-code flow + refresh, read-only borrow of `~/.codex/auth.json` | Tools, the loop, UI |
| `bullpen-tools` | `Tool` trait, `Registry`, built-ins (bash, read/write/edit, grep, glob), parallel-safety flags | Providers, the loop |
| `bullpen-store` | SQLite persistence: sessions, transcripts, usage; schema migrations via `user_version` | Providers, tools, the loop |
| `bullpen-agent` | The loop: transcript, provider calls, tool continuation, events, max-turns fuse, the `Journal` durability protocol (trait only) | Config files, sessions, vendors, UI, storage |
| `bullpen-harness` | Durable execution: `StoreJournal` (implements `Journal` over the store), crash recovery orchestration; future home of the pen | Vendors, UI |
| `bullpen` (cli) | Composition root: wiring, system prompt, headless `run`, `sessions` | — (the only crate that knows everything) |

The rule inherited from neo, kept absolute: **the core loop is policy-free.**
Product capabilities (skills, project context, phases, the pen, workflows,
TUI) compose around `bullpen-agent` through interfaces and events. If a
feature needs the loop changed, that's a design smell to justify explicitly.

## Providers and auth

Adapters are organized by **wire format**, not vendor — three formats cover
every supported provider, and compatible hosts (GLM, Kimi) become config, not
code:

| Provider | Wire format | Auth | Verified |
|---|---|---|---|
| `anthropic` | Anthropic messages | `ANTHROPIC_API_KEY` | wire-level tests only |
| `openrouter` | OpenAI chat-completions | `bullpen login openrouter` (official OAuth PKCE → API key) or `OPENROUTER_API_KEY` | live, incl. tool round-trip (2026-08-07) |
| `codex` | OpenAI Responses over SSE | `bullpen login codex` (device-code flow) or read-only borrow of the Codex CLI's `~/.codex/auth.json` | live, incl. tool round-trip and session resume (2026-08-07) |

Contract notes learned the hard way (all verified live against the Codex
subscription backend, 2026-08-07): it rejects `max_output_tokens`; it rejects
`gpt-5-codex` for ChatGPT accounts (default comes from `~/.codex/config.toml`
instead); its `response.completed` event omits `output`, so content is
assembled from `response.output_item.done` events; and encrypted `reasoning`
items must be replayed verbatim on later turns.

That last point is why `ContentBlock::Opaque { provider, data }` exists: each
adapter replays only its own opaque blocks and every other adapter skips them,
which is what makes cross-provider session resume safe.

The borrowed Codex credential path deliberately **never refreshes** the
token — a rotation could invalidate the Codex CLI's own session. An expired
borrow asks the user to run either tool's login. Bullpen's own
`login codex` credentials refresh proactively and persist rotations.

The `codex` and (borrowed-key) subscription paths use each vendor's
first-party client flows; whether third-party harness use stays within each
vendor's terms is the user's call, not a property this code can guarantee.

## Transcript invariants

Enforced in `bullpen-agent`, tested in its unit suite:

- Every `tool_use` gets exactly one `tool_result`, in the model's request
  order — including unknown tools, failed tools, and future
  denied/canceled/skipped calls, which produce error-shaped results.
- An assistant message and its tool results are appended together, never
  separately. The transcript is always structurally valid for the next
  provider call, even after the max-turns fuse trips.
- A `max_tokens` stop returns the partial text as a distinct error; it is
  never silently continued.
- Tool results are capped (256 KiB) before entering the transcript.

## Event model

The loop emits `Event` values (assistant text, tool start/end, turn done) on
an optional channel. Rendering lives entirely outside the loop — the headless
CLI prints them; the future TUI consumes the same stream. A gone subscriber
never stops the loop.

## Persistence and durable execution

One database: `~/.bullpen/bullpen.db`, WAL mode, `busy_timeout` set, schema
versioned by `pragma user_version`. Session ids resolve by unique prefix.

The durable-execution design below is adapted from pi's `harness-v2.md`
design spec (earendil-works/pi, `packages/agent/docs/`), simplified to
bullpen's current scope. Credit where due: the durability rule, reduction
idea, and recovery discipline are theirs; the Rust realization and the
narrower cut are ours. Note harness-v2 is a spec pi has only partially
implemented — bullpen is a first production test of these ideas, so each
piece gets verified here, not assumed.

### Two kinds of state

**Entries** are the conversation: an append-only tree per session
(`id`/`parent_id`), where each session lane has a leaf pointer. Entries are
what the model sees; rebuilding provider context walks leaf → root. The
tree never contains orchestration state.

**Records** are execution: a flat per-session log (`operation_started`,
`step_attempt`, `tool_started`, `operation_finished`) that nothing reads
during normal execution. Records never enter model context. Deleting every
record leaves a complete, valid conversation.

Both share one per-session monotonic `seq`, allocated inside the storage
write — callers never see or pass sequence numbers or parent ids.

### The durability rule

> Before an effect: write an intent record naming what will happen and the
> ids it will produce. After the effect: append the result as an entry with
> exactly those ids.

There is **no multi-row atomicity and none is needed** — every record and
entry is durable alone. An intent is fulfilled iff an entry with its
provisioned id exists. Appends are idempotent (`append-if-missing` on the
provisioned id), so re-running recovery is always safe.

The run protocol, as the `Journal` trait in `bullpen-agent`:

```text
run_started        op record + user entry
  step_attempt     durable attempt counter (a crash-restart loop cannot reset it)
  assistant_message  entry
  tool_batch       one intent record per call, all sharing one provisioned
                   results-entry id + a snapshot of each tool's replay safety
  tool_results     the provisioned grouped results entry
run_finished       op record (completed | failed | truncated | aborted)
```

A journal write failure fails the run — durability is not best-effort.
Results are grouped in one entry per batch (matching the in-memory message
shape); the cost is that a crash mid-batch loses the whole batch's results
to synthesis. Acceptable at serial tool execution; revisit with parallel
scheduling.

### Reduction and recovery

A session's execution state is **defined** as the reduction of its records:
no open `operation_started` → idle; exactly one → suspended; two or more →
corruption (single-writer states only; the reducer rejects, never repairs).

Recovery (run automatically before using a session with an open operation)
reads the open run's records and the entry path back to the run's source
leaf, then: appends synthetic `interrupted` tool results for unfulfilled
intents (grouped entry, provisioned id), appends a closing assistant entry
if the leaf would otherwise end on a user message, writes
`operation_finished(aborted)`, and clears the lane's open-operation marker.
Conservative v1 choice: replay-safe tools are *not* re-executed on
recovery; everything unfulfilled synthesizes as interrupted. The
declaration is snapshotted in the intent record now so re-execution can be
added without a schema change.

### Rules carried forward (from harness-v2, enforced here as we grow)

- **Append-only context**: provider context only grows at the tail within
  a lane; inserting earlier silently invalidates provider KV cache.
  Steering and deferred writes (M2+) must consume at checkpoints, never
  splice.
- **Never persist partial provider streams** — retry or abandon.
- **Deterministic child ids** for the pen: a subagent's session id derives
  from `f(parent_session, tool_call_id)`, so a replayed spawn reattaches
  instead of spawning a twin.
- **Lanes**: schema carries a lane name (default `main`) from day one;
  multi-lane execution arrives with the pen.
- Usage stays a side channel (session totals today, a ledger later);
  reduction and recovery never read it.

## Roadmap

Milestones, in order. Each lands as its own crate or a bounded extension:

- ~~**M1.5 — Auth + multi-provider.**~~ Landed 2026-08-07: OpenRouter (OAuth
  PKCE) and Codex (device-code + CLI borrow) alongside Anthropic. Remaining
  from this line: GLM/Kimi as config-only Anthropic-compatible endpoints, and
  a stored-key path for Anthropic itself.
- ~~**M1 — Streaming + prompt caching.**~~ Landed 2026-08-07. `Provider`
  grows `complete_streaming(req, &TextSink)` with a buffering default, so
  every provider streams *something*; the Anthropic adapter implements true
  SSE streaming (assistant text forwarded live, tool-call JSON accumulated
  per block and parsed at `content_block_stop`, never persisting a partial
  stream — a mid-stream failure is abandoned, not retried). Assistant text
  streams to a `TextSink` distinct from the `Event` stream, so the CLI
  prints tokens to stdout live while tool activity goes to stderr. Prompt
  caching marks the system prompt as an ephemeral breakpoint. **Still
  buffered**: Codex and OpenRouter (chat-completions) use the default —
  incremental streaming for them is a fast follow (each needs its own SSE
  delta parser); GLM/Kimi buffer until their SSE shape is confirmed.
- **M2 — The pen (durable subagents).** SHIPPED 2026-08-07 (foundation +
  pen same day). The `agent` tool spawns children as sessions in the same
  store: deterministic ids (`uuidv5(parent, tool_call_id)`) make replayed
  spawns *reattach* — a completed child returns its recorded answer with no
  provider call, an interrupted child is recovered and continued. Modes:
  `inspect` (read-only tools) and `work` (full registry); never a nested
  pen. Budgets are durable: child count is a database count (default 20), a
  crash-restart loop cannot reset it; 15-minute wall clock per child run.
  Children appear in `bullpen sessions` (`└ child of …`) and resume with
  `run -r` like any session.

  **Parallel scheduling** (landed same day): within one assistant response,
  maximal *adjacent* runs of parallel-safe tool calls execute concurrently
  under a semaphore (default 8); results always enter the transcript in
  request order. `Tool::parallel_safe` judges the concrete input — the
  runtime owns the decision, never the model. Read/grep/glob are
  parallel-safe; bash and writes are serial; the `agent` tool is
  parallel-safe only in `inspect` mode. Durability is unchanged by
  scheduling: the whole batch's intents are journaled before any execution,
  and a crash mid-batch synthesizes the whole batch on recovery. Still open
  for M2.x: live child event streaming to the parent's UI, token budgets
  (needs the usage ledger).
- **M3 — Sandbox + permissions.** Landed 2026-08-07 as write-confinement.
  `bullpen-sandbox` enforces the workspace boundary two ways together: an
  in-process `allows_write` check the file tools consult (all platforms,
  catches `..` and symlink escapes by resolving to real paths), and, on
  macOS, a generated Seatbelt (`sandbox-exec`) profile that confines shell
  commands *and their children* to the workspace + system temp, optionally
  denying network (`--sandbox` / `--sandbox-strict`). Scope is deliberate:
  it is write-confinement, not a full jail — reads stay broad because
  toolchains legitimately read across the system. Applies to the top-level
  agent and to pen work-children. Live-verified: a shell write to `$HOME`
  under `--sandbox` is denied by the OS while a workspace write succeeds.
  Still open: Landlock for Linux shell confinement (the in-process check
  already works there); a persisted per-tool authorization model beyond the
  workspace boundary.
- **M4 — TUI / agent view.** Stage 1 landed 2026-08-07: `bullpen agents`, a
  daemonless dashboard for background sessions. Unlike Claude Code's agent
  view (which needs a supervisor process because its sessions live in
  memory), bullpen's sessions already live in the WAL store with durable
  execution — so a background session is just a detached `bullpen run`
  coordinating through SQLite, and the `ratatui` dashboard is a read view
  over the store plus process-liveness checks. `bullpen run --bg` dispatches
  a detached session; the dashboard groups sessions by derived state
  (Working = running + live pid, Failed = running + dead pid i.e. crashed,
  Completed, Idle), dispatches from an input line, and peeks a session's
  latest output. `bullpen logs <id>` tails captured output. Stage 2:
  interactive attach to a *live* process (needs a per-session control
  socket), needs-input state (needs the approvals feature), notifications.
  Also here eventually: the full transcript/tool-card/pen-tree view over the
  event stream.
- **M5 — Workflow engine.** Durable steps in SQLite; deterministic
  orchestration (sequence/fan-out/join) over pen agents; resume from any
  step. This is the layer neo has only as UI state.
- **Ongoing.** Compaction (M1.5, before long sessions hurt); project
  context discovery (AGENTS.md); skills; second provider adapter when
  genuinely needed, not before.

## Resource bounds (v0)

| Resource | Bound |
|---|---|
| Provider turns per send | 500 (fuse, not budget) |
| Tool result in transcript | 256 KiB |
| bash timeout | 120 s default, 600 s max |
| bash output | 100 KB (head+tail truncation) |
| read_file | 256 KiB |
| grep/glob results | 200 |
| Provider retries | 5 attempts, exponential backoff, honors Retry-After |

## Security posture (v0 — honest version)

Until M3 lands, bullpen has the same posture as neo: tools run with the
process's full authority; run it somewhere you trust the model to act.
The difference is intent — sandboxing is a roadmap differentiator here, not a
documented non-goal. Do not point v0 at anything you wouldn't hand to a
contractor's laptop.

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
| `bullpen-llm` | Provider-neutral conversation types, `Provider` trait, vendor adapters (Anthropic today), shared retry policy | Tools, transcripts, UI |
| `bullpen-tools` | `Tool` trait, `Registry`, built-ins (bash, read/write/edit, grep, glob), parallel-safety flags | Providers, the loop |
| `bullpen-store` | SQLite persistence: sessions, transcripts, usage; schema migrations via `user_version` | Providers, tools, the loop |
| `bullpen-agent` | The loop: transcript, provider calls, tool continuation, events, max-turns fuse | Config files, sessions, vendors, UI |
| `bullpen` (cli) | Composition root: wiring, system prompt, headless `run`, `sessions` | — (the only crate that knows everything) |

The rule inherited from neo, kept absolute: **the core loop is policy-free.**
Product capabilities (skills, project context, phases, the pen, workflows,
TUI) compose around `bullpen-agent` through interfaces and events. If a
feature needs the loop changed, that's a design smell to justify explicitly.

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

## Persistence

One database: `~/.bullpen/bullpen.db`, WAL mode, `busy_timeout` set,
schema versioned by `pragma user_version`. Transcript saves are
transactional replace-all; the CLI saves after every turn **including failed
ones**, so a session is always resumable. Session ids resolve by unique
prefix.

## Roadmap

Milestones, in order. Each lands as its own crate or a bounded extension:

- **M1 — Streaming + prompt caching.** SSE streaming in the Anthropic
  adapter; cache-control breakpoints on the system block. Event stream grows
  text deltas.
- **M2 — The pen (durable subagents).** `pen` crate: supervisor with budgets
  (count, wall-clock, tokens), each child a fresh `Agent` persisted as an
  `agent_runs` row + own transcript. Children survive process exit and are
  resumable/attachable. `agent` tool exposed to the coordinator. Parallel
  scheduling of read-only children.
- **M3 — Sandbox + permissions.** `sandbox` crate: capability grants (fs
  read/write path sets, network, process spawn) resolved per tool call;
  Seatbelt profile generation on macOS, Landlock on Linux. Approvals become
  persisted authorization decisions, not UI friction.
- **M4 — TUI.** `ratatui` front-end over the same event stream: transcript,
  tool cards, pen tree, steering, approvals.
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

---
title: "Handoff: Agent View Stage 2 durable inbox"
type: handoff
date: 2026-08-12
status: ready
next_todo: TODO-c13875c2
---

# Agent View Stage 2 handoff

## Fresh-session prompt

> Read `docs/plans/2026-08-12-002-agent-view-stage2-handoff.md`, claim
> `TODO-c13875c2`, and implement only the durable session inbox foundation in
> a new isolated worktree. Keep SQLite authoritative, verify every crash
> boundary, run the full Rust checks, and obtain an independent review. Do not
> modify or commit the primary checkout's untracked `env.` file.

## Current state

Repository: `/Users/vics/dev/structupath/bullpen`

Agent View Stage 2's execution-safety foundation shipped in:

- `b4a1cb7 feat(agent-view): enforce exclusive session workers`
- pushed to `origin/main`
- CI passed on Linux and macOS, including fmt and clippy:
  <https://github.com/StructuPath/bullpen/actions/runs/31571278777>

A subsequent local documentation commit preserves the resource-table spacing:

- `9ec4d2a docs: preserve resource table separator spacing`

This handoff is committed after that documentation change. At handoff time,
`env.` is the only intentionally untracked file in the primary checkout. Do not
stage, delete, or alter it.

The old implementation worktree remains at
`/private/tmp/bullpen-agent-view-stage2-foundation`. Do not reuse it. Create a
new worktree from current `main` for the inbox task.

## What is implemented

Bullpen already has:

- durable SQLite WAL sessions and crash recovery;
- detached `bullpen run --bg` workers;
- `bullpen agents` dispatch/state/peek dashboard;
- background logs and optional isolated git worktrees;
- durable pen child sessions;
- exclusive per-session worker ownership shared by CLI runs and pen children;
- schema v7 `worker_generation` tokens for stale-safe lifecycle writes.

The latest foundation guarantees:

1. A crash-released lock at `$BULLPEN_HOME/run/<session-id>.lock` prevents two
   provider loops from operating one persisted session.
2. Top-level runs and pen children acquire the same lock before recovery or
   provider activity.
3. Only the worker writes `running` and terminal state. Detached parents do not,
   so a fast child cannot be overwritten back to Working.
4. Terminal updates are conditional on the persisted worker generation.
5. Concurrent first-open schema migrations recheck the version under an
   IMMEDIATE transaction.

Primary implementation files:

- `crates/store/src/worker.rs`
- `crates/store/src/lib.rs`
- `crates/cli/src/main.rs`
- `crates/harness/src/pen.rs`
- `crates/cli/tests/background_lifecycle.rs`

Verification for `b4a1cb7`:

- `cargo fmt --all --check` passed
- `cargo clippy --workspace --all-targets -- -D warnings` passed
- `cargo test --workspace` passed: 139 passed, 0 failed, 1 ignored helper
- independent review found no blocker/high/medium correctness issues

## Architectural decisions

Bullpen stays daemonless. SQLite is the durable coordination and truth plane.
A future local socket may improve wakeup or token streaming, but accepted input
must never exist only in IPC memory.

For the next stages:

- a reply received during active work queues for the next turn;
- do not implement steering or cancellation yet;
- attach is a disposable UI over durable entries and execution records;
- detach closes the UI only and never stops the worker;
- do not build a supervisor, idle worker pool, or PTY proxy.

## Next task

Claim **TODO-c13875c2 — Agent View Stage 2: durable session inbox foundation**.

Implement only the schema/store/journal primitive. Do not change CLI or TUI
behavior in this slice.

### Required behavior

1. Add schema v8 durable session inputs.
2. Preserve deterministic per-session ordering.
3. Accept caller-provisioned input IDs so enqueue retries are idempotent.
4. Enqueue under an IMMEDIATE transaction; avoid deferred read-to-write
   upgrades and `SQLITE_BUSY_SNAPSHOT`.
5. Atomically start the oldest pending input by performing all of these in one
   transaction:
   - mark the input started;
   - open its operation;
   - append exactly one user message;
   - associate the input with the operation.
6. A crash before that transaction leaves the input pending.
7. A crash after it uses existing operation recovery and never replays or
   re-appends the prompt as a new input.
8. Existing direct foreground/background execution remains unchanged.

### Critical seam

`StoreJournal::run_started` in `crates/harness/src/lib.rs` currently calls
`start_operation` and `append_entry` separately. Do not wrap an inbox claim
around those separate transactions. Add one store-level atomic operation and
wire `StoreJournal` to an optional input identity—or introduce an equally clean
boundary—while preserving the ordinary direct-run path.

### Schema considerations

Account explicitly for:

- provisioned input/idempotency ID;
- session ID;
- transactionally allocated per-session position;
- prompt payload;
- pending/started state;
- linked operation ID;
- timestamps needed for inspection.

Do not calculate `MAX(position) + 1` outside the same IMMEDIATE transaction.

### Required tests

- duplicate provisioned ID enqueues once;
- concurrent enqueue writers receive unique deterministic ordering;
- pending input survives store close/reopen;
- oldest pending input starts first;
- start creates exactly one operation and one user entry atomically;
- failure before start leaves input pending;
- recovery after start never duplicates the prompt;
- direct `StoreJournal` behavior is unchanged;
- v7→v8 migration works;
- concurrent first-open migration remains safe.

### Out of scope

Do not pull forward:

- worker drain loop or `ensure_worker`;
- attach UI or peek replies;
- sockets or token-level streaming;
- approvals or Needs Input;
- notifications;
- stop/delete/restart;
- summaries or PR integration.

## Sequence after the inbox

1. Worker loop: enqueue prompts, ensure one worker, drain serially, and close
   the enqueue-versus-idle-exit race.
2. Headless attach: render durable state, enqueue replies, detach safely.
3. Dashboard attach and peek replies using the same enqueue API.
4. Optional authenticated per-session socket for transient streaming/wakeup.
5. Durable approval requests and Needs Input state.
6. Deduplicated notification outbox.

## Completion standard

- Work in a new isolated git worktree based on current `main`.
- Keep one writer and use a read-only independent reviewer.
- Run:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
- Confirm the primary checkout's `env.` remains untouched.
- Commit only the inbox foundation.
- Do not merge or push unless the user explicitly requests it.

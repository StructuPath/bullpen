---
type: crate
title: the job tool — the coordination plane for pen children
description: The JobTool exposing list/wait/cancel over a session's pen children, deriving state from the store plus process liveness (for_session + pid_alive), wait polling to a terminal state, and in-process cooperative cancellation via the shared Cancels registry.
tags: [job, coordination, pen-children, cancel, liveness, store-derived-state]
---

# The `job` tool (`bullpen-harness::job`)

`crates/harness/src/job.rs`. `bullpen agents` reads sessions from the store and
never talks to the processes running them; `job` gives the model the same
read-and-signal view over its own [pen](pen.md) children. The store is the
source of truth, so a `job` call after a crash sees reality, not a stale
in-process handle.

`JobTool` is built by [`PenTool::job_tool`](pen.md), sharing the pen's
`Cancels` registry (`Arc<Mutex<HashMap<child_id, oneshot::Sender<()>>>>`) so
background children dispatched by this process can be cancelled. A child's
entry in `cancels` exists exactly while its task runs here; children run by
other processes are never in it. The [CLI](cli.md) registers `pen.job_tool()`
alongside the pen.

## `name() = "job"`

`spec` advertises `list`, `wait`, and `cancel`. Children are addressed by the
id prefix `list` shows (8-char prefix). `parallel_safe(input)`: `list` and
`wait` only read the store, so several waits can block side by side — that is
how a fan-out joins; `cancel` signals a running child and stays serial.

## State derivation

`derived(session)` = `for_session(session, session.pid.is_some_and(pid_alive))`
— the same [`AgentStatus`](store.md) the [agents dashboard](cli-background.md)
derives, using `bullpen_store::status::pid_alive` (moved here from `cli/bg.rs`
so the tool and the dashboard share one liveness check). `for_session` maps the
stored `status` + liveness bit to `Working`/`Completed`/`Failed`/`Idle`: a
session marked `running` whose pid is gone is `Failed` (crashed), which is
different from one that finished cleanly.

`render(children)` lists each child with its 8-char id, derived status label,
title, and a `· worktree` marker when the child has a `worktree_path`.

## `wait`

`resolve(store, prefix)` — prefix resolution mirroring session/todo resolution
(zero → `NotFound`, two+ → `Ambiguous`). Polls `store.get_session(child.id)`
every `POLL_INTERVAL` (250 ms) until the deadline
(`timeout_seconds`, default `DEFAULT_WAIT_SECS = 900`, capped at
`MAX_WAIT_SECS = 3600`):

- `Completed` → `answer_of(store, child.id)` (the last assistant message on the
  child's path) + `[child {short} · completed]`.
- `Failed` → "child {short} failed or was interrupted; its session is saved —
  `agent` with the same task continues it".
- `Working`/`Idle` → keep polling (`Idle` covers the dispatch race: created but
  not yet started; the deadline bounds it).
- Deadline → `ToolError::Timeout(timeout)`.

## `cancel`

Resolves the child by prefix, then looks in `cancels`:

- **Found** (`Some(cancel)`) — send the signal. A dropped receiver (the child
  just finished on its own) is a success for `cancel` too. Returns "cancel
  signalled for child {short}; its session is saved and resumable". The child
  finishes as `failed` and stays resumable — cancellation is an outcome, not
  an erasure.
- **Not found** (`None`) — the child was not dispatched by this process. If
  `Working`, error naming the other process's pid ("running in another
  process; this session did not dispatch it, so cancel it there"); otherwise
  `InvalidInput` ("not running").

Cross-process `cancel` is a deliberate non-goal: today `cancel` reaches only
background children dispatched by the calling process (in-process cooperative
cancellation, so the child records its own terminal state). Signalling a child
owned by another process needs a protocol for the *other* process to finish
its session cleanly.

## Focused tests

`crates/harness/src/job.rs` test suite uses a `FakeProvider` and a
`HangingProvider` (never resolves) for liveness/cancellation:

- `empty_list_and_bad_inputs` — empty session lists "No children dispatched";
  missing `action`, missing `id`, unknown action → `InvalidInput`.
- `list_shows_the_completed_child_and_wait_returns_its_answer` — a completed
  child lists as `Completed`; `wait` returns its answer immediately; a
  finished child cannot be cancelled ("not running").
- `a_crashed_child_reads_as_failed_not_working` — a fabricated crashed child
  (status `running`, unreachable pid) lists as `Failed`; `wait` errors with
  "failed or was interrupted"; `prepare_session` then recovers it.
- `reads_are_parallel_safe_cancel_is_not` — `list` and `wait` are
  parallel-safe; `cancel` is not.

See [the pen](pen.md) for the child dispatch protocol and the `Cancels`
registry, and [the store](store.md) for `for_session`/`pid_alive` and the
`AgentStatus` derivation the tool shares with the [agents dashboard](cli-background.md).

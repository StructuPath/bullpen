---
type: crate
title: bullpen-harness — durable execution composition of the loop and the store
description: The StoreJournal implementing the agent Journal over SQLite, prepare_session recovering then rebuilding the transcript, the PenTool durable subagents (worktree isolation + background dispatch), the job coordination tool, the todo durable session plan, and worktree placement.
tags: [harness, storejournal, durable-execution, pen, job, todo, worktree, prepare-session]
---

# `bullpen-harness`

`crates/harness/Cargo.toml` dependencies: `bullpen-agent`, `bullpen-store`,
`bullpen-llm`, `bullpen-tools`, `bullpen-sandbox`, `anyhow`, `async-trait`,
`tokio`, `uuid`, `serde_json`, `tempfile` (dev). It knows nothing about vendors
or UI.

This crate is where durable execution is composed: the agent loop's
[`Journal`](agent.md) protocol implemented over the SQLite [store](store.md).
It also hosts the session-scoped tools that need the store: the [pen](pen.md)
(durable subagents), [job](job.md) (the coordination plane), [todo](todo.md)
(the durable session plan), and [worktree](worktree.md) placement.

## Module layout

- `lib.rs` — `StoreJournal`, `prepare_session`; re-exports `JobTool`,
  `PenConfig`/`PenTool`, `TodoTool`, and the `worktree` module
- [`pen.rs`](pen.md) — the `agent` tool (durable subagents, worktree
  isolation, background dispatch + cancellation)
- [`job.rs`](job.md) — the `job` tool (list/wait/cancel over pen children)
- [`todo.rs`](todo.md) — the `todo` tool (durable session plan in the store)
- [`worktree.rs`](worktree.md) — per-session git worktree placement (moved
  here from `crates/cli` so the pen can place work children)
- `testutil.rs` (test-only) — shared `FakeProvider`/`response`/`text_response`
  + `HangingProvider` (never resolves) for cancellation/liveness tests

## `prepare_session`

```rust
pub fn prepare_session(store, session_id) -> Result<(Vec<Message>, Option<Recovery>), StoreError>
```

The front door. It calls [`recover(store, session_id)`](recovery.md) (crash
recovery — synthesizes interrupted results, closes the transcript, finishes
the operation as aborted) then `Store::path_messages(session_id)` to rebuild
the transcript from the durable entry tree. Returns the messages and the
optional `Recovery` report. The [CLI](cli.md) calls this after acquiring a
`SessionWorker`, seeding the `Agent` with `.with_transcript(messages, usage)`.

## `StoreJournal`

```rust
pub struct StoreJournal {
    store: std::sync::Mutex<Store>,
    session_id: String,
    run_id: Option<String>,
    pending_results_entry: Option<String>,  // allocated at intent time
}
```

`Mutex<Store>` is never contended: every method is `&mut self`, so `store()`
uses `get_mut()` (lock-free). `jerr(StoreError)` flattens to `JournalError`.
Owns the store for the duration of a run; the [CLI](cli.md) opens its own
`Store` handle for the journal (WAL makes the two connections safe).

Implements the [agent `Journal`](agent.md) protocol against the store:

- `run_started(user)` → `Store::start_operation` (the run id is the
  `operation_started` record's id), `append_entry` the user message.
- `step_attempt(n)` → `append_record("{run_id}:step:{n}", "step_attempt",
  {attempt: n})`.
- `assistant_message(m)` → `append_entry` with a fresh v4 id.
- `tool_batch(intents)` → allocate one `results_entry_id` (v4), then for each
  intent `append_record("{run_id}:tool:{tool_use_id}", "tool_started",
  {tool_use_id, name, input, replay_safe, results_entry_id})`. The
  deterministic intent id makes a retried batch write idempotent. Stash the
  results_entry_id.
- `tool_results(results)` → `append_entry` at the stashed
  `pending_results_entry` id (errors if no pending batch). This is the
  grouped results entry; recovery pairs intents to it.
- `run_finished(outcome, usage)` → `Store::finish_operation(run_id,
  outcome.as_str())`, then `path_messages` → `title_from` and
  `update_session_meta(usage, title)` to roll usage and the first-user-message
  title forward, then clear `run_id`.

The whole protocol honors the durability rule: intent records before effects,
results at provisioned ids, everything idempotent. A journal write failure
fails the run.

## The pen (subagents) — see [the pen](pen.md)

`PenTool` implements [`bullpen_tools::Tool`](tools.md) as the `agent` tool. It
spawns children as ordinary sessions in the same store with deterministic ids
(`uuidv5(parent, tool_call_id)`), so a replayed spawn reattaches. `PenConfig`
carries the store path, workspace, provider/model, system prompt, and the
durable budgets (`max_children`, `child_timeout`, `child_max_turns`). Work
children can be isolated (`worktree: true`) or dispatched to the background
(`background: true`); background children are coordinated through the
[`job` tool](job.md) via the shared `Cancels` registry. See the
[pen page](pen.md) for the full protocol and [job](job.md) for the coordination
plane.

## The session-scoped tools

- [`PenTool`](pen.md) — the `agent` tool; `PenTool::job_tool()` returns the
  [`JobTool`](job.md) sharing the pen's cancel registry.
- [`JobTool`](job.md) — `list`/`wait`/`cancel` over the session's pen children,
  deriving state from the store + `pid_alive`.
- [`TodoTool`](todo.md) — the `todo` tool; a durable plan in the store with
  deterministic item ids and the one-active-item invariant.
- [`worktree`](worktree.md) — per-session git worktree placement, used by the
  pen (`place_child`) and the [CLI](cli.md) run path.

## Focused tests

`lib.rs` test suite uses a `FakeProvider` and an `Echo` tool, building the
agent with `agent_with(store, session_id, provider)` (a `StoreJournal`).

- `full_run_is_durable_and_resumable` — a tool round-trip then a text answer:
  the durable transcript equals the in-memory one, the operation is closed,
  usage rolled up (`output_tokens == 10`, two steps × 5), the title is the
  first user message ("go"), and a follow-up run seeded from the durable
  transcript works (6 messages after the second run).
- `provider_failure_leaves_recoverable_state` — a provider error mid-run:
  `run_finished(Failed)` closed the operation; recovery finds nothing left but
  the transcript never ends on a dangling user message (preparation appends a
  closing note when it would).
- `simulated_crash_mid_batch_recovers` — a `CrashAfterIntents` journal that
  errors on `tool_results` and `run_finished` (simulating a killed process):
  the operation stays open; preparation recovers it (synthetic interrupted
  result paired to the intent, closing assistant note, operation closed).

The pen tests live in [pen.md](pen.md).

---
type: crate
title: bullpen-store recovery — synthesizing interrupted results and closing a crashed run
description: The recover() procedure that reads an open run's records, appends synthetic interrupted tool results at provisioned ids, closes the transcript, and finishes the operation as aborted — idempotent and safe to re-run.
tags: [store, recovery, durable-execution, crash-recovery, interrupted]
---

# Recovery (`bullpen-store::recovery`)

`crates/store/src/recovery.rs`. `recover(store, session_id) ->
Result<Option<Recovery>, StoreError>` is the procedure that makes a session
usable again after a previous process died mid-run. It is called automatically
by `harness::prepare_session` before a session is used (see
[durable execution](../architecture/durable-execution.md)).

## What recovery does

1. Calls `Store::open_run(session_id)` — the [reducer](store.md). If no run is
   open, returns `Ok(None)` (nothing to recover).
2. Groups the open run's `tool_started` records by their shared
   `results_entry_id` (the provisioned id allocated at intent time).
3. For each group whose `results_entry_id` is **not** already present in
   `entries` (the intent was not fulfilled — results landed before the crash),
   appends a synthetic grouped tool-results entry at that exact provisioned id,
   with one `ContentBlock::ToolResult { tool_use_id, content: "interrupted:
   bullpen exited before this tool finished", is_error: true }` per intent.
4. If the transcript would otherwise end on a user message (a prompt or the
   just-synthesized results), appends a closing assistant note (`"[this run was
   interrupted and recovered; state above may be incomplete]"`) so the next
   run never produces two consecutive user turns.
5. Calls `finish_operation(session_id, run_id, "aborted")` to close the
   operation and clear the lane's open-operation marker.

`Recovery { run_id, interrupted_tools, closed_with_note }` reports what
happened, surfaced to the user by the [CLI](cli.md).

## Why this is safe to re-run

Every append is idempotent on its provisioned id: the synthetic
tool-results entry uses the intent's `results_entry_id`; the closing assistant
entry uses `format!("{run_id}:closing")`; `finish_operation` is idempotent.
So re-running recovery after a crash *during* recovery is safe — the
append-if-missing semantics mean each step is a no-op on the second pass.

## The conservative v1 cut

Replay-safe tools are **not** re-executed on recovery; their intents
synthesize as interrupted like everything else. The replay declaration is
already snapshotted in each `tool_started` record's `replay_safe` field, so
re-execution can be added later without a schema change. See [durable
execution](../architecture/durable-execution.md) for the rule and the
revisit point.

## Focused tests

`crashed_mid_batch` builds the durable state of a run that crashed mid-tool-batch:
a session, an `operation_started`, a user entry, an assistant entry with a
`ToolUse`, and a `tool_started` record pointing at an unfulfilled
`results_entry_id`. Then:

- `recovers_crashed_batch_with_synthetic_results` — `recover` returns
  `Recovery { interrupted_tools: 1, closed_with_note: true }`; the transcript
  has 4 messages (user, assistant tool_use, synthetic interrupted tool_result,
  closing assistant note); the `ToolResult` is paired to the right `tool_use_id`
  and `is_error`; `open_run` is `None` afterward; a new `start_operation`
  succeeds (the session is usable again).
- `fulfilled_intents_are_left_alone` — when the results entry for an intent
  already exists (it landed before the crash), `recover` does not overwrite it
  with a synthetic result; real results survive recovery.
- `recovery_is_idempotent` — re-running `recover` after a (simulated) crash
  during recovery produces the same state: appends are `append-if-missing` on
  the provisioned id and `finish_operation` is idempotent, so the second pass
  appends nothing and closes nothing new.

`closed_with_note` is decided from the **role of the last path message**: if
`path_messages` ends on a `Role::User` message (a prompt or a synthesized
results batch), a closing `Role::Assistant` note is appended at
`{run_id}:closing`; otherwise nothing is appended. The rule is that the
transcript must never end on a user message, because the next run would then
produce two consecutive user turns.
- The harness-level `simulated_crash_mid_batch_recovers` (see [harness](harness.md))
  drives the same path through the agent loop with a `CrashAfterIntents` journal
  that fails on `tool_results` and `run_finished`, then asserts recovery
  synthesizes the result and the operation closes.

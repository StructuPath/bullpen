---
type: crate
title: the pen — durable subagents via the `agent` tool
description: The PenTool spawning children as ordinary sessions with deterministic uuidv5 ids so replays reattach, durable child-count budgets, inspect/work modes, the reattach shortcut for completed children, and recovery+continuation for interrupted ones.
tags: [pen, subagents, deterministic-ids, reattach, child-budget, durable-execution]
---

# The pen (`bullpen-harness::pen`)

`crates/harness/src/pen.rs`. The `agent` tool: delegate a bounded task to a
durable child agent. A child is an ordinary session in the same store, linked
to its parent and named **deterministically** from the invocation. That single
choice does most of the work.

## The deterministic-id rule

```rust
pub fn child_session_id(parent_session, tool_call_id) -> String {
    uuidv5(NAMESPACE_OID, format!("bullpen:{parent_session}:{tool_call_id}"))
}
```

Same parent + same tool call → same child id. Consequences:

- A **replayed spawn** (same tool call) *reattaches* to the same child instead
  of spawning a twin — a completed child returns its recorded answer with no
  provider call; an interrupted child is recovered and continued.
- Children survive process exit and are inspectable with `bullpen sessions`
  (`└ child of …`) and resumable with `bullpen run -r` like any session.

## `PenConfig`

```rust
pub struct PenConfig {
    pub store_path, workspace, provider_name, model, system,
    pub max_children: u64,           // default 20
    pub child_timeout: Duration,     // default 900s (15 min wall clock)
    pub child_max_turns: u32,        // default 200
    pub sandbox: Option<Arc<Sandbox>>,
}
```

`PenConfig::new(store_path, workspace, provider_name, model, system)`;
`.with_sandbox(sandbox)` applies write-confinement to work children (inspect
children are read-only regardless). The [CLI](cli.md) builds the pen config
from the session's provider/model/system and the optional sandbox, and
registers `PenTool` in the registry alongside the standard tools.

## The `agent` tool

`name() = "agent"`. `spec` advertises modes `inspect` (default — read-only
tools `read_file`/`grep`/`glob`) and `work` (full workspace tools — still no
nested pen). Required input: `prompt` (a complete, self-contained task — the
child cannot see the parent's conversation).

`parallel_safe(input)`: inspect children are read-only in the workspace and
each writes only to its own child session (WAL handles store contention), so
they can run alongside each other; work children mutate the workspace and
stay serial. This is what lets the [agent loop](agent.md) run adjacent
inspect children concurrently.

## `run(ctx, call_id, input)`

1. Parse `prompt` and `mode` (default `inspect`); `registry_for_mode(mode)`
   builds the registry (`inspect` → read-only; `work` → `Registry::standard()`;
   unknown → `InvalidInput`).
2. `child_id = child_session_id(parent_session, call_id)`.
3. **Budget** (new children only): if the child doesn't exist and
   `Store::count_children(parent) >= max_children`, fail with "child budget
   exhausted". The count is a database count, so a crash-restart loop cannot
   reset it. Reattaching an existing child is always allowed.
4. `Store::create_child_session(child_id, parent, cwd, provider, model)` —
   idempotent.
5. **Reattach shortcut**: if the child already exists and its
   `last_run_outcome == "completed"`, return its recorded final assistant
   message (`"{answer}\n\n[child {short} · reattached to completed run]"`)
   with **no provider call**.
6. Fresh or interrupted child: `SessionWorker::acquire` the same exclusive
   ownership used by top-level CLI runs (prevents a manual
   `bullpen run -r <child>` from racing the pen), `start(&mut store)`.
7. [`prepare_session`](harness.md) (recover if interrupted, rebuild
   transcript). If the transcript is empty, the task is the prompt; otherwise
   it's "The previous attempt above was interrupted. Continue and complete the
   original task, then give your final report."
8. Build the child system prompt (base + the relief-agent role text; inspect
   adds "You have read-only tools."), construct a `StoreJournal` + `Agent`
   with `.with_transcript(transcript, child.usage)`, and run
   `tokio::time::timeout(child_timeout, agent.send(&task))`.
9. Outcome: `Ok(Ok(answer))` → report with usage and a `· recovered` marker
   if recovery ran; `Ok(Err(e))` → "child {short} failed: … (session is
   saved and resumable)"; `Err(_)` (timeout) → "child {short} timed out …
   its session is saved and recoverable — calling agent again with the same
   task will continue it". `session_worker.finish("completed"|"failed")`.

## Focused tests

- `inspect_children_are_parallel_safe_work_children_are_not` — `inspect` and
  default mode are parallel-safe; `work` is not.
- `inspect_mode_has_no_bash` — an `inspect` child that calls `bash` gets an
  "unknown tool" error result rather than execution (`bash` is absent from the
  inspect registry), and the child continues to recover.
- `child_ids_are_deterministic` — same (parent, call) → same id; changing
  either → different id.
- `child_runs_and_is_durably_linked` — a child run produces the answer,
  `count_children == 1`, the child row has the right `parent_session_id`,
  `status == "completed"`, `pid == None`, and
  `last_run_outcome == "completed"`.
- `a_cli_owned_child_cannot_also_run_in_the_pen` — a held `SessionWorker` on
  the child id makes a pen run fail with "already has a running worker";
  dropping the owner lets the pen proceed. This is the single-ownership
  invariant (see [worker](worker.md)).
- `replay_reattaches_without_provider_calls` — after a first run, a second pen
  call with an **empty** provider script returns the recorded answer
  ("reattached") with no provider call; `count_children` stays 1.
- `child_budget_is_durable` — `max_children = 1`: a first child runs; a *new
  pen instance* (fresh process) sees the budget spent and fails the second
  spawn with "budget exhausted".
- `interrupted_child_is_recovered_and_continued` — a fabricated crashed child
  (open operation, user entry) is recovered by `prepare_session` and
  continued with the relief-agent continuation prompt, producing the answer.

See [durable execution](../architecture/durable-execution.md) for the
deterministic-child-id rule and [the harness](harness.md) for the surrounding
`StoreJournal` composition.

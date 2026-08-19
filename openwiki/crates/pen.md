---
type: crate
title: the pen — durable subagents via the `agent` tool
description: The PenTool spawning children as ordinary sessions with deterministic uuidv5 ids so replays reattach, durable child-count budgets, inspect/work modes, worktree isolation and background dispatch with the job coordination tool, cancellation, and the place_child worktree resolver.
tags: [pen, subagents, deterministic-ids, reattach, child-budget, worktree, background, job, cancel, durable-execution]
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
registers `PenTool` plus its `job_tool()` in the registry alongside the
standard tools and the [todo](todo.md) tool.

## The `agent` tool

`name() = "agent"`. `spec` advertises modes `inspect` (default — read-only
tools `read_file`/`grep`/`glob`/`ast_grep`) and `work` (full workspace tools —
still no nested pen). Required input: `prompt` (a complete, self-contained task
— the child cannot see the parent's conversation). Two flags extend dispatch:

- `worktree: true` (work mode only) — run the child in its own git worktree on
  a `bullpen/<child-id>` branch, so isolated work children can run in
  parallel without editing each other's files.
- `background: true` — dispatch the child and return immediately; the child
  runs in this process, coordinated through the store where the
  [job tool](job.md) finds it. Inspect, isolated work, and background children
  are all parallel-safe.

`parallel_safe(input)`: inspect children are read-only in the workspace and
each writes only to its own child session (WAL handles store contention), so
they can run alongside each other; isolated work children each mutate only
their own worktree; background dispatches only spawn and return. A work child
in the shared checkout mutates the workspace and stays serial. This is what
lets the [agent loop](agent.md) run adjacent inspect/isolated/background
children concurrently.

## `run(ctx, call_id, input)`

1. Parse `prompt`, `mode` (default `inspect`), `worktree`, `background`.
   `worktree` with a non-`work` mode → `InvalidInput` ("worktree isolation is
   for `work` children"). `registry_for_mode(mode)` builds the registry
   (`inspect` → read-only + `AstGrep`; `work` → `Registry::standard()`; unknown
   → `InvalidInput`).
2. `child_id = child_session_id(parent_session, call_id)`. If the child is
   already in flight in *this process* (`cancels`), report and return — use
   the [job tool](job.md) to wait or cancel.
3. Open the store. **Budget** (new children only): if the child doesn't exist
   and `Store::count_children(parent) >= max_children`, fail with "child budget
   exhausted". The count is a database count, so a crash-restart loop cannot
   reset it. Reattaching an existing child is always allowed.
4. `Store::create_child_session(child_id, parent, cwd, provider, model)` —
   idempotent.
5. **Reattach shortcut**: if the child already exists and its
   `last_run_outcome == "completed"`, return its recorded final assistant
   message (`"{answer}\n\n[child {short} · reattached to completed run]"`)
   with **no provider call**.
6. `place_child(store, child, use_worktree)` — resolves where the child runs.
   First worktree dispatch records the path+branch **before** creating the
   tree (a failed creation leaves a row naming a missing directory, which
   resume refuses); after that the recorded state decides via
   [`worktree::locate`](worktree.md) — a gone-but-branch-survives worktree is
   recreated, a gone-and-branch-gone is a hard error.
7. **Background dispatch** (`background`): create a `oneshot` cancel channel,
   register it in `cancels` keyed by `child_id`, `tokio::spawn` the child run,
   and return immediately with "dispatched child {short} in the background
   (mode {mode}{· isolated worktree}); use the job tool to list, wait on, or
   cancel it". The spawned task removes its entry from `cancels` when it ends.
8. **Foreground**: `run_child(spec, None)` — see below.

## `run_child(spec, cancel)` — one child's outcome

`ChildSpec` owns everything a child run needs (provider, config, child_id,
child_cwd, registry, mode, prompt, usage) so a background dispatch can move it
into its task. The flow:

1. `SessionWorker::acquire` + `start` — the same exclusive ownership used by
   top-level [CLI](cli.md) runs (prevents a manual `bullpen run -r <child>`
   from racing the pen).
2. [`prepare_session`](harness.md) (recover if interrupted, rebuild transcript).
   If the transcript is empty, the task is the prompt; otherwise it's "The
   previous attempt above was interrupted. Continue and complete the original
   task, then give your final report."
3. Build the child system prompt (base + the relief-agent role text; inspect
   adds "You have read-only tools.").
4. **Sandbox for an isolated child**: rebased onto the child's worktree (same
   network policy), widened with `worktree::git_write_roots(&child_cwd)` so a
   linked worktree can stage/commit; a shared-checkout child inherits the
   parent's sandbox as-is.
5. `StoreJournal` + `Agent` with `.with_transcript(transcript, child.usage)`,
   run `tokio::time::timeout(child_timeout, agent.send(&task))`. A background
   child selects on the cancel channel too: `Ran::Cancelled` resolves the run
   early (the child finishes as failed and stays resumable — cancellation is an
   outcome, not an erasure).
6. Outcome: `Ok(Ok(answer))` → report with usage and a `· recovered` marker if
   recovery ran; `Ok(Err(e))` → "child {short} failed: … (session is saved and
   resumable)"; `Err(_)` (timeout) → "child {short} timed out … its session is
   saved and recoverable"; `Cancelled` → "child {short} was cancelled; its
   session is saved and resumable". `session_worker.finish("completed"|"failed")`.

## The `job` tool and cancellation

`PenTool::job_tool()` returns a [`JobTool`](job.md) sharing the pen's
`Cancels` registry (`Arc<Mutex<HashMap<child_id, oneshot::Sender<()>>>>`). A
background child's entry exists exactly while its task runs in this process;
children run by other processes are never in it. The [CLI](cli.md) registers
`pen.job_tool()` so the model can `list` children with live state, `wait` for
a result, or `cancel` a background child — all reading the store as the source
of truth, so a `job` call after a crash sees reality, not a stale in-process
handle.

## Focused tests

- `inspect_children_are_parallel_safe_work_children_are_not` — `inspect` and
  default mode are parallel-safe; `work` is not (pre-worktree version).
- `isolated_work_children_are_parallel_safe` — a `work` + `worktree: true`
  child is parallel-safe; `background: true` is parallel-safe.
- `inspect_mode_has_no_bash` — an `inspect` child that calls `bash` gets an
  "unknown tool" error result rather than execution, and the child continues.
- `child_ids_are_deterministic` — same (parent, call) → same id; changing
  either → different id.
- `child_runs_and_is_durably_linked` — a child run produces the answer,
  `count_children == 1`, the child row has the right `parent_session_id`,
  `status == "completed"`, `pid == None`, `last_run_outcome == "completed"`.
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
- `a_background_child_is_cancelled_and_stays_resumable` (job tests) — a
  background child with a `HangingProvider` is cancelled via the job tool,
  finishes as failed, and `prepare_session` recovers it.

See [durable execution](../architecture/durable-execution.md) for the
deterministic-child-id rule, [the harness](harness.md) for the surrounding
`StoreJournal` composition, [the job tool](job.md) for the coordination plane,
and [worktree](worktree.md) for worktree placement.

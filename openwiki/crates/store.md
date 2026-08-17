---
type: crate
title: bullpen-store — SQLite persistence of sessions, the entry tree, and the execution record log
description: The single SQLite WAL database holding sessions, the append-only entry tree (the conversation), the flat record log (the orchestration), schema migrations via user_version, session/entry/record APIs, the reducer, and worker generation ownership.
tags: [store, sqlite, persistence, schema, entries, records, worker-generation]
---

# `bullpen-store`

`crates/store/Cargo.toml` dependencies: `bullpen-llm` (for `Message`/`Role`/
`Usage`), `rusqlite` (bundled), `serde`, `serde_json`, `uuid`, `fs2` (file
locks), `tempfile` (dev). It knows nothing about providers, tools, or the
loop.

## Module layout

- `lib.rs` — `Store`, `Session`, `Entry`, `Record`, schema migrations,
  session/entry/record APIs, the reducer
- [`recovery.rs`](recovery.md) — `recover()` (crash recovery)
- `status.rs` — `AgentStatus` (derived dashboard state)
- `worker.rs` — `SessionWorker` (exclusive ownership + generation)

## One database, two kinds of state

`~/.bullpen/bullpen.db` (WAL mode, `busy_timeout` 10 s, `foreign_keys` ON),
schema versioned by `pragma user_version` (v7). `BULLPEN_HOME` overrides the
directory (see `home_dir`/`resolve_home` — an empty `BULLPEN_HOME` counts as
unset). `Store::default_path()` is `home_dir().join("bullpen.db")`.

The two kinds of per-session state share one monotonic `seq`, allocated inside
the storage write by `next_seq` (an `UPDATE … RETURNING` on
`sessions.next_seq`). Callers never see or pass sequence numbers or parent ids.

**Entries** (`entries` table) — the conversation: an append-only tree
(`id`/`parent_id`) per session with a per-lane leaf pointer (`lanes.leaf_id`).
`kind` is `"message"` today; future kinds (compaction) carry their own
payloads. `payload` holds a serialized `Message`.

**Records** (`records` table) — the execution log: a flat per-session log
(`operation_started`, `step_attempt`, `tool_started`,
`operation_finished`) keyed by `run_id` (the `operation_started` record's own
id). Nothing reads it during normal execution; it never enters model context.
Deleting every record leaves a complete, valid conversation.

Both have `UNIQUE (session_id, seq)` and are idempotent on their caller-provided
`id` (see [durable execution](../architecture/durable-execution.md) for why).

## Schema migrations (`migrate`)

`migrate()` reads `user_version`, rechecks inside an `IMMEDIATE` transaction
(two first-opens can otherwise race a stale migration decision — busy_timeout
cannot repair that), and applies each version's batch guarded by
`if version < N`. The history:

- v1 — `sessions` + `messages` (flat list)
- v2 — `sessions.provider` column
- v3 — `entries`/`records`/`lanes` tables; `migrate_messages_to_entries` turns
  each session's flat message list into an entry chain and points the lane
  leaf at the last one; `messages` dropped
- v4 — `sessions.parent_session_id` (pen children)
- v5 — `sessions.status`, `sessions.pid` (background worker state)
- v6 — `sessions.worktree_path`, `sessions.worktree_branch`
- v7 — `sessions.worker_generation` (transient ownership token)

Migration tests build old-shape databases by hand (`migrates_v2_sessions_into_entry_tree`,
`migrates_v5_sessions_adding_worktree_columns`, `migrates_v6_sessions_adding_worker_generation`)
and assert `Store::open` upgrades them into the current shape.
`concurrent_first_open_serializes_the_v7_migration` opens the same pre-v7
database from two threads and asserts exactly one migration writes and both
see `worker_generation` afterward — the `IMMEDIATE` recheck serializes them.

## `Session` row

`id`, `title` (first 80 chars of the first user message), `cwd` (the directory
the run was dispatched from — still points at the repository when the worktree
is gone), `provider`, `model`, `usage`, timestamps, `parent_session_id` (pen
children), `status` (`idle`/`running`/`completed`/`failed`), `pid` (for
liveness), `worktree_path`/`worktree_branch` (NULL for a shared-cwd session).

## Sessions API

- `create_session(cwd, provider, model)` — new v4 uuid, inserts a `main` lane.
- `create_child_session(child_id, parent_id, cwd, provider, model)` —
  idempotent: if the id exists, returns the existing session. This is how a
  replayed pen spawn reattaches to its child (see [the pen](pen.md)).
- `resolve_session(prefix)` — unique-prefix resolution (`LIKE prefix || '%'`,
  `LIMIT 2`); `NotFound`/`Ambiguous` errors.
- `list_sessions()` — `ORDER BY updated_at DESC`.
- `count_children(parent_id)` — the durable child-count budget.
- `update_session_meta(usage, title)` — rolls usage forward and sets the title
  if empty (first-user-message wins).
- `set_worktree(path, branch)` — records the worktree location **before** it is
  created, so a failed creation leaves a row naming a missing directory (which
  resume refuses) rather than a row naming nothing (which resume would treat
  as a shared-cwd session).

## Worker lifecycle and generation

- `start_worker(session_id, pid) -> generation` — sets `status='running'`,
  `pid`, a fresh v4 `worker_generation`, in an `IMMEDIATE` tx. The generation
  makes terminal updates conditional.
- `finish_worker(session_id, generation, status) -> bool` — updates only if
  `worker_generation` matches; returns `false` for a stale generation without
  mutating. This is what makes a stale worker unable to overwrite a newer
  worker's state even if lifecycle code is accidentally reordered.

`SessionWorker` ([worker.rs](worker.md)) wraps the crash-released OS file lock
around these. The generation makes terminal status stale-safe.

## Entries API

- `append_entry(session_id, provisioned_id, kind, payload)` — takes an
  `IMMEDIATE` transaction up front so a concurrent writer makes us wait
  (honoring `busy_timeout`) rather than failing with `SQLITE_BUSY_SNAPSHOT`
  when a deferred read-then-write upgrade races. It is `append-if-missing` on
  the id, the parent is the current lane leaf (callers never pass a parent),
  and it advances the leaf. Idempotent.
- `leaf(session_id)` — the lane's leaf id.
- `entry_exists(id)` — used by recovery to check whether an intent's
  `results_entry_id` was fulfilled.
- `path(session_id)` — the active path root → leaf (walks `parent_id` from the
  leaf; errors if the leaf references a missing entry).
- `path_messages(session_id)` — the active path as `Message`s (`kind ==
  "message"` only); this is what rebuilds the transcript for a provider call.

## Records API and the reducer

- `append_record(session_id, id, run_id, kind, payload)` — idempotent on `id`.
- `start_operation(session_id, payload) -> run_id` — refuses if an operation
  is already open (returns `Corrupt("cannot start an operation while one is
  open (recover first)")`); inserts an
  `operation_started` record whose id *is* the run id, snapshots
  `source_leaf_id` into the payload, sets the lane's `open_operation_id`.
- `finish_operation(session_id, run_id, outcome)` — idempotent; inserts
  `operation_finished` (`{run_id}:finished`), clears the marker.
- `open_run(session_id) -> Option<OpenRun>` — the **reducer**: counts
  `operation_started` records with no matching `operation_finished`. 0 →
  `None` (idle); 1 → `Some(OpenRun { run_id, source_leaf_id, records })`; ≥2 →
  `Corrupt` (single-writer states only; the reducer rejects, never repairs).
- `last_run_outcome(session_id)` — the most recent `operation_finished`
  payload's `outcome`; used by the pen to recognize a completed child for
  reattachment.

## Derived dashboard state (`status.rs`)

`AgentStatus` is `Idle | Working | Completed | Failed`. The raw `status` column
records what the run *intended* (running → completed/failed); combined with
process liveness, that tells the whole story: a session marked `running` whose
process is gone crashed and is recoverable, which is different from one that
finished cleanly. `derive(status, alive)` is the pure function:
`"running" + alive → Working`, `"running" + !alive → Failed` (crashed),
`"completed" → Completed`, `"failed" → Failed`, else `Idle`. `group_rank()`
orders the dashboard (Working, Idle, Failed, Completed).

`title_from(messages)` extracts the first user message's text (first 80 chars)
for the session title.

## Focused tests

- `unset_bullpen_home_still_resolves_under_dot_pillpen` /
  `bullpen_home_overrides_the_default_directory` — `resolve_home` semantics
  (empty `BULLPEN_HOME` is unset).
- `entry_chain_roundtrip` — append two entries, `leaf` is the last, `path_messages`
  returns them in order.
- `provisioned_append_is_idempotent` — appending the same entry/record id
  twice writes one row.
- `operation_lifecycle_and_reduction` — `open_run` is `None` initially;
  `start_operation` makes it `Some`; a second start while open is refused;
  `finish_operation` returns to `None`; finishing again is idempotent.
- `seq_is_shared_and_monotonic_across_tables` — entries and records draw from
  one shared `seq` (entry seqs `[1, 3]`, record seq `2`).
- The three migration tests noted above.

See [durable execution](../architecture/durable-execution.md) for the protocol
and [recovery](recovery.md) for the procedure that consumes the reducer.

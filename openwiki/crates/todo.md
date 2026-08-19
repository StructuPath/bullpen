---
type: crate
title: the todo tool — a durable session plan in the store
description: The TodoTool backing a session's todo list in the SQLite store with deterministic uuidv5 item ids so replays converge, the store-enforced one-active-item invariant, and add/start/done/remove/list actions that each return the current plan.
tags: [todo, durable-plan, session-plan, deterministic-ids, replay-safe, store]
---

# The `todo` tool (`bullpen-harness::todo`)

`crates/harness/src/todo.rs`. A durable session plan: a todo list that lives
in the [store](store.md), not in the model's context window — it survives
crashes, resumes, and compaction like everything else in the session. Two
choices carry the design:

- **Deterministic item ids** (`uuidv5(session, call_id, index)`) make a
  replayed `add` converge on the same rows instead of duplicating them, so the
  whole tool is `replay_safe`.
- **The store owns the one-active-item invariant**: marking an item
  `in_progress` returns any other active item to `pending`. The model cannot
  talk itself into three parallel "current" tasks.

Every action returns the rendered plan, so the model always acts on the
current state rather than its memory of it. The [CLI](cli.md) registers
`TodoTool::new(Store::default_path(), &session.id)` for every run.

## `name() = "todo"`

`spec` advertises `add` (append `items`), `start` (mark one `in_progress`),
`done` (complete one), `remove` (drop one), `list` (show the plan). Items are
addressed by the id prefix shown in the list (8-char prefix).
`parallel_safe(input)`: only `list` (reading the plan rides alongside
anything; mutations keep serial ordering so a batch applies in the order the
model issued it). `replay_safe()` true — adds converge via deterministic ids;
status changes and removes are idempotent by construction.

## `todo_id` — deterministic identity

```rust
pub fn todo_id(session_id, call_id, index) -> String {
    uuidv5(NAMESPACE_OID, format!("bullpen-todo:{session_id}:{call_id}:{index}"))
}
```

Same session + same tool call + same index → same todo id. This is what makes
a replayed `add` reattach instead of duplicate — the `INSERT ... SELECT
COALESCE(MAX(position), 0) + 1` in `Store::add_todo` is `append-if-missing` on
the id, so a replayed add with the same id is a no-op.

## `run(ctx, call_id, input)`

`TodoTool` opens `Store::open(&self.store_path)` directly (its own WAL-safe
handle) and dispatches on `action`:

- `add` — requires a non-empty `items` array; for each `(index, content)`,
  `todo_id(session, call_id, index)` → `Store::add_todo(session, id, content)`.
- `start`/`done` — requires an `id` prefix; `Store::set_todo_status(session,
  prefix, "in_progress"|"completed")` (the store returns any other active item
  to `pending`).
- `remove` — requires an `id` prefix; `Store::remove_todo(session, prefix)`.
- `list` — no store mutation; `Store::list_todos(session)`.

`render(todos)` → "Plan (N of M done):" with `[ ]`/`[>]`/`[x]` marks and
8-char id prefixes. Store errors map through `terr`: `NotFound`/`Ambiguous`
become `InvalidInput` (the model can act on the message); other errors become
`Failed`.

See [the store](store.md) for the `todos` table (schema v8), the one-active
item invariant, and prefix resolution, and [the pen](pen.md) for the other
session-scoped tool that uses deterministic ids for replay safety.

---
type: operations
title: Where state lives — bullpen home and the SQLite store
description: The single durable store at $BULLPEN_HOME/bullpen.db in WAL mode, the auth.json credential file, the logs and worktrees directories, and how to read a running database with the immutable flag.
tags: [operations, state, bullpе-home, sqlite, wal, layout]
---

# Where state lives

`~/.bullpen/` by default, or `$BULLPEN_HOME` when set to a non-empty path.
`bullpen_store::home_dir` and `bullpen_auth::home_dir` both resolve it the
same way (empty `BULLPEN_HOME` counts as unset — taken literally it would put
the whole store in whatever directory the process happened to start in).

## Layout

| Path | Contents | Owner |
|---|---|---|
| `bullpen.db` | SQLite in WAL mode: the entry tree (conversation), the record log (execution), sessions, lanes. Schema versioned by `pragma user_version` (v7). | [store](../crates/store.md) |
| `auth.json` | Credential store, 0600, atomic writes. | [auth](../crates/auth.md) |
| `logs/<id>.log` | A background session's captured stdout+stderr. | [bg](../crates/cli-background.md) |
| `worktrees/<id>/` | A `--bg --worktree` session's isolated checkout (never removed automatically). | [worktree](../crates/worktree.md) |
| `run/<id>.lock` | The OS file lock giving a session a single owner. | [worker](../crates/worker.md) |

`BULLPEN_HOME` moves the whole directory — database, `auth.json`, background
logs, and worktree checkouts land directly in it, with no `.bullpen` segment
appended. The store's `next_seq`, `worker_generation`, lane leaf pointers, and
`open_operation_id` marker all live in `bullpen.db`.

## Reading a running database

WAL databases can't be opened read-only without their shared-memory file, so
reading the store while sessions run needs the immutable flag:

```bash
sqlite3 "file:${BULLPEN_HOME:-$HOME/.bullpen}/bullpen.db?immutable=1" \
  "select id, status from sessions"
```

See [the store](../crates/store.md) for the schema and
[durable execution](../architecture/durable-execution.md) for why one database
holds both the conversation and the execution log.

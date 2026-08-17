---
type: crate
title: bullpen-store worker — exclusive session ownership via an OS file lock and a stale-safe generation
description: The SessionWorker that acquires a crash-released OS file lock to single-own a session, starts a persisted generation, and finishes conditionally so a stale worker can never overwrite a newer one.
tags: [store, worker, file-lock, generation, ownership]
---

# Session worker (`bullpen-store::worker`)

`crates/store/src/worker.rs`. SQLite serializes writes, but it cannot stop two
agent processes from building divergent in-memory transcripts and calling
providers for the same session. `SessionWorker` provides that single-owner
invariant with two mechanisms together: a crash-released OS file lock, and a
persisted generation that makes terminal status updates stale-safe.

## `SessionWorker`

```rust
pub struct SessionWorker {
    lock: File,
    db_path: PathBuf,
    session_id: String,
    generation: Option<String>,
}
```

### `acquire(db_path, session_id)`

Creates `$BULLPEN_HOME/run/` (restricted via `restrict_directory`), opens
`run/<session_id>.lock` (mode 0600, created 0600), and
`try_lock_exclusive()`s it. `WouldBlock` →
`WorkerError::AlreadyRunning(short_id)`; other I/O → `Io`. Writes the current
pid into the lock file as **informational only** — the OS lock, not this text,
is ownership. The lock is automatically released when the process exits
(crash or clean), which is the whole point: a crashed process releases
ownership without a cleanup path.

### `start(store)`

Calls `Store::start_worker(session_id, pid)`, which sets `status='running'`,
`pid`, and a fresh v4 `worker_generation` in an `IMMEDIATE` transaction, and
returns the generation. `start` must be called exactly once (`AlreadyStarted`
if called twice).

### `finish(status)`

Opens its own store connection, calls `Store::finish_worker(session_id,
generation, status)`, which updates only if `worker_generation` matches —
returns `false` (→ `WorkerError::StaleGeneration`) for a stale generation
without mutating. This is what makes a stale worker unable to overwrite a
newer worker's state even if lifecycle code is accidentally reordered around
process startup.

### `Drop`

If a generation is still held (the worker was dropped without `finish`),
opens a store and records `failed` for that generation (best-effort), then
unlocks. So a panic or early return marks the session `failed` rather than
leaving it `running` forever.

## Used by

- The [CLI](cli.md) `run` path — acquires a worker before recovery/provider
  activity so any later setup error is recorded as a failed run by the guard.
- [The pen](pen.md) — each child acquires the same `SessionWorker` before
  recovery or provider activity, preventing a manual `bullpen run -r <child>`
  from racing the pen. The `a_cli_owned_child_cannot_also_run_in_the_pen` test
  verifies this: a held lock makes a pen run fail with "already has a running
  worker"; releasing it lets the pen proceed.

## Focused tests

`worker.rs` unit tests:

- `process_exit_releases_the_worker_lock` — a helper subprocess acquires the
  lock and exits; the parent then acquires it, proving the OS releases the
  lock on process death (the property that makes ownership crash-safe).
- `stale_worker_cannot_overwrite_the_current_generation` — after a `start`
  writes a generation, a second `start` produces a newer generation, and a
  `finish` with the *old* generation returns `false` and leaves the row on the
  new generation's state.

The pen tests in [harness](harness.md) exercise the worker through the pen
path. The background lifecycle integration test
(see [cli background](cli-background.md)) verifies a fast worker failure
reaches the terminal `failed` state rather than being left `running`.

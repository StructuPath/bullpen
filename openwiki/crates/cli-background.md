---
type: crate
title: bullpen CLI background dispatch and the agents dashboard
description: The daemonless background model — a detached bullpen run --resume coordinating through the store, the ratatui dashboard as a read+dispatch view, liveness via kill(pid,0), and the integration test asserting pre-spawn failure leaves sessions idle and fast worker failure is terminal.
tags: [cli, background, dashboard, ratatui, daemonless, liveness]
---

# Background dispatch + the agents dashboard

`crates/cli/src/bg.rs` and `crates/cli/src/agents.rs`. Bullpen has no
supervisor daemon. A background session is just a detached `bullpen run
--resume` process that coordinates through the shared SQLite [store](store.md)
(WAL-safe across processes). The dashboard reads the store; it never talks to
these processes directly — which is why they survive it closing.

## `bg.rs`

- `log_path(session_id)` → `$BULLPEN_HOME/logs/<id>.log` (where a background
  session's stdout/stderr is captured).
- `pid_alive` — re-exported from [`bullpen_store::status`](store.md); `kill(pid, 0)`;
  success or `EPERM` both mean the process exists, `pid <= 0` → false. Lives in
  the store so the agents dashboard (below) and the [job tool](job.md) share one
  check.
- `spawn_detached(session_id, prompt, extra_args) -> io::Result<u32>` — creates
  the logs dir, opens the log as stdout+stderr, re-execs
  `bullpen run --resume <id> <prompt>` with the `extra_args` (e.g.
  `--sandbox`), and on Unix calls `setsid()` in `pre_exec` so a closing
  terminal (SIGHUP) doesn't take the background run down with it. Returns the
  child pid.

## `agents.rs` — the dashboard

A `ratatui` TUI. `run()` opens the store, inits the terminal, runs the event
loop, restores the terminal.

`Row { session, status }`. `arrange(rows)` is pure: sort by
`status.group_rank()` then `updated_at` descending (Working first, then Idle,
Failed, Completed — see [store status](store.md)). `load_rows(store)` pairs
each session with its derived `AgentStatus` (using `bg::pid_alive` for the
liveness bit).

`App { rows, selected, input, peek, error }`. The event loop polls crossterm
every 250 ms and refreshes the store every 1 s. Keybindings:

- `Ctrl-C` / `Esc` (when not peeking and input empty) — quit (stops nothing).
- `Esc` while peeking — close peek; `Esc` while input non-empty — clear input.
- `↑`/`↓` — select; `Space` (input empty) — toggle peek.
- `Enter` — dispatch if the prompt is ≥4 chars; else an error.
- `Backspace`/`Char(c)` — edit the input line.

`dispatch(app, store, prompt)`: creates a session with `store.create_session`
and `bg::spawn_detached`. **Behavioral boundary**: `dispatch` hardcodes the
`codex` provider and `default_codex_model()` (Codex-configured or
`DEFAULT_CODEX_MODEL`) — there is no provider flag in the dashboard, so a
dashboard-dispatched session is always a Codex session, unlike
`bullpen run -p <provider>`.

Rendering: a header (session count, working count), the grouped list
(8-char id, provider, usage, 48-char title, `↳` for children, `›` for the
selected row), an input box, and a centered peek panel (`draw_peek`) that
reads the latest non-empty assistant `Message::text()` from the durable
transcript via a fresh store connection.

## Focused tests

`crates/cli/tests/background_lifecycle.rs` is the integration test. It runs
the real `bullpen` binary (via `CARGO_BIN_EXE_bullpen`) with `BULLPEN_HOME` and
`HOME` set to a tempdir and provider env vars removed.

- `pre_spawn_failure_never_marks_the_session_running` — putting a file at
  `logs` (forcing `spawn_detached` to fail at `create_dir_all`) → the
  dispatch command exits non-zero and the session row stays `idle` with null
  pid. A non-repo `--worktree` dispatch follows the same rule at a different
  step (see [worktree](worktree.md)).
- `a_fast_worker_failure_is_terminal_not_left_running` — a `--bg --json`
  dispatch (which will fail before a provider request because no provider is
  configured) returns a `dispatched` object; the test polls `sessions --json`
  until the row reaches `failed` with null pid within 5 s (killing the worker
  and panicking if it doesn't).

See [the CLI run path](cli.md) for where background dispatch is decided, and
[the store](store.md) for the `status`/`pid` columns the dashboard derives
from.

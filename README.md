# bullpen

A durable agent harness in Rust. A bullpen is a roster of warmed-up relievers
you call in, pull back, and send out again — agents as managed, durable
workers, not fire-and-forget processes.

**Status: v0.** Headless vertical slice — a working agent loop (Anthropic,
six workspace tools, SQLite-backed resumable sessions). TUI, durable
subagents, OS sandboxing, and the workflow engine are on the
[roadmap](ARCHITECTURE.md#roadmap).

## Use

```bash
cargo install --path crates/cli

# Pick your provider(s):
export ANTHROPIC_API_KEY=sk-ant-...   # anthropic (default), streams live
bullpen login openrouter              # OpenRouter, official OAuth PKCE
bullpen login codex                   # ChatGPT subscription, device-code flow
export GLM_API_KEY=...                # GLM (Z.ai), Anthropic-compatible
export KIMI_API_KEY=...               # Kimi (Moonshot), Anthropic-compatible
# ...or skip codex login entirely: bullpen borrows a logged-in Codex CLI's
# session (~/.codex/auth.json) read-only.

cd your-project
bullpen run "find the failing test, explain why it fails"
bullpen run -p codex "..."            # ChatGPT subscription
bullpen run -p glm "..."              # or -p kimi, -p openrouter
bullpen run --sandbox "refactor X"    # confine writes to the workspace (Seatbelt on macOS)
bullpen run -v "..."                  # show tool activity on stderr
bullpen sessions                      # list stored sessions
bullpen run -r <id-prefix> "follow-up question"   # resumes with the session's provider

# Dispatch and watch many background sessions from one screen:
bullpen run --bg "audit the auth module"   # detached; returns immediately
bullpen agents                             # dashboard: grouped, live, dispatch + peek
bullpen logs <id-prefix>                   # tail a background session's output
```

**Agent view** (`bullpen agents`) manages background sessions from one
screen — dispatch, watch them work, peek their output. It's daemonless:
each background session is a detached `bullpen run` that coordinates through
the SQLite store, so they keep running after you close the dashboard, survive
crashes (a dead process shows as Failed and resumes on `run -r`), and appear
in `bullpen sessions` like anything else.

Providers: **anthropic** (true token streaming + prompt caching), **codex**
(ChatGPT subscription), **openrouter**, **glm**, **kimi**. `--sandbox`
confines file writes to the workspace on every platform and, on macOS, runs
shell commands under Seatbelt so arbitrary code can't escape either;
`--sandbox-strict` also cuts network.

Sessions persist in `~/.bullpen/bullpen.db` (SQLite, WAL) as an append-only
entry tree plus an execution log — every step of a run is durable the moment
it happens. Kill bullpen mid-run (crash, `kill -9`, power loss) and the next
invocation recovers: interrupted tool calls get marked, the transcript is
closed cleanly, and the session resumes where it left off.

The same machinery powers **the pen**: the model can delegate bounded tasks
to child agents via the `agent` tool (`inspect` = read-only, `work` = full
tools). Children are ordinary sessions — durable, budgeted, listed by
`bullpen sessions`, resumable — with deterministic identities, so a replayed
delegation reattaches to its child instead of running it twice.

## Design

Start with [ARCHITECTURE.md](ARCHITECTURE.md). The one-sentence version:
a policy-free core loop (`bullpen-agent`) that knows nothing about vendors,
config, or UI, with everything else composed around it at the edge — plus a
single durable store instead of per-process state.

## Develop

```bash
cargo test          # all crates
cargo clippy --all-targets
cargo run -p bullpen -- run "hello"
```

## License

MIT

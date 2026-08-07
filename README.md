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
export ANTHROPIC_API_KEY=sk-ant-...   # anthropic (default)
bullpen login openrouter              # OpenRouter, official OAuth PKCE
bullpen login codex                   # ChatGPT subscription, device-code flow
# ...or skip codex login entirely: bullpen borrows a logged-in Codex CLI's
# session (~/.codex/auth.json) read-only.

cd your-project
bullpen run "find the failing test, explain why it fails"
bullpen run -p codex "..."            # ChatGPT subscription
bullpen run -p openrouter -m "anthropic/claude-sonnet-4.5" "..."
bullpen run -v "..."                  # show tool activity
bullpen sessions                      # list stored sessions
bullpen run -r <id-prefix> "follow-up question"   # resumes with the session's provider
```

Sessions persist in `~/.bullpen/bullpen.db` (SQLite, WAL) as an append-only
entry tree plus an execution log — every step of a run is durable the moment
it happens. Kill bullpen mid-run (crash, `kill -9`, power loss) and the next
invocation recovers: interrupted tool calls get marked, the transcript is
closed cleanly, and the session resumes where it left off.

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

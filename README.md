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
export ANTHROPIC_API_KEY=sk-ant-...
cargo install --path crates/cli

cd your-project
bullpen run "find the failing test, explain why it fails"
bullpen run -v "..."          # show tool activity
bullpen sessions              # list stored sessions
bullpen run -r <id-prefix> "follow-up question"
```

Sessions persist in `~/.bullpen/bullpen.db` (SQLite, WAL) and are saved after
every turn — including failed ones — so anything is resumable.

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

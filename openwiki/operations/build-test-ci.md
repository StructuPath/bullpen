---
type: operations
title: Build, test, and CI — workspace validation
description: The three CI gates (test on Linux+macOS, fmt, clippy -D warnings), the pinned rust-toolchain, the standard tool registry, the shared resource bounds, and the VHS demo tape.
tags: [operations, ci, build, test, clippy, fmt, resource-bounds]
---

# Build, test, and CI

The workspace (`Cargo.toml`, resolver 3, edition 2024) pins
`rust-toolchain.toml` to Rust **1.97.1** with `clippy` and `rustfmt`
components; `rustup` reads it automatically on any cargo invocation. The
profile.release uses `lto = "thin"` and `strip = true`.

## The three CI gates (`.github/workflows/ci.yml`)

- **test (ubuntu-latest, macos-latest)** — `cargo test --workspace`. Both
  platforms matter: [sandbox](../crates/sandbox.md) confines shell commands
  with Seatbelt on macOS and falls back to the in-process check elsewhere,
  and [auth](../crates/auth.md) branches on macOS for credential storage. A
  Linux-only run would not exercise either path. Uses
  `Swatinem/rust-cache@v2` keyed by OS; checkout with
  `persist-credentials: false`.
- **fmt (ubuntu-latest)** — `cargo fmt --all --check`. No rust-cache (rustfmt
  parses sources and never builds).
- **clippy (ubuntu-latest)** — `cargo clippy --workspace --all-targets --
  -D warnings`.

CI runs on push to `main` and on PRs, with `cancel-in-progress` per
`workflow-ref`. Least-privilege `permissions: contents: read`.

The OpenWiki update workflow (`.github/workflows/openwiki-update.yml`)
regenerates this wiki on a schedule.

## The standard tool registry (`Registry::standard`)

`bash`, `read_file`, `write_file`, `edit_file`, `grep`, `glob` — plus `agent`
when the [pen](../crates/pen.md) is enabled (the CLI always registers it).
Specs are emitted in stable sorted order; the `registry_specs_sorted_and_complete`
test asserts the six names.

## Resource bounds (v0)

| Resource | Bound | Source |
|---|---|---|
| Provider turns per send | 500 (fuse, not budget) | `AgentConfig::max_turns` ([agent](../crates/agent.md)) |
| Tool result in transcript | 256 KiB | `MAX_TOOL_RESULT_BYTES` ([agent](../crates/agent.md)) |
| bash timeout | 120 s default, 600 s max | `bash.rs` ([tools](../crates/tools.md)) |
| bash output | 100 KB (head+tail truncation) | `truncate_middle` ([tools](../crates/tools.md)) |
| read_file | 256 KiB | `MAX_READ_BYTES` ([tools](../crates/tools.md)) |
| grep/glob results | 200 | `MAX_RESULTS` ([tools](../crates/tools.md)) |
| Provider retries | 5 attempts, exponential backoff, honors Retry-After | [retry](../crates/llm.md) |
| Pen children per session | 20 (durable count) | `PenConfig::max_children` ([pen](../crates/pen.md)) |
| Pen child wall clock | 900 s | `PenConfig::child_timeout` |
| Parallel tools per batch | 8 | `AgentConfig::max_parallel_tools` ([agent](../crates/agent.md)) |

## The VHS demo

`docs/media/bullpen-agents.tape` drives the real binary through
[VHS](https://github.com/charmbracelet/vhs) to reproduce the README GIF; it
is a demo tape, not a test, and is excluded from the wiki's substantive
coverage (see [quickstart](../quickstart.md) backlog).

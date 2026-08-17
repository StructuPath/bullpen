---
type: crate
title: bullpen-tools — the Tool trait, the Registry, and the built-in workspace tools
description: The Tool trait with parallel_safe and replay_safe hooks, the ambient ToolCtx with optional sandbox write-confinement, the standard registry (bash, read_file, write_file, edit_file, grep, glob), and the shared output caps.
tags: [tools, tool-trait, registry, bash, fs, grep, glob, sandbox]
---

# `bullpen-tools`

`crates/tools/Cargo.toml` dependencies: `bullpen-llm` (for `ToolSpec`),
`bullpen-sandbox` (optional write-confinement in `ToolCtx`),
`async-trait`, `tokio`, `serde_json`, `regex`, `globset`, `ignore`,
`tempfile` (dev). It knows nothing about providers or the loop.

## Module layout

- `lib.rs` — `Tool` trait, `Registry`, `ToolCtx`, shared helpers
- `bash.rs` — `Bash` (shell execution; always serial)
- `fs.rs` — `ReadFile`, `WriteFile`, `EditFile`
- `search.rs` — `Grep`, `Glob` (gitignore-aware workspace search)

## The `Tool` trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn spec(&self) -> ToolSpec;
    fn parallel_safe(&self, _input: &Value) -> bool { false }      // defaults closed
    fn replay_safe(&self) -> bool { false }                          // defaults closed
    async fn run(&self, ctx: &ToolCtx, call_id: &str, input: Value)
        -> Result<String, ToolError>;
}
```

Two hooks deserve emphasis because the [agent loop](agent.md) acts on them:

- **`parallel_safe(input)`** — whether *this concrete invocation* is safe to
  run concurrently with other parallel-safe calls. The runtime owns this
  decision — model-supplied arguments cannot declare themselves parallel-safe.
  Mutating tools and shell execution stay serial. The loop groups maximal
  adjacent runs of parallel-safe calls under a semaphore; everything else runs
  serially in place.
- **`replay_safe()`** — whether re-executing the tool with the same input
  after a crash is safe (no side effects). Snapshotted into the durability
  intent record (`ToolIntent.replay_safe`) at intent time, so re-execution can
  be added on recovery without a schema change. v0 synthesizes interrupted
  results for everything regardless (see [durable execution](../architecture/durable-execution.md)).

`call_id` is the provider-assigned `tool_use` id, stable across a replay —
this is what lets effectful tools (the [pen](pen.md)) derive deterministic
identities.

`ToolError` is `InvalidInput(String) | Failed(String) | Timeout(u64)`.

## `ToolCtx` — ambient context

```rust
pub struct ToolCtx {
    pub workspace: PathBuf,
    pub sandbox: Option<Arc<bullpen_sandbox::Sandbox>>,
}
```

`workspace` is the directory relative paths resolve against (`resolve_path`).
`sandbox` is the optional write-confinement boundary — when set, `bash` runs
under it and the file-editing tools refuse writes it disallows via
`check_write`. The [CLI](cli.md) builds this and widens the sandbox for
worktree git dirs (see [worktree](worktree.md) and [sandbox](sandbox.md)).

## `Registry`

A `BTreeMap<&'static str, Arc<dyn Tool>>` — so `specs()` returns tool specs in
stable sorted order for provider requests. `Registry::standard()` is the
standard workspace registry: `Bash`, `ReadFile`, `WriteFile`, `EditFile`,
`Grep`, `Glob`. The [CLI](cli.md) registers these plus the `agent` (pen) tool.

## Shared output caps and helpers

- `MAX_TOOL_RESULT_BYTES` (defined in the [agent loop](agent.md), 256 KiB)
  caps tool results entering the transcript and the JSON event stream.
- `truncate_middle(s, max_bytes)` — keeps head and tail around an elision
  marker (used by `Bash` at 100 KB and `ReadFile` at 256 KiB).
- `required_str(input, key)` — the shared "missing required string field"
  error.

## Built-in tools

### `Bash` (`bash.rs`)
Always serial (`parallel_safe` false). `DEFAULT_TIMEOUT_SECS = 120`,
`MAX_TIMEOUT_SECS = 600`, `MAX_OUTPUT_BYTES = 100_000`. Under a sandbox,
`ctx.sandbox.wrap_bash(command)` produces the program+args (Seatbelt on macOS,
plain `bash -c` elsewhere); otherwise `bash -c`. `kill_on_drop(true)` ensures
a cancelled run takes the child with it. Non-zero exit → `ToolError::Failed`
with the exit code and output; timeout → `ToolError::Timeout`. Stderr is
folded into the output under a `[stderr]` marker.

### `ReadFile` (`fs.rs`)
`parallel_safe` and `replay_safe` both true. Reads a file, 1-indexed
`line<TAB>content` output, optional `offset`/`limit` line window, capped at
`MAX_READ_BYTES = 256 KiB` via `truncate_middle`. Empty selection reports the
file's line count rather than an empty string.

### `WriteFile` (`fs.rs`)
Serial (default `parallel_safe` false). `check_write` before writing; creates
parent directories; overwrites existing content. Returns the byte count and
path. The in-process sandbox check catches `..` and symlink escapes (see the
`sandbox_blocks_write_outside_workspace` test).

### `EditFile` (`fs.rs`)
Serial. `check_write` before writing. Single-occurrence replacement of
`old_string` with `new_string` — fails if the string is absent ("not found")
or ambiguous ("appears N times"). `old_string` must be non-empty. This is the
change recipe: provide enough surrounding context to disambiguate.

### `Grep` (`search.rs`)
`parallel_safe` and `replay_safe` both true. Rust regex search over file
contents under the workspace (or an optional `path` subdirectory),
gitignore-aware via `ignore::WalkBuilder`. Returns `path:line: text` capped at
`MAX_RESULTS = 200`. Walking and matching run on `spawn_blocking` to keep them
off the runtime. Bad regex → `InvalidInput`.

### `Glob` (`search.rs`)
`parallel_safe` and `replay_safe` both true. Glob match (e.g. `**/*.rs`) under
the workspace, gitignore-aware, `literal_separator(true)`, capped at
`MAX_RESULTS = 200`, results sorted. Bad glob → `InvalidInput`.

## Focused tests

- `registry_specs_sorted_and_complete` — `Registry::standard()` yields exactly
  `[bash, edit_file, glob, grep, read_file, write_file]` in sorted order.
- `write_then_read_roundtrip` / `read_line_window` — write a file, read it
  back; read a line window.
- `edit_requires_unique_match` / `edit_missing_string_fails` — ambiguous
  match errors with the count; absent match errors with "not found".
- `sandbox_blocks_write_outside_workspace` — a workspace-rooted `Sandbox`
  allows an inside-workspace write and refuses `/etc/bullpen-escape`
  in-process.
- `grep_finds_matches_with_locations` / `grep_rejects_bad_regex` /
  `glob_matches_relative_paths` — search behavior and input validation.

See [the sandbox](sandbox.md) for the confinement boundary itself and
[the agent loop](agent.md) for how `parallel_safe`/`replay_safe` drive
scheduling and durability.

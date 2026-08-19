---
type: crate
title: bullpen-tools — the Tool trait, the Registry, and the built-in workspace tools
description: The Tool trait with parallel_safe and replay_safe hooks, the ambient ToolCtx with optional sandbox write-confinement, the standard registry (bash, hashline read/write/edit, grep, glob, ast_grep/ast_edit), the Ask follow-up tool, the shared output caps, and the content-probed read path (files, directories, SQLite, archives, URLs).
tags: [tools, tool-trait, registry, bash, fs, grep, glob, hashline, sqlite, archive, ast-grep, ask, sandbox]
---

# `bullpen-tools`

`crates/tools/Cargo.toml` dependencies: `bullpen-llm` (for `ToolSpec`),
`bullpen-sandbox` (optional write-confinement in `ToolCtx`),
`async-trait`, `tokio`, `serde_json`, `regex`, `globset`, `ignore`,
`sha2` (hashline anchors), `rusqlite` (SQLite reads), `zip`/`tar`/`flate2`
(archive reads), `reqwest` + `futures` (URL fetches), `tempfile` (dev). It
knows nothing about providers or the loop.

## Module layout

- `lib.rs` — `Tool` trait, `Registry`, `ToolCtx`, shared helpers
- `bash.rs` — `Bash` (shell execution; always serial)
- `fs.rs` — `ReadFile`, `WriteFile`, `EditFile` (hashline reads + patches)
- `search.rs` — `Grep`, `Glob` (gitignore-aware workspace search)
- `sqlite.rs` — SQLite database reading behind `ReadFile` (content-detected)
- `archive.rs` — zip/tar/tar.gz/gzip reading behind `ReadFile` (content-detected)
- `ast.rs` — `AstGrep`, `AstEdit` (structural search/rewrite via ast-grep)
- `github.rs` — `GitHub` (raw `gh` CLI arguments; parallel-safe for reads)
- `ask.rs` — `Ask` + `Asker` trait (transport-agnostic follow-up questions)

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
this is what lets effectful tools (the [pen](pen.md)) and the [todo](todo.md)
tool derive deterministic identities.

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
under it, the file-editing tools refuse writes it disallows via `check_write`,
URL fetches are refused when the sandbox denies network, and `ast_edit`'s
apply path runs through the sandbox's shell wrapper. The [CLI](cli.md) builds
this and widens the sandbox for worktree git dirs (see [worktree](worktree.md)
and [sandbox](sandbox.md)).

## `Registry`

A `BTreeMap<&'static str, Arc<dyn Tool>>` — so `specs()` returns tool specs in
stable sorted order for provider requests. `Registry::standard()` is the
standard workspace registry: `Bash`, `ReadFile`, `WriteFile`, `EditFile`,
`Grep`, `Glob`, `AstGrep`, `AstEdit` — eight tools, verified by
`registry_specs_sorted_and_complete` to yield exactly
`[ast_edit, ast_grep, bash, edit_file, glob, grep, read_file, write_file]` in
sorted order. The [CLI](cli.md) registers these plus the session-scoped tools
(`agent`/[pen](pen.md), `job`, `todo`), `GitHub`, and `Ask`.

## Shared output caps and helpers

- `MAX_TOOL_RESULT_BYTES` (defined in the [agent loop](agent.md), 256 KiB)
  caps tool results entering the transcript and the JSON event stream.
- `truncate_middle(s, max_bytes)` — keeps head and tail around an elision
  marker (used by `Bash` at 100 KB, `ReadFile` at 256 KiB, archive entries).
- `required_str(input, key)` — the shared "missing required string field"
  error.
- `line_hash(line)` — the first two bytes of the line's SHA-256, in hex
  (4 hex chars). The anchor `read_file` emits and `edit_file` patches address.
- `hashlines(lines, first, limit)` — render a slice of lines as 1-indexed
  `line#hash<TAB>content`.

## Built-in tools

### `Bash` (`bash.rs`)
Always serial (`parallel_safe` false). `DEFAULT_TIMEOUT_SECS = 120`,
`MAX_TIMEOUT_SECS = 600`, `MAX_OUTPUT_BYTES = 100_000`. Under a sandbox,
`ctx.sandbox.wrap_bash(command)` produces the program+args (Seatbelt on macOS,
plain `bash -c` elsewhere); otherwise `bash -c`. `kill_on_drop(true)` ensures
a cancelled run takes the child with it. Non-zero exit → `ToolError::Failed`
with the exit code and output; timeout → `ToolError::Timeout`. Stderr is
folded into the output under a `[stderr]` marker.

### `ReadFile` (`fs.rs`) — one path for files, directories, SQLite, archives, URLs
`parallel_safe` and `replay_safe` both true. `read_file` is the single read
path: the `path` input is content-probed and dispatched to the right renderer.
Output is 1-indexed `line#hash<TAB>content` (hashline), capped at
`MAX_READ_BYTES = 256 KiB` via `truncate_middle`.

The dispatch in `ReadFile::run`:

1. **URL** (`http://`/`https://`) → `read_url` (see below).
2. **Directory** (`tokio::fs::metadata` is a dir) → `list_dir` — a sorted
   listing (directories first, then files with byte sizes), capped at 256 KiB.
3. **Content probe** (on `spawn_blocking`): `sqlite::is_sqlite` (16-byte
   `SQLite format 3\0` magic) → `Probed::Sqlite`; `archive::detect` (zip
   `PK\x03\x04`, gzip `0x1f 0x8b`, tar `ustar` at offset 257; a gzip stream is
   `TarGz` only if the decompressed head carries the tar magic, else `Gz`) →
   `Probed::Archive(kind)`; else `Probed::Plain`.
4. **SQLite** → `sqlite::read_sqlite(path, query)`: no `query` renders the
   schema overview (every table with columns + row count); a `query` runs
   read-only SQL rendered as TSV. Read-only is enforced by the engine
   (`mode=ro` + `query_only` pragma; `immutable=1` fallback for live-WAL
   stores). `MAX_ROWS = 200`, `MAX_OUTPUT_BYTES = 100_000`.
5. **Archive** → `archive::list` (no `entry`) or `archive::read_entry` (with
   `entry`). A plain gzip (`Kind::Gz`) decompresses as hashline text, not an
   archive. Listing caps at `MAX_ENTRIES = 1000`; extraction caps at
   `MAX_ENTRY_BYTES = 262_144` while streaming; tar scans are bounded by
   `MAX_SCAN_BYTES = 64 MiB` (decompressed) so a gzip bomb cannot buy
   unbounded CPU.
6. **Plain file** → read as text, `hashlines(lines, offset, limit)` with the
   optional `offset`/`limit` line window. Empty selection reports the file's
   line count.

`read_url(ctx, url)`: GET with a 30 s timeout, body streamed and capped at
`MAX_FETCH_BYTES = 2 MiB` *while streaming* (not after). The sandbox's network
capability governs this exactly as it governs shell commands: a
`--sandbox-strict` sandbox (network cut) refuses the call. Non-2xx →
`ToolError::Failed` carrying the status and a truncated body.

### `WriteFile` (`fs.rs`)
Serial (default `parallel_safe` false). `check_write` before writing; creates
parent directories; overwrites existing content. Returns the byte count and
path. The in-process sandbox check catches `..` and symlink escapes (see the
`sandbox_blocks_write_outside_workspace` test).

### `EditFile` (`fs.rs`) — exact string or hashline patch
Serial. `check_write` before writing. Two modes, exactly one per call:

- **Exact string** (`old_string`/`new_string`): single-occurrence replacement.
  Fails if absent ("not found") or ambiguous ("appears N times … provide more
  context"). `old_string` must be non-empty.
- **Hashline patch** (`patch`): an array of hunks, each addressed by an
  `anchor` — the `line#hash` token `read_file` emits. `op` is `replace`,
  `insert_after`, or `delete`; `to` extends `replace`/`delete` to an inclusive
  span; `content` is the replacement or inserted lines (newline-separated).
  Anchor `"0"` with `insert_after` prepends at the top of the file.

The anchor is a claim about *content*, not just a position. `resolve_anchor`
decides: if the anchored line still has its hash at that line number, splice
there; if the hash moved (found on exactly one other line), follow it — the
edit lands correctly and the result notes "(N moved anchor(s) followed by
content hash)"; if the hash is gone or appears on several lines, fail with
fresh hashline context to re-anchor from, never a wrong edit. `parse_hunks`
sorts splices and rejects overlapping hunks (including two insertions at one
point, whose order the input cannot express). Tested by
`a_moved_anchor_is_followed_by_its_hash`,
`a_changed_anchor_fails_with_fresh_context_not_a_wrong_edit`, and
`ambiguous_recovery_fails_rather_than_guessing`.

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

### `AstGrep` / `AstEdit` (`ast.rs`) — structural search and rewrite
Shell out to the [ast-grep] binary, discovered at call time (`ast-grep`, then
`sg`, verified by `--version` — `sg` is also the Unix shell-group utility, so
the version check is load-bearing). Absence is a clear error with an install
hint (`cargo install ast-grep`), never a degraded fallback to text search.
`TIMEOUT_SECS = 120`, `MAX_MATCHES = 200`, `MAX_OUTPUT_BYTES = 100_000`.

- **`AstGrep`** (`name = "ast_grep"`): `parallel_safe` and `replay_safe` both
  true. `pattern` matched against the syntax tree with metavariables
  (`$NAME`, `$$$ARGS`); `lang` forces a language; `path` narrows. Results are
  `file:line` plus the matched text (1-based, matching `read_file`/`grep`).
- **`AstEdit`** (`name = "ast_edit"`): `parallel_safe` only for a *preview*
  (`apply` false); an apply mutates the workspace and stays serial. Every match
  of `pattern` becomes `rewrite`, with metavariables carried over. By default
  nothing is written — the call returns the diff to review; `apply: true`
  writes it. An apply runs through the sandbox's shell wrapper
  (`sandbox.wrap_bash`), Seatbelt-confined on macOS like `bash`.

### `GitHub` (`github.rs`) — GitHub CLI operations
`name = "github"`. Raw `gh` arguments as an array — no shell in between, so
nothing needs quoting and nothing can be injected. `parallel_safe(input)`
derives from the verb: `search` (first position) or a read verb
(`view`/`list`/`diff`/`checks`/`status`, second position) is parallel-safe;
mutations and `api` stay serial (`api` can carry any method, so it is absent
from `READ_VERBS` on purpose). `replay_safe` false. A sandbox that denies
network refuses the call outright (`sandbox.capabilities().allow_network`).
A missing binary errors with the install + `gh auth login` hint.
`TIMEOUT_SECS = 120`, `MAX_OUTPUT_BYTES = 100_000`. `GH_PROMPT_DISABLED=1` and
`NO_COLOR=1` are set so a headless run never falls into an interactive prompt.
Registered on top-level [CLI](cli.md) runs only — pen children keep to the
workspace.

### `Ask` / `Asker` (`ask.rs`) — follow-up questions
`name = "ask"`. Transport-agnostic: the application injects an `Asker` (the
[CLI's `TtyAsker`](cli.md) reads one line from the controlling terminal), and a
detached run (background, `--json`, piped stdin) registers `Ask::detached()`,
which fails the call with a clear reason ("this run is detached — no one is
listening") instead of blocking forever. `parallel_safe` false, `replay_safe`
false. Optional `options` render as a numbered choice list; a numeric reply
resolves to the chosen option's text, anything else is taken verbatim (`resolve`).
The model always sees the same tool; only the answerer changes.

## Focused tests

- `registry_specs_sorted_and_complete` — `Registry::standard()` yields exactly
  `[ast_edit, ast_grep, bash, edit_file, glob, grep, read_file, write_file]` in
  sorted order.
- `write_then_read_roundtrip` / `read_line_window` — write a file, read it back
  (hashline `line#hash` format); read a line window.
- `edit_requires_unique_match` / `edit_missing_string_fails` — exact-string
  mode: ambiguous match errors with the count; absent match errors with
  "not found".
- `patch_applies_replace_insert_and_delete_in_one_call` — three hunks in one
  `patch` call (replace, delete, insert_after at `"0"`).
- `a_moved_anchor_is_followed_by_its_hash` — lines inserted above; the anchor
  is followed to the new location, result notes "moved anchor".
- `a_changed_anchor_fails_with_fresh_context_not_a_wrong_edit` — the anchored
  content changed; the error carries current hashlines to re-anchor from, and
  the file is untouched.
- `ambiguous_recovery_fails_rather_than_guessing` — the content now appears
  twice; the edit fails rather than guessing.
- `malformed_patches_are_invalid_input` — 12 malformed `patch` shapes
  (empty, missing anchor, bad op, missing content, overlapping hunks, both
  modes at once) all yield `InvalidInput`.
- `a_directory_reads_as_a_sorted_listing` — directories first, files with sizes.
- `a_sqlite_database_reads_as_schema_then_queries` — schema overview then a
  read-only `query` returning rows.
- `plain_gzip_reads_as_decompressed_text` / `an_archive_reads_as_a_listing_then_an_entry`
  — gzip decompresses as hashline text; a zip lists then extracts one entry.
- `urls_fetch_through_the_same_path` / `a_failing_url_is_an_error_carrying_the_status` /
  `a_network_denying_sandbox_refuses_urls` — URL fetch, error bodies, sandbox cut.
- `sandbox_blocks_write_outside_workspace` — a workspace-rooted `Sandbox`
  allows an inside-workspace write and refuses `/etc/bullpen-escape`
  in-process.
- `grep_finds_matches_with_locations` / `grep_rejects_bad_regex` /
  `glob_matches_relative_paths` — search behavior and input validation.
- `only_reads_are_parallel_safe` (github) — read verbs ride together;
  mutations and `api` stay serial; `replay_safe` false.

See [the sandbox](sandbox.md) for the confinement boundary itself and
[the agent loop](agent.md) for how `parallel_safe`/`replay_safe` drive
scheduling and durability. The session-scoped tools (`agent`/pen, `job`,
`todo`) live in [harness](harness.md); `Ask` is wired by the [CLI](cli.md).

[ast-grep]: https://ast-grep.github.io

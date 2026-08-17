---
type: crate
title: bullpen-sandbox — OS-level write confinement with Seatbelt on macOS
description: The Sandbox with in-process allows_write path resolution and out-of-process wrap_bash via a generated Seatbelt profile, strict network denial, and the intended-Landlock gap on Linux.
tags: [sandbox, seatbelt, write-confinement, macos, landlock, sandbox-exec]
---

# `bullpen-sandbox`

`crates/sandbox/Cargo.toml` is minimal: `bullpen-tools` is *not* a dependency
(sandbox is a leaf); only `libc` would be expected, but the crate is pure Rust
using `std::fs` and `cfg!(target_os = "macos")`. It is a leaf dependency of
[`bullpen-tools`](tools.md) (in-process check) and the [CLI](cli.md)
(out-of-process shell wrap).

This is deliberately a **write-confinement** boundary, not a full jail: reads
stay broad because compilers and interpreters legitimately read across the
system. It closes the specific gap that approval-prompt-only harnesses leave
open — an agent modifying files outside its workspace — with an honest scope.

## `Sandbox` and `Capabilities`

```rust
pub struct Capabilities {
    pub write_roots: Vec<PathBuf>,
    pub allow_network: bool,
}
pub struct Sandbox { caps: Capabilities }
```

- `Sandbox::workspace(ws)` — default policy: writes confined to the workspace
  plus system temp (`/tmp`, `/private/tmp`, `/private/var/folders`, `$TMPDIR`);
  network allowed (package managers, fetches).
- `Sandbox::strict(ws)` — same write confinement, network denied
  (`--sandbox-strict`).
- `Sandbox::with_capabilities(caps)` — custom.
- `Sandbox::allowing_writes(roots)` — widen the write confinement to further
  subtrees. The caller names those, since the sandbox has no business knowing
  what a git worktree is. This is the seam the [CLI](cli.md) uses to add a
  linked worktree's git dirs so a sandboxed agent in a worktree can still
  stage and commit (see [worktree](worktree.md)).

## In-process: `allows_write` (every platform)

`allows_write(path)` resolves the path to an absolute, symlink-free form for
prefix-checking against the canonicalized write roots. Since the file itself
may not exist, `resolve_for_write` walks up to the nearest existing ancestor,
canonicalizes it (following symlinks), and re-appends the trailing components —
so `..` and symlink escapes are caught. The [file tools](tools.md) call this
via `ToolCtx::check_write` before writing.

## Out-of-process: `wrap_bash` (macOS only)

`wrap_bash(command)` returns the program + args that run a shell command
confined. On macOS:

```
sandbox-exec -p <profile> bash -c <command>
```

The generated `seatbelt_profile()` is an SBPL profile: allow everything, then
`(deny network*)` if strict, then `(deny file-write*)`, then re-allow
`(allow file-write* (subpath <root>))` for each canonicalized write root plus
`/dev` (device nodes — writing ttys and `/dev/null` is normal and safe). Last
matching rule wins in SBPL, so a write inside a root ends on the re-allow and
a write outside ends on the deny. Roots are canonicalized so `/tmp` →
`/private/tmp` and workspace symlinks match the real paths Seatbelt sees.

On non-macOS, `wrap_bash` returns plain `bash -c` — the in-process check still
guards the file tools, but arbitrary shell writes are unconfined. Linux
out-of-process confinement is the intended Landlock mechanism; not yet
implemented.

`Sandbox::os_enforced()` returns `cfg!(target_os = "macos")` so callers can
warn (the CLI prints a notice when `--sandbox` is set on a non-macOS host).

## Security posture (v0)

With `--sandbox`, writes are confined to the workspace on every platform, and
on macOS shell commands and their children run under Seatbelt. `--sandbox-strict`
also denies network. **Without** `--sandbox`, tools run with the process's
full authority — that is still the default. Do not point v0 at anything you
wouldn't hand to a contractor's laptop. See [security posture](../operations/security-posture.md)
for the full honest version and the worktree confinement caveat.

## Focused tests

- `allows_writes_inside_workspace_denies_outside` — workspace subpaths and
  `/tmp/scratch` allowed; `/etc/passwd`, `/usr/lib/thing`, `$HOME/evil.txt`
  denied (escape targets chosen outside the temp roots so the tempdir's own
  writes don't mask the denial).
- `symlink_escape_is_denied` — a symlink inside the workspace pointing at
  `/usr`; writing through it resolves to `/usr` → denied.
- `seatbelt_profile_shape` — strict profile contains `(deny file-write*)`,
  `(deny network*)`, `(allow file-write*`, and the canonicalized workspace path;
  non-strict profile has no `(deny network*)`.
- `wrap_bash_targets_the_platform` — macOS: `sandbox-exec -p … bash -c …`;
  elsewhere: `bash -c …`.
- Live verification: a shell write to `$HOME` under `--sandbox` is denied by
  the OS on macOS while a workspace write succeeds.
- `a_sandboxed_worktree_can_still_commit` (macOS-only, in
  [worktree](worktree.md)'s suite) — the widened write roots are exercised for
  real by running `git commit` under the generated Seatbelt profile inside a
  linked worktree: the commit succeeds, proving the `git_write_roots` widening
  is sufficient to let a sandboxed agent stage and commit (the only evidence
  that would justify reclaiming its directory).

---
type: operations
title: Security posture — sandboxing and the trust boundary
description: With --sandbox writes are confined to the workspace on every platform and on macOS shell commands run under Seatbelt; --sandbox-strict also denies network. Without --sandbox tools run with the process's full authority and that is still the default.
tags: [operations, security, sandbox, seatbelt, trust-boundary]
---

# Security posture (v0 — honest version)

With `--sandbox` (M3), writes are confined to the workspace on every
platform, and on macOS shell commands and their children run under Seatbelt.
`--sandbox-strict` also denies network. That boundary is **write-confinement,
not a jail**: reads stay broad, because toolchains legitimately read across
the system. See [sandbox](../crates/sandbox.md).

Without `--sandbox`, tools run with the process's full authority — and that
is still the default. Linux has no out-of-process confinement yet (Landlock
is the intended mechanism; the in-process write check already works there).
The README puts it bluntly: do not point v0 at anything you wouldn't hand to
a contractor's laptop.

## The two layers (together)

1. **In-process** (`Sandbox::allows_write`) — every platform. The file tools
   ask `ToolCtx::check_write` before writing, so a path that escapes the
   allowed roots is refused in Rust before any syscall. Resolves the nearest
   existing ancestor (following symlinks) so `..` and symlink escapes are
   caught.
2. **Out-of-process** (`Sandbox::wrap_bash`) — macOS only today. A generated
   Seatbelt (`sandbox-exec`) profile confines shell commands and their
   children to the workspace + system temp, optionally denying network.
   `Sandbox::os_enforced()` reports false off-macOS so the CLI can warn.

## Worktree sandbox widening

A linked worktree's `.git` is a file; its index, refs, and objects all live
under the main repository, outside the worktree. A sandbox confined to the
worktree alone would leave an agent able to edit files but unable to stage or
commit them — which is fatal, since a commit is the only evidence that would
ever justify reclaiming its directory. The run therefore adds the worktree's
git dirs to the write roots via [`worktree::git_write_roots`](../crates/worktree.md).
That grants the whole shared common directory (git's ref store cannot be
sliced by path), so a sandboxed agent in a worktree can also write the main
repository's config and hooks — a residual risk that needs a mechanism other
than a path allowlist.

See [sandbox](../crates/sandbox.md) for the implementation and the focused
tests (workspace denial, symlink escape, seatbelt profile shape, wrap_bash
platform targeting).

---
type: crate
title: worktree — per-session git worktrees that are never removed
description: The fail-closed worktree module in bullpen-harness — path/branch derivation (including store-anchored paths for pen children), the resume decision table, the thin git adapter, git_write_roots widening the sandbox, and the four Location outcomes.
tags: [worktree, git, fail-closed, retention, sandbox-widening, harness]
---

# Per-session git worktrees (`crates/harness/src/worktree.rs`)

`--bg --worktree` gives a background session its own git worktree on a
run-unique `bullpen/<id>` branch, so concurrent background sessions stop
editing each other's files. The [pen](pen.md) uses the same module to isolate a
`worktree: true` work child. This module creates worktrees and **never removes
one** — not on success, not on failure, not later. A worktree can hold the
only copy of what an agent did, so cleaning up is yours to decide. The
asymmetry is the whole argument: a leftover directory costs disk; an eager
cleanup can destroy the only copy of the work.

The module moved from `crates/cli` to `crates/harness` so the pen can place
work children; the [CLI](cli.md) now imports it as `bullpen_harness::worktree`.
The derivation and the `Location` decision table are unchanged.

## Pure derivation (testable without a repository)

- `worktree_path(session_id)` → `$BULLPEN_HOME/worktrees/<id>` (under
  `bullpen_store::home_dir`, not inside the repo — the worktree is bullpen's
  state, keyed to a session id; moving the database without it would split
  the two apart).
- `worktree_path_for_store(store_path, session_id)` → the same layout anchored
  to a specific store: `<store_dir>/worktrees/<id>`. For the default store
  this is exactly `worktree_path`; for a pen pointed at an isolated store
  (tests, `$BULLPEN_HOME`), the worktrees follow the store instead of the
  ambient environment. The pen uses this in `place_child`.
- `branch_for(session_id)` → `bullpen/<id>` (the whole id, not the short
  prefix the CLI prints — a truncated id is 32 bits and two collisions would
  name one branch, which git refuses to check out in two worktrees at once).
- `decide(recorded, candidate, branch_exists) -> Location` — the **resume
  decision table**. git and the filesystem supply only the `Candidate` and
  the branch bit, so every row is testable without either:

  | recorded | candidate | branch_exists | Location |
  |---|---|---|---|
  | None | — | — | `Shared` (no worktree was ever recorded) |
  | Some | `Live` | — | `Use(path)` |
  | Some | `Foreign` | — | `Occupied { path, branch }` |
  | Some | `Gone` | true | `Recreate { path, branch }` |
  | Some | `Gone` | false | `Fail { path, branch }` |

  A recorded worktree always wins over the caller's working directory: a
  `bullpen run -r <id>` typed from somewhere else must not quietly append to
  the session in a different tree. By the same rule a foreign directory is
  refused outright rather than run in or overwritten.

## The impure half

- `repo_root(cwd)` — `git -C cwd rev-parse --show-toplevel`; **fails when
  there isn't one** — `--worktree` never degrades to the shared working
  directory (silently doing that is exactly the behaviour the flag exists to
  avoid).
- `create(root, path, branch)` — `git worktree add -b branch path HEAD`
  (new branch off current HEAD).
- `recreate(root, path, branch)` — `git worktree add --force path branch`.
  `--force` is required, not optional: git's administrative entry for the
  deleted directory still claims both the path and the branch; pruning that
  entry would be a removal path, which this module does not have.
- `branch_exists(root, branch)` — `git rev-parse --verify refs/heads/<branch>`.
- `locate(recorded_path, recorded_branch, anchor) -> Location` — answers
  `decide`'s two inputs. `inspect(anchor, path, branch) -> Candidate` is the
  three-way check that what stands at the path is *this session's* worktree:
  shared git common dir (so a worktree of another repository is not mistaken
  for this one), the worktree's own top level (so an ordinary directory left
  at the path is not run in), and the checked-out branch (so a *different*
  worktree of the same repo restored at this path does not capture the
  session). A dangling symlink reports itself absent, so it is `Foreign`
  (not `Gone`, which would send resume into a recreate git then refuses).

## `git_write_roots(cwd)`

The git directories a run in `cwd` must be able to write to, beyond `cwd`
itself. Empty for an ordinary checkout (`.git` is inside the workspace); for
a linked worktree the index, refs, and objects all live under the main
repository, so a [sandbox](sandbox.md) confined to the worktree alone would
let an agent edit files but never stage or commit them — and a commit is the
only evidence that would ever justify reclaiming its directory.

Returns `vec![git_dir, common_dir]` when `git_dir != common_dir`. It is the
**whole common directory**, not the session's own ref and objects: git's ref
store is shared mutable state that a path allowlist cannot slice. Once
`git gc` packs refs it prunes `refs/heads/bullpen/`, so the next commit has to
recreate that directory — and `packed-refs` sits at the top of the common
directory. Granting either grants every branch. The residual risk is real
and unavoidable here: a sandboxed agent in a worktree can write the main
repository's config and hooks. Confining that needs a mechanism other than a
path allowlist.

## Focused tests

The `decide` table is pure, so it is tested without git or a filesystem:

- `decide_recreates_from_the_branch_when_only_the_directory_is_gone` —
  `Candidate::Gone` + branch survives → `Location::Recreate`.
- `a_foreign_directory_at_the_recorded_path_is_refused_even_with_the_branch` —
  `Candidate::Foreign` → `Location::Occupied` even when the branch still
  exists (never run elsewhere, never overwrite).
- `a_worktree_of_the_same_repo_on_another_branch_is_not_this_session` — a
  worktree of the same repository checked out on a *different* branch is
  `Candidate::Foreign`, not `Live`, so resuming onto it is refused (the branch
  check in `inspect`).
- `a_regular_file_at_the_recorded_path_is_occupied_not_gone` — a regular file
  (not a directory) at the recorded path is `Occupied`, not `Gone`, so resume
  does not fall through to recreate (which `git worktree add` would refuse on
  the occupied path with a misleading error).

The `inspect` checks that classify `Live`: the candidate and the anchor share
the same git common dir (same repository), the path is the worktree's own top
level, and the checked-out branch matches the recording. A **detached HEAD**
is deliberately `Foreign` (not `Live`): the recording names a branch, and
resuming onto a detached head would leave the work unreachable by that name
(`head_branch` uses `symbolic-ref` and returns `None` for detached HEADs).

`init_repo` builds a real repo with one commit; tests for `create`/`recreate`/
`git_write_roots` exercise the git adapter against it, and the macOS-only
`a_sandboxed_worktree_can_still_commit` is covered in [sandbox](sandbox.md).

See [the CLI run path](cli.md) for where the worktree is recorded before
creation and where `locate` decides the run directory, [the pen](pen.md) for
how `place_child` records then creates a work child's worktree, and
[sandbox](sandbox.md) for how `git_write_roots` widens the write roots.

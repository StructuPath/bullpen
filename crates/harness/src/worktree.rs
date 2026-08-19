//! Per-session git worktrees for `--bg --worktree`.
//!
//! Two background sessions sharing one checkout edit each other's files.
//! Giving each its own worktree on its own branch removes that entirely, at
//! the cost of a directory that has to be found again later — which is why
//! the path and branch are recorded in the store rather than recomputed.
//!
//! **This module creates worktrees and never removes one.** No exit path
//! here deletes a directory or a branch, because a worktree may hold the
//! only copy of an agent's work: an uncommitted diff, a half-finished
//! rebase, a file the agent wrote but never mentioned. Uncertainty retains.
//! A leftover directory costs disk; an eager cleanup costs the work itself.
//!
//! Split like [`bullpen_store::home_dir`]: pure derivation (path, branch
//! name, and the resume decision table) separated from a thin `git` adapter,
//! so the decisions are testable without a repository. git is shelled out to
//! rather than linked, matching how `bullpen-tools` runs bash and grep.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::bail;

/// Where a background session's isolated checkout lives. Under
/// `$BULLPEN_HOME` rather than inside the repository: the worktree is
/// bullpen's state, keyed to a session id, and moving the database without
/// it would split the two apart.
pub fn worktree_path(session_id: &str) -> PathBuf {
    worktree_dir(&bullpen_store::home_dir(), session_id)
}

/// The same layout, anchored to a specific store: worktrees live alongside
/// the database. For the default store this is exactly [`worktree_path`];
/// for a pen pointed at an isolated store (tests, `$BULLPEN_HOME`), the
/// worktrees follow the store instead of the ambient environment.
pub fn worktree_path_for_store(store_path: &Path, session_id: &str) -> PathBuf {
    worktree_dir(store_path.parent().unwrap_or(Path::new(".")), session_id)
}

fn worktree_dir(home: &Path, session_id: &str) -> PathBuf {
    home.join("worktrees").join(session_id)
}

/// The branch a session's worktree checks out. The whole session id, not the
/// short prefix the CLI prints: a truncated id is 32 bits, and two sessions
/// that collided would name one branch, which git refuses to check out in two
/// worktrees at once — the second dispatch would simply fail.
pub fn branch_for(session_id: &str) -> String {
    format!("bullpen/{session_id}")
}

/// The repository containing `cwd`.
///
/// Fails when there isn't one. `--worktree` never degrades to the shared
/// working directory: silently doing that is exactly the behaviour the flag
/// exists to avoid.
pub fn repo_root(cwd: &Path) -> anyhow::Result<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    let out = match out {
        Ok(out) => out,
        Err(e) => bail!("--worktree needs the git binary, but running it failed: {e}"),
    };
    if !out.status.success() {
        bail!(
            "{} is not inside a git repository, so --worktree has nothing to \
             branch from — it will not fall back to running in the shared \
             working directory",
            cwd.display()
        );
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

/// Check out a new worktree at `path` on a new `branch` off the current HEAD.
pub fn create(root: &Path, path: &Path, branch: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git_worktree_add(root, &["-b", branch], path, "HEAD")
}

/// Restore a recorded worktree whose directory is gone but whose branch
/// survives. `--force` is required, not optional: git's administrative entry
/// for the deleted directory still claims both the path and the branch.
/// Pruning that entry would be a removal path, which this module does not have.
pub fn recreate(root: &Path, path: &Path, branch: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git_worktree_add(root, &["--force"], path, branch)
}

fn git_worktree_add(
    root: &Path,
    flags: &[&str],
    path: &Path,
    commit_ish: &str,
) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add"])
        .args(flags)
        .arg(path)
        .arg(commit_ish)
        .output();
    let out = match out {
        Ok(out) => out,
        Err(e) => bail!("could not run git worktree add: {e}"),
    };
    if !out.status.success() {
        bail!(
            "git worktree add failed for {}:\n{}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The git directories a run in `cwd` must be able to write to, beyond `cwd`
/// itself. Empty for an ordinary checkout, whose `.git` is already inside the
/// workspace; for a linked worktree the index, refs and objects all live under
/// the main repository, so a sandbox confined to the worktree alone would let
/// an agent edit files but never stage or commit them — and a commit is the
/// only evidence that would ever justify reclaiming its directory.
///
/// The whole common directory, not the session's own ref and objects: git's
/// ref store is shared mutable state that a path allowlist cannot slice. Once
/// `git gc` packs refs it prunes `refs/heads/bullpen/`, so the next commit has
/// to recreate that directory under `refs/heads` — and `packed-refs` itself
/// sits at the top of the common directory. Granting either grants every
/// branch. The residual risk is real and unavoidable here: a sandboxed agent
/// in a worktree can write the main repository's config and hooks.
pub fn git_write_roots(cwd: &Path) -> Vec<PathBuf> {
    let (Some(git_dir), Some(common_dir)) = (
        rev_parse_dir(cwd, "--absolute-git-dir"),
        rev_parse_dir(cwd, "--git-common-dir"),
    ) else {
        return Vec::new();
    };
    if git_dir == common_dir {
        return Vec::new();
    }
    vec![git_dir, common_dir]
}

/// `git rev-parse <flag>`, resolved to an absolute path. `--git-common-dir`
/// answers relative to `cwd` in an ordinary checkout and absolutely in a
/// linked worktree, so both forms have to be accepted before the two can be
/// compared.
fn rev_parse_dir(cwd: &Path, flag: &str) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", flag])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let dir = if dir.is_absolute() {
        dir
    } else {
        cwd.join(dir)
    };
    Some(dir.canonicalize().unwrap_or(dir))
}

pub fn branch_exists(root: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Where a run should happen, given what the session recorded.
#[derive(Debug, PartialEq, Eq)]
pub enum Location {
    /// No worktree was ever recorded: the caller's working directory.
    Shared,
    Use(PathBuf),
    Recreate {
        path: PathBuf,
        branch: String,
    },
    Fail {
        path: PathBuf,
        branch: String,
    },
    /// Something that is not this session's worktree sits at the recorded
    /// path.
    Occupied {
        path: PathBuf,
        branch: String,
    },
}

/// What stands at a session's recorded worktree path.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Candidate {
    /// A live worktree of the session's own repository.
    Live,
    /// Nothing — the directory is gone.
    Gone,
    /// A directory, but not that worktree: an ordinary one restored at the
    /// path, or a worktree of a different repository.
    Foreign,
}

/// The resume decision, as a table. git and the filesystem supply only the
/// [`Candidate`] and the branch bit, so every row is testable without either.
///
/// A recorded worktree always wins over the caller's working directory: a
/// `bullpen run -r <id>` typed from somewhere else must not quietly append
/// to the session in a different tree than the one it has been editing. By
/// the same rule a foreign directory is refused outright rather than run in
/// or overwritten — either would be the silent misplacement this avoids.
pub fn decide(
    recorded: Option<(&str, &str)>,
    candidate: Candidate,
    branch_exists: bool,
) -> Location {
    let Some((path, branch)) = recorded else {
        return Location::Shared;
    };
    let path = PathBuf::from(path);
    let branch = branch.to_string();
    match (candidate, branch_exists) {
        (Candidate::Live, _) => Location::Use(path),
        (Candidate::Foreign, _) => Location::Occupied { path, branch },
        (Candidate::Gone, true) => Location::Recreate { path, branch },
        (Candidate::Gone, false) => Location::Fail { path, branch },
    }
}

/// The impure half of [`decide`]: answers its two inputs. `anchor` is the
/// session's recorded cwd — the directory it was dispatched from, which is
/// what still points at the repository once the worktree itself is gone.
pub fn locate(
    recorded_path: Option<&str>,
    recorded_branch: Option<&str>,
    anchor: &Path,
) -> Location {
    let Some((path, branch)) = recorded_path.zip(recorded_branch) else {
        return Location::Shared;
    };
    let candidate = inspect(anchor, Path::new(path), branch);
    let branch_lives = candidate == Candidate::Gone
        && repo_root(anchor).is_ok_and(|root| branch_exists(&root, branch));
    decide(Some((path, branch)), candidate, branch_lives)
}

/// Whether `path` really is *this session's* worktree of the repository
/// `anchor` sits in. Three things have to agree, and each guards a different
/// way of ending up in the wrong tree:
///
/// - the shared git directory, so a worktree of another repository is not
///   mistaken for this one;
/// - the worktree's own top level, so an ordinary directory left at the
///   recorded path is not run in;
/// - the checked-out branch, so a *different* worktree of the same
///   repository restored at this path does not capture the session. Without
///   this a resumed session commits to whatever branch happens to be there.
///
/// Anything that exists at the path but is not that worktree is `Foreign`,
/// including a regular file: `Gone` would send resume down the recreate path,
/// where `git worktree add` fails on the occupied path with an error that
/// says nothing about what is actually in the way.
fn inspect(anchor: &Path, path: &Path, branch: &str) -> Candidate {
    // `symlink_metadata` rather than `exists`, which follows links: a dangling
    // symlink at the recorded path is an obstruction that reports itself
    // absent, and `Gone` would send resume into a recreate git then refuses.
    if std::fs::symlink_metadata(path).is_err() {
        return Candidate::Gone;
    }
    if !path.is_dir() {
        return Candidate::Foreign;
    }
    let same_repo = rev_parse_dir(path, "--git-common-dir")
        .zip(rev_parse_dir(anchor, "--git-common-dir"))
        .is_some_and(|(candidate, anchor)| candidate == anchor);
    let is_top_level = repo_root(path).is_ok_and(|top| canonical(&top) == canonical(path));
    let same_branch = head_branch(path).is_some_and(|head| head == branch);
    if same_repo && is_top_level && same_branch {
        Candidate::Live
    } else {
        Candidate::Foreign
    }
}

/// The branch checked out at `path`, or `None` when the worktree is detached
/// or the path is not a worktree at all. A detached HEAD is deliberately not
/// a match: the recording names a branch, and resuming onto a detached head
/// would leave the work unreachable by that name.
fn head_branch(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    /// A repository at `root` with one committed file.
    fn init_repo(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                // CI runners have no global identity, and the commit below
                // needs one.
                .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init"]);
        std::fs::write(root.join("f.txt"), "x").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-m", "init"]);
    }

    #[test]
    fn branch_name_carries_the_whole_session_id_not_the_short_prefix() {
        assert_eq!(branch_for(ID), format!("bullpen/{ID}"));
        assert_ne!(
            branch_for("0123456789abcdef00000000deadbeef"),
            branch_for("0123456789abcdef11111111deadbeef")
        );
    }

    #[test]
    fn worktree_dir_lives_under_bullpen_home_not_the_repo() {
        assert_eq!(
            worktree_dir(Path::new("/h/.bullpen"), ID),
            PathBuf::from("/h/.bullpen/worktrees/0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn decide_falls_back_to_the_shared_cwd_only_when_nothing_was_recorded() {
        assert_eq!(decide(None, Candidate::Gone, false), Location::Shared);
    }

    #[test]
    fn decide_uses_the_recorded_directory_when_it_is_still_that_worktree() {
        assert_eq!(
            decide(Some(("/w/a", "bullpen/a")), Candidate::Live, false),
            Location::Use(PathBuf::from("/w/a"))
        );
    }

    #[test]
    fn decide_recreates_from_the_branch_when_only_the_directory_is_gone() {
        assert_eq!(
            decide(Some(("/w/a", "bullpen/a")), Candidate::Gone, true),
            Location::Recreate {
                path: PathBuf::from("/w/a"),
                branch: "bullpen/a".into(),
            }
        );
    }

    #[test]
    fn missing_branch_fails_rather_than_falling_back_to_the_callers_cwd() {
        assert_eq!(
            decide(Some(("/w/a", "bullpen/a")), Candidate::Gone, false),
            Location::Fail {
                path: PathBuf::from("/w/a"),
                branch: "bullpen/a".into(),
            }
        );
    }

    #[test]
    fn a_foreign_directory_at_the_recorded_path_is_refused_even_with_the_branch() {
        assert_eq!(
            decide(Some(("/w/a", "bullpen/a")), Candidate::Foreign, true),
            Location::Occupied {
                path: PathBuf::from("/w/a"),
                branch: "bullpen/a".into(),
            }
        );
    }

    #[test]
    fn non_repo_error_names_the_directory_and_refuses_to_degrade() {
        let dir = tempfile::tempdir().unwrap();
        let err = repo_root(dir.path()).unwrap_err().to_string();
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
        assert!(err.contains("not fall back"), "{err}");
    }

    /// The tests below really shell out: the `--force` recreate path and the
    /// layout of a linked worktree's git dirs are claims about git's
    /// behaviour, not about ours, and asserting them against a stub would
    /// prove nothing.
    #[test]
    fn create_then_recreate_restores_a_deleted_worktree_on_its_branch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);

        let path = dir.path().join("worktrees").join("s1");
        create(&root, &path, "bullpen/s1").unwrap();
        assert!(path.join("f.txt").is_file());
        assert!(branch_exists(&root, "bullpen/s1"));
        // macOS resolves TempDir under /private/var, so compare canonical forms.
        assert_eq!(
            repo_root(&path).unwrap().canonicalize().unwrap(),
            path.canonicalize().unwrap()
        );

        std::fs::remove_dir_all(&path).unwrap();
        recreate(&root, &path, "bullpen/s1").unwrap();
        assert!(path.join("f.txt").is_file());
    }

    #[test]
    fn locate_accepts_only_a_real_worktree_of_the_sessions_own_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        let path = dir.path().join("worktrees").join("s1");
        create(&root, &path, "bullpen/s1").unwrap();
        let recorded = path.display().to_string();
        let at = |anchor: &Path| locate(Some(&recorded), Some("bullpen/s1"), anchor);

        assert_eq!(at(&root), Location::Use(path.clone()));

        // A worktree of some other repository, at the same recorded path.
        let other = dir.path().join("other");
        init_repo(&other);
        assert_eq!(
            at(&other),
            Location::Occupied {
                path: path.clone(),
                branch: "bullpen/s1".into(),
            }
        );

        // An ordinary directory restored where the worktree used to be: the
        // branch still exists, and it is still refused rather than run in.
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::create_dir_all(path.join("f.txt")).unwrap();
        assert_eq!(
            at(&root),
            Location::Occupied {
                path: path.clone(),
                branch: "bullpen/s1".into(),
            }
        );

        std::fs::remove_dir_all(&path).unwrap();
        assert_eq!(
            at(&root),
            Location::Recreate {
                path,
                branch: "bullpen/s1".into(),
            }
        );
    }

    /// The path and the repository can both match while the branch does not.
    /// Removing a session's worktree and putting another one from the same
    /// repository at that path must not capture the session — resuming into
    /// it would commit the agent's work to whatever branch it found.
    #[test]
    fn a_worktree_of_the_same_repo_on_another_branch_is_not_this_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        let path = dir.path().join("worktrees").join("s1");

        create(&root, &path, "bullpen/s1").unwrap();
        assert_eq!(
            locate(path.to_str(), Some("bullpen/s1"), &root),
            Location::Use(path.clone())
        );

        // Same path, same repository, different branch — what a user gets by
        // clearing the directory and reusing the path. `prune` drops git's
        // administrative entry for the deleted worktree, which otherwise
        // still claims the path.
        std::fs::remove_dir_all(&path).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["worktree", "prune"])
                .output()
                .unwrap()
                .status
                .success()
        );
        create(&root, &path, "someone-elses-branch").unwrap();
        assert_eq!(
            locate(path.to_str(), Some("bullpen/s1"), &root),
            Location::Occupied {
                path: path.clone(),
                branch: "bullpen/s1".into()
            }
        );
    }

    /// A regular file at the recorded path is in the way, not absent.
    /// Reporting it as gone sends resume down the recreate path, where
    /// `git worktree add` fails with an error that never mentions the file.
    #[test]
    fn a_regular_file_at_the_recorded_path_is_occupied_not_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        let path = dir.path().join("worktrees").join("s1");

        // The branch exists, so "gone" would mean Recreate.
        create(&root, &path, "bullpen/s1").unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::write(&path, "not a worktree").unwrap();

        assert_eq!(
            locate(path.to_str(), Some("bullpen/s1"), &root),
            Location::Occupied {
                path,
                branch: "bullpen/s1".into()
            }
        );
    }

    /// A dangling symlink is an obstruction that reports itself absent.
    /// `Path::exists` follows the link and says no; `symlink_metadata` sees
    /// the link itself. Getting this wrong sends resume into a recreate that
    /// git refuses, because something does occupy the path.
    #[test]
    fn a_dangling_symlink_at_the_recorded_path_is_occupied_not_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        let path = dir.path().join("worktrees").join("s1");

        // The branch exists, so a wrong "gone" would mean Recreate.
        create(&root, &path, "bullpen/s1").unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &path).unwrap();
        assert!(!path.exists(), "the link dangles");

        assert_eq!(
            locate(path.to_str(), Some("bullpen/s1"), &root),
            Location::Occupied {
                path,
                branch: "bullpen/s1".into()
            }
        );
    }

    /// A detached worktree has no branch to agree with. Accepting it would let
    /// a resumed session commit where the recorded branch name can never reach
    /// the work again. `git symbolic-ref -q HEAD` exits nonzero when detached,
    /// so a check that treats failure as agreement gets this backwards.
    #[test]
    fn a_detached_worktree_at_the_recorded_path_is_not_this_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        let path = dir.path().join("worktrees").join("s1");

        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };
        create(&root, &path, "bullpen/s1").unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        git(&["worktree", "prune"]);
        git(&[
            "worktree",
            "add",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ]);

        assert_eq!(
            locate(path.to_str(), Some("bullpen/s1"), &root),
            Location::Occupied {
                path,
                branch: "bullpen/s1".into()
            }
        );
    }

    #[test]
    fn an_ordinary_checkout_needs_no_write_roots_beyond_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        assert!(git_write_roots(&root).is_empty());
        // Nor does a directory that is not a repository at all.
        assert!(git_write_roots(dir.path()).is_empty());
    }

    #[test]
    fn a_linked_worktree_needs_the_main_repositorys_git_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        let path = dir.path().join("worktrees").join("s1");
        create(&root, &path, "bullpen/s1").unwrap();

        let common = root.join(".git").canonicalize().unwrap();
        assert_eq!(
            git_write_roots(&path),
            vec![common.join("worktrees").join("s1"), common]
        );
    }

    /// Write roots alone do not prove an agent can publish its work; only
    /// running git under the real generated profile does. macOS-only because
    /// that is the platform where the profile is enforced.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_sandboxed_worktree_can_still_commit() {
        // Under $HOME rather than a tempdir: the sandbox deliberately allows
        // writes anywhere under the system temp roots, so a tempdir repo
        // would pass whatever the write roots said.
        let base = std::env::home_dir()
            .unwrap()
            .join(".bullpen-worktree-sandbox-test");
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("repo");
        init_repo(&root);
        let path = base.join("worktrees").join("s1");
        create(&root, &path, "bullpen/s1").unwrap();

        let sandbox =
            bullpen_sandbox::Sandbox::workspace(&path).allowing_writes(git_write_roots(&path));
        let (prog, args) = sandbox
            .wrap_bash("git -c user.email=t@example.com -c user.name=t commit -qam sandboxed");
        std::fs::write(path.join("f.txt"), "edited").unwrap();
        let out = Command::new(prog)
            .args(args)
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");

        std::fs::remove_dir_all(&base).unwrap();
    }
}

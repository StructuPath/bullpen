//! OS-level write confinement for tool execution.
//!
//! bullpen treats the workspace as the boundary the agent may modify. A
//! [`Sandbox`] enforces that two ways, together:
//!
//! - **In-process** ([`Sandbox::allows_write`]), on every platform: the
//!   file-editing tools ask before writing, so a path that escapes the
//!   allowed roots is refused in Rust before any syscall.
//! - **Out-of-process** ([`Sandbox::wrap_bash`]), macOS only today: shell
//!   commands run under Seatbelt (`sandbox-exec`) with a generated profile,
//!   so arbitrary code the model runs — and its children — also cannot write
//!   outside the allowed roots, and optionally cannot reach the network.
//!
//! This is deliberately a **write-confinement** boundary, not a full jail:
//! reads stay broad because compilers and interpreters legitimately read
//! across the system. It closes the specific gap neo and pi leave open — an
//! agent modifying files outside its workspace — with an honest scope.
//!
//! On Linux and other platforms the out-of-process layer is not yet
//! implemented (Landlock is the intended mechanism); [`Sandbox::os_enforced`]
//! reports false there so callers can warn. The in-process check works
//! everywhere.

use std::path::{Path, PathBuf};

/// What a sandboxed process may do beyond reading.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Directory subtrees the agent may write to.
    pub write_roots: Vec<PathBuf>,
    /// Whether sandboxed shell commands may use the network.
    pub allow_network: bool,
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    caps: Capabilities,
}

impl Sandbox {
    /// Default policy: writes confined to the workspace plus the system temp
    /// directories; network allowed (package managers, fetches).
    pub fn workspace(workspace: &Path) -> Self {
        Self::with_capabilities(Capabilities {
            write_roots: default_write_roots(workspace),
            allow_network: true,
        })
    }

    /// Strict policy: same write confinement, network denied.
    pub fn strict(workspace: &Path) -> Self {
        Self::with_capabilities(Capabilities {
            write_roots: default_write_roots(workspace),
            allow_network: false,
        })
    }

    pub fn with_capabilities(caps: Capabilities) -> Self {
        Self { caps }
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Whether out-of-process (shell) confinement is enforced on this OS.
    pub fn os_enforced() -> bool {
        cfg!(target_os = "macos")
    }

    /// Whether writing to `path` is permitted. The path need not exist; its
    /// nearest existing ancestor is resolved (following symlinks) and checked
    /// against the write roots, so `..` and symlink escapes are caught.
    pub fn allows_write(&self, path: &Path) -> bool {
        let resolved = resolve_for_write(path);
        self.caps
            .write_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| resolved.starts_with(&root))
    }

    /// Turn a shell command into the program + args that run it confined.
    /// On macOS: `sandbox-exec -p <profile> bash -c <command>`. Elsewhere:
    /// plain `bash -c <command>` (the in-process check still guards the file
    /// tools; only arbitrary shell writes are unconfined).
    pub fn wrap_bash(&self, command: &str) -> (String, Vec<String>) {
        if cfg!(target_os = "macos") {
            (
                "sandbox-exec".to_string(),
                vec![
                    "-p".to_string(),
                    self.seatbelt_profile(),
                    "bash".to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ],
            )
        } else {
            ("bash".to_string(), vec!["-c".to_string(), command.to_string()])
        }
    }

    /// The generated Seatbelt (SBPL) profile: allow everything, then deny all
    /// writes, then re-allow writes under the canonicalized write roots. The
    /// last matching rule wins in SBPL, so a write inside a root ends on the
    /// re-allow and a write outside ends on the deny.
    pub fn seatbelt_profile(&self) -> String {
        let mut p = String::from("(version 1)\n(allow default)\n");
        if !self.caps.allow_network {
            p.push_str("(deny network*)\n");
        }
        p.push_str("(deny file-write*)\n(allow file-write*\n");
        for root in &self.caps.write_roots {
            // Canonicalize so /tmp → /private/tmp and workspace symlinks match
            // the real paths Seatbelt sees.
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            p.push_str(&format!("  (subpath {})\n", sbpl_quote(&canonical)));
        }
        // Device nodes (ttys, /dev/null) — writing here is normal and safe.
        p.push_str("  (subpath \"/dev\")\n)\n");
        p
    }
}

/// Workspace + the system temp locations tools rely on.
fn default_write_roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![workspace.to_path_buf()];
    for p in ["/tmp", "/private/tmp", "/private/var/folders"] {
        roots.push(PathBuf::from(p));
    }
    if let Ok(tmp) = std::env::var("TMPDIR") {
        roots.push(PathBuf::from(tmp));
    }
    roots
}

/// Resolve a write target to an absolute, symlink-free path for prefix
/// checking. Since the file itself may not exist, resolve the nearest
/// existing ancestor and re-append the trailing components.
fn resolve_for_write(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail = PathBuf::new();
    loop {
        if existing.exists() {
            let base = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
            return base.join(&tail);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail = Path::new(name).join(&tail);
                existing = parent;
            }
            // Reached the root with nothing existing; return as-is.
            _ => return path.to_path_buf(),
        }
    }
}

fn sbpl_quote(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_writes_inside_workspace_denies_outside() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let sandbox = Sandbox::workspace(ws);

        assert!(sandbox.allows_write(&ws.join("file.txt")));
        assert!(sandbox.allows_write(&ws.join("sub/deep/new.rs")));
        // System temp is allowed.
        assert!(sandbox.allows_write(Path::new("/tmp/scratch")));
        // Escapes to genuinely-outside paths are denied. (Note the tempdir
        // itself lives under the temp roots, which are intentionally
        // writable, so escape targets must be truly elsewhere.)
        assert!(!sandbox.allows_write(Path::new("/etc/passwd")));
        assert!(!sandbox.allows_write(Path::new("/usr/lib/thing")));
        assert!(!sandbox.allows_write(&home_path("evil.txt")));
    }

    /// A path under $HOME (outside workspace and temp) for escape tests.
    fn home_path(name: &str) -> PathBuf {
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("/root"))
            .join(name)
    }

    #[test]
    fn symlink_escape_is_denied() {
        // Workspace under a real directory that is NOT inside the temp roots,
        // so the only writable area is the workspace itself.
        let ws = home_path(".bullpen-sandbox-test-ws");
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        // A symlink inside the workspace pointing at a system dir.
        let link = ws.join("link");
        std::os::unix::fs::symlink("/usr", &link).unwrap();

        let sandbox = Sandbox::workspace(&ws);
        assert!(sandbox.allows_write(&ws.join("ok.txt")));
        // Writing through the symlink resolves to /usr — denied.
        assert!(!sandbox.allows_write(&link.join("pwned.txt")));

        std::fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn seatbelt_profile_shape() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::strict(dir.path());
        let profile = sandbox.seatbelt_profile();
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(deny network*)")); // strict
        assert!(profile.contains("(allow file-write*"));
        let canonical = dir.path().canonicalize().unwrap();
        assert!(profile.contains(&canonical.to_string_lossy().to_string()));

        // Non-strict allows network (no deny line).
        assert!(!Sandbox::workspace(dir.path())
            .seatbelt_profile()
            .contains("(deny network*)"));
    }

    #[test]
    fn wrap_bash_targets_the_platform() {
        let dir = tempfile::tempdir().unwrap();
        let (prog, args) = Sandbox::workspace(dir.path()).wrap_bash("echo hi");
        if cfg!(target_os = "macos") {
            assert_eq!(prog, "sandbox-exec");
            assert_eq!(args[0], "-p");
            assert_eq!(args[args.len() - 3], "bash");
            assert_eq!(args[args.len() - 1], "echo hi");
        } else {
            assert_eq!(prog, "bash");
            assert_eq!(args, vec!["-c", "echo hi"]);
        }
    }
}

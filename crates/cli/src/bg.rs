//! Background dispatch: bullpen has no supervisor daemon. Prompts first enter
//! the durable session inbox, then a detached internal worker is kicked using
//! only the session identity. The dashboard reads the store; it never talks to
//! workers directly, which is why workers survive it closing.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::bail;
use bullpen_llm::Message;
use bullpen_store::Store;
use serde_json::Value;

pub struct Dispatch {
    pub input_id: String,
    pub pid: u32,
}

/// Where a background session's stdout/stderr is captured.
pub fn log_path(session_id: &str) -> PathBuf {
    logs_dir().join(format!("{session_id}.log"))
}

fn logs_dir() -> PathBuf {
    bullpen_store::home_dir().join("logs")
}

/// Whether `pid` is a live process. Uses `kill(pid, 0)`: success or an
/// `EPERM` both mean the process exists.
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only the existence/permission
    // check and never delivers a signal.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Durably enqueue one prompt, then always launch a prompt-free worker kick.
/// Concurrent losing kicks safely hand off only to a registered draining
/// worker. A failed kick deliberately leaves the input pending for a later
/// explicit background or dashboard dispatch to kick.
pub fn enqueue_and_spawn(
    store: &mut Store,
    session_id: &str,
    prompt: &str,
    options: &Value,
) -> anyhow::Result<Dispatch> {
    let input_id = uuid::Uuid::new_v4().to_string();
    let message = serde_json::to_value(Message::user_text(prompt))?;
    store.enqueue_input_for_worker(session_id, &input_id, &message, options)?;

    let pid = match spawn_detached_worker(session_id) {
        Ok(pid) => pid,
        Err(error) => bail!(
            "worker kick failed after input {input_id} was durably queued; \
             the input remains pending until another explicit kick: {error}"
        ),
    };

    Ok(Dispatch { input_id, pid })
}

/// Spawn a detached internal worker. Its argv contains no prompt or policy;
/// both are read from the durable inbox after it owns the session lock.
pub fn spawn_detached_worker(session_id: &str) -> std::io::Result<u32> {
    std::fs::create_dir_all(logs_dir())?;
    let log = append_log(&log_path(session_id))?;
    let stderr = log.try_clone()?;
    let exe = std::env::current_exe()?;
    let mut command = detached_worker_command(&exe, session_id);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));

    // Detach into its own session so a closing terminal (SIGHUP) doesn't take
    // the background run down with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid only creates a new session/process group in the
        // child between fork and exec; it touches no parent state.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = command.spawn()?;
    Ok(child.id())
}

fn append_log(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn detached_worker_command(exe: &Path, session_id: &str) -> Command {
    let mut command = Command::new(exe);
    command.arg("session-worker").arg(session_id);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn detached_worker_argv_contains_identity_but_no_prompt() {
        let command = detached_worker_command(Path::new("/tmp/bullpen"), "session-123");
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [OsStr::new("session-worker"), OsStr::new("session-123")]
        );
    }

    #[test]
    fn append_log_does_not_truncate_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker.log");
        std::fs::write(&path, "first\n").unwrap();
        use std::io::Write;
        writeln!(append_log(&path).unwrap(), "second").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "first\nsecond\n");
    }
}

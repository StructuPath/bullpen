//! Background dispatch: bullpen has no supervisor daemon. A background
//! session is just a detached `bullpen run --resume <id>` process that
//! coordinates through the shared SQLite store (WAL-safe across processes).
//! The dashboard reads the store; it never talks to these processes
//! directly — which is why they survive it closing.

use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

/// Spawn a detached `bullpen run --resume <session_id> <prompt>` that outlives
/// this process and the controlling terminal. Returns the child's pid.
/// `extra_args` carries flags like `--sandbox` through to the child.
pub fn spawn_detached(
    session_id: &str,
    prompt: &str,
    extra_args: &[String],
) -> std::io::Result<u32> {
    std::fs::create_dir_all(logs_dir())?;
    let log = File::create(log_path(session_id))?;
    let stderr = log.try_clone()?;
    let exe = std::env::current_exe()?;

    let mut cmd = Command::new(exe);
    cmd.arg("run")
        .arg("--resume")
        .arg(session_id)
        .args(extra_args)
        .arg(prompt)
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
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    Ok(child.id())
}

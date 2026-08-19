//! Deriving a session's dashboard state from its stored run status and
//! whether its process is still alive.
//!
//! The raw `status` column records what the run *intended* (running →
//! completed/failed). Combined with process liveness, that tells the whole
//! story: a session marked `running` whose process is gone crashed and is
//! recoverable, which is different from one that finished cleanly.

use crate::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Never run, or finished and ready for another prompt.
    Idle,
    /// A process is actively working this session.
    Working,
    /// The last run finished successfully.
    Completed,
    /// The last run failed, or its process died mid-run (recoverable).
    Failed,
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Working => "Working",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
        }
    }

    /// Grouping order for the dashboard: Working first, then Idle, then the
    /// finished states.
    pub fn group_rank(self) -> u8 {
        match self {
            Self::Working => 0,
            Self::Idle => 1,
            Self::Failed => 2,
            Self::Completed => 3,
        }
    }
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

/// Derive the display state. `alive` is whether the session's recorded pid is
/// a live process (the caller checks the OS); it only matters while the
/// stored status is `running`.
pub fn derive(status: &str, alive: bool) -> AgentStatus {
    match status {
        "running" if alive => AgentStatus::Working,
        // Marked running but the process is gone → it crashed.
        "running" => AgentStatus::Failed,
        "completed" => AgentStatus::Completed,
        "failed" => AgentStatus::Failed,
        _ => AgentStatus::Idle,
    }
}

pub fn for_session(session: &Session, alive: bool) -> AgentStatus {
    derive(&session.status, alive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_only_matters_while_running() {
        assert_eq!(derive("running", true), AgentStatus::Working);
        assert_eq!(derive("running", false), AgentStatus::Failed); // crashed
        assert_eq!(derive("completed", false), AgentStatus::Completed);
        assert_eq!(derive("failed", true), AgentStatus::Failed);
        assert_eq!(derive("idle", false), AgentStatus::Idle);
        assert_eq!(derive("anything-else", true), AgentStatus::Idle);
    }

    #[test]
    fn group_order_puts_working_first() {
        let mut states = [
            AgentStatus::Completed,
            AgentStatus::Working,
            AgentStatus::Failed,
            AgentStatus::Idle,
        ];
        states.sort_by_key(|s| s.group_rank());
        assert_eq!(
            states,
            [
                AgentStatus::Working,
                AgentStatus::Idle,
                AgentStatus::Failed,
                AgentStatus::Completed
            ]
        );
    }
}

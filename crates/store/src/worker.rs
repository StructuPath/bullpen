//! Exclusive ownership and lifecycle for one persisted session worker.
//!
//! SQLite serializes writes, but it cannot stop two agent processes from
//! building divergent in-memory transcripts and calling providers for the same
//! session. A crash-released OS file lock provides that single-owner invariant;
//! a persisted generation makes terminal status updates stale-safe.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("session {0} already has a running worker")]
    AlreadyRunning(String),
    #[error("worker lock I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("session worker lifecycle already started")]
    AlreadyStarted,
    #[error("session worker lifecycle was not started")]
    NotStarted,
    #[error("session worker generation is no longer current")]
    StaleGeneration,
}

pub struct SessionWorker {
    lock: File,
    db_path: PathBuf,
    session_id: String,
    generation: Option<String>,
}

impl SessionWorker {
    pub fn acquire(db_path: &Path, session_id: &str) -> Result<Self, WorkerError> {
        let run_dir = db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("run");
        std::fs::create_dir_all(&run_dir)?;
        restrict_directory(&run_dir)?;

        let lock_path = run_dir.join(format!("{session_id}.lock"));
        let mut lock = open_lock_file(&lock_path)?;
        if let Err(error) = lock.try_lock_exclusive() {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(WorkerError::AlreadyRunning(
                    session_id[..session_id.len().min(8)].to_string(),
                ));
            }
            return Err(WorkerError::Io(error));
        }

        // Informational only. The OS lock—not this pid text—is ownership.
        lock.set_len(0)?;
        writeln!(lock, "{}", std::process::id())?;

        Ok(Self {
            lock,
            db_path: db_path.to_path_buf(),
            session_id: session_id.to_string(),
            generation: None,
        })
    }

    pub fn start(&mut self, store: &mut Store) -> Result<(), WorkerError> {
        if self.generation.is_some() {
            return Err(WorkerError::AlreadyStarted);
        }
        self.generation = Some(store.start_worker(&self.session_id, std::process::id() as i64)?);
        Ok(())
    }

    pub fn finish(&mut self, status: &str) -> Result<(), WorkerError> {
        let generation = self.generation.as_deref().ok_or(WorkerError::NotStarted)?;
        let mut store = Store::open(&self.db_path)?;
        if !store.finish_worker(&self.session_id, generation, status)? {
            return Err(WorkerError::StaleGeneration);
        }
        self.generation = None;
        Ok(())
    }
}

impl Drop for SessionWorker {
    fn drop(&mut self) {
        if let Some(generation) = self.generation.take()
            && let Ok(mut store) = Store::open(&self.db_path)
        {
            let _ = store.finish_worker(&self.session_id, &generation, "failed");
        }
        let _ = FileExt::unlock(&self.lock);
    }
}

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn restrict_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_second_owner_is_rejected_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bullpen.db");
        let first = SessionWorker::acquire(&db, "session-1").unwrap();
        let error = SessionWorker::acquire(&db, "session-1")
            .err()
            .expect("second owner must fail");
        assert!(matches!(error, WorkerError::AlreadyRunning(_)));

        drop(first);
        SessionWorker::acquire(&db, "session-1").unwrap();
    }

    #[test]
    fn process_exit_releases_the_worker_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bullpen.db");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::tests::lock_helper_process",
                "--ignored",
                "--nocapture",
            ])
            .env("BULLPEN_LOCK_HELPER_DB", &db)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let mut locked = false;
        for line in lines.by_ref() {
            let line = line.unwrap();
            if line.contains("LOCKED") {
                locked = true;
                break;
            }
        }
        assert!(locked, "lock helper exited before acquiring the lock");
        assert!(SessionWorker::acquire(&db, "crash-session").is_err());

        child.kill().unwrap();
        child.wait().unwrap();
        SessionWorker::acquire(&db, "crash-session").unwrap();
    }

    #[test]
    #[ignore = "subprocess helper for process_exit_releases_the_worker_lock"]
    fn lock_helper_process() {
        let Some(db) = std::env::var_os("BULLPEN_LOCK_HELPER_DB") else {
            return;
        };
        let _worker = SessionWorker::acquire(Path::new(&db), "crash-session").unwrap();
        println!("LOCKED");
        std::thread::sleep(Duration::from_secs(60));
    }
}

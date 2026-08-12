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

use crate::{DrainDecision, Store, StoreError};

/// Whether an owned session worker drains the durable input queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerKind {
    NonDraining,
    Draining,
}

impl WorkerKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NonDraining => "non_draining",
            Self::Draining => "draining",
        }
    }

    pub(crate) fn from_str(value: &str) -> Option<Self> {
        match value {
            "non_draining" => Some(Self::NonDraining),
            "draining" => Some(Self::Draining),
            _ => None,
        }
    }
}

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
        match Self::try_acquire(db_path, session_id)? {
            Some(worker) => Ok(worker),
            None => Err(WorkerError::AlreadyRunning(
                session_id[..session_id.len().min(8)].to_string(),
            )),
        }
    }

    /// Ensure a top-level kick either owns the worker lock or safely hands off
    /// to the generation registered by the current lock owner.
    ///
    /// Ensure this kick eventually owns the crash-released session lock.
    ///
    /// Every accepted enqueue launches a kick. Waiting rather than handing off
    /// is necessary because the current worker may fail after the enqueue and
    /// intentionally leave later inputs pending. No SQLite transaction is held
    /// while this process waits on the OS lock.
    pub fn ensure(db_path: &Path, session_id: &str) -> Result<Self, WorkerError> {
        match Self::try_acquire(db_path, session_id)? {
            Some(worker) => Ok(worker),
            None => Self::acquire_blocking(db_path, session_id),
        }
    }

    fn acquire_blocking(db_path: &Path, session_id: &str) -> Result<Self, WorkerError> {
        let mut lock = session_lock_file(db_path, session_id)?;
        lock.lock_exclusive()?;
        record_lock_owner(&mut lock)?;
        Ok(Self::from_lock(lock, db_path, session_id))
    }

    fn try_acquire(db_path: &Path, session_id: &str) -> Result<Option<Self>, WorkerError> {
        let mut lock = session_lock_file(db_path, session_id)?;
        if let Err(error) = lock.try_lock_exclusive() {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(WorkerError::Io(error));
        }

        record_lock_owner(&mut lock)?;
        Ok(Some(Self::from_lock(lock, db_path, session_id)))
    }

    fn from_lock(lock: File, db_path: &Path, session_id: &str) -> Self {
        Self {
            lock,
            db_path: db_path.to_path_buf(),
            session_id: session_id.to_string(),
            generation: None,
        }
    }

    /// Register a direct CLI or pen worker that cannot drain queued inputs.
    pub fn start(&mut self, store: &mut Store) -> Result<(), WorkerError> {
        self.start_as(store, WorkerKind::NonDraining)
    }

    /// Register the hidden queue worker as a safe durable handoff target.
    pub fn start_draining(&mut self, store: &mut Store) -> Result<(), WorkerError> {
        self.start_as(store, WorkerKind::Draining)
    }

    fn start_as(&mut self, store: &mut Store, kind: WorkerKind) -> Result<(), WorkerError> {
        if self.generation.is_some() {
            return Err(WorkerError::AlreadyStarted);
        }
        self.generation =
            Some(store.start_worker(&self.session_id, std::process::id() as i64, kind)?);
        Ok(())
    }

    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Decide whether to drain another input or retire, retaining the file
    /// lock across the decision. A retired worker disarms Drop's fatal finish.
    pub fn drain_or_retire(
        &mut self,
        store: &mut Store,
        terminal_status: &str,
    ) -> Result<DrainDecision, WorkerError> {
        let generation = self.generation.as_deref().ok_or(WorkerError::NotStarted)?;
        match store.drain_or_retire_worker(&self.session_id, generation, terminal_status)? {
            DrainDecision::Pending(input) => Ok(DrainDecision::Pending(input)),
            DrainDecision::Retired => {
                self.generation = None;
                Ok(DrainDecision::Retired)
            }
        }
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

fn session_lock_file(db_path: &Path, session_id: &str) -> std::io::Result<File> {
    let run_dir = db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("run");
    std::fs::create_dir_all(&run_dir)?;
    restrict_directory(&run_dir)?;
    open_lock_file(&run_dir.join(format!("{session_id}.lock")))
}

fn record_lock_owner(lock: &mut File) -> std::io::Result<()> {
    // Informational only. The OS lock—not this pid text—is ownership.
    lock.set_len(0)?;
    writeln!(lock, "{}", std::process::id())
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
    use std::sync::{Arc, Barrier, mpsc};
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
    fn draining_generation_waits_then_acquires_after_release() {
        ensured_waiter_acquires_after_release(Some(WorkerKind::Draining));
    }

    #[test]
    fn direct_generation_waits_then_acquires_after_release() {
        ensured_waiter_acquires_after_release(Some(WorkerKind::NonDraining));
    }

    #[test]
    fn ownership_gap_waits_then_acquires_after_release() {
        ensured_waiter_acquires_after_release(None);
    }

    fn ensured_waiter_acquires_after_release(kind: Option<WorkerKind>) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bullpen.db");
        let mut store = Store::open(&db).unwrap();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        let mut owner = SessionWorker::acquire(&db, &session.id).unwrap();
        match kind {
            Some(WorkerKind::NonDraining) => owner.start(&mut store).unwrap(),
            Some(WorkerKind::Draining) => owner.start_draining(&mut store).unwrap(),
            None => {}
        }

        let waiter_db = db.clone();
        let waiter_session = session.id.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            SessionWorker::ensure(&waiter_db, &waiter_session).unwrap()
        });
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!waiter.is_finished());

        if kind.is_some() {
            owner.finish("completed").unwrap();
        }
        drop(owner);

        drop(waiter.join().unwrap());
    }

    #[test]
    fn concurrent_ensures_serialize_two_owners() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bullpen.db");
        let store = Store::open(&db).unwrap();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        drop(store);

        let barrier = Arc::new(Barrier::new(3));
        let (result_tx, result_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let db = db.clone();
                let session_id = session.id.clone();
                let barrier = barrier.clone();
                let result_tx = result_tx.clone();
                let release_rx = release_rx.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let mut worker = SessionWorker::ensure(&db, &session_id).unwrap();
                    let mut store = Store::open(&db).unwrap();
                    worker.start_draining(&mut store).unwrap();
                    result_tx.send("acquired").unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                    worker.finish("completed").unwrap();
                })
            })
            .collect();
        barrier.wait();

        assert_eq!(result_rx.recv().unwrap(), "acquired");
        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        assert_eq!(result_rx.recv().unwrap(), "acquired");
        release_tx.send(()).unwrap();
        for handle in handles {
            handle.join().unwrap();
        }
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

//! Durable local state.
//!
//! One SQLite database in WAL mode. Schema v3 implements the durable
//! execution model from ARCHITECTURE.md ("Persistence and durable
//! execution"):
//!
//! - **entries**: the conversation, an append-only tree (`id`/`parent_id`)
//!   per session with a per-lane leaf pointer. What the model sees.
//! - **records**: the execution log (operation/step/tool intents and
//!   outcomes). Never enters model context; nothing reads it during normal
//!   execution. Deleting every record leaves a valid conversation.
//!
//! Both share one per-session monotonic `seq` allocated inside the write.
//! Appends are idempotent on the caller-provisioned id, which is what makes
//! re-running recovery safe.

mod recovery;
pub mod status;

pub use recovery::{Recovery, recover};
pub use status::AgentStatus;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bullpen_llm::{Message, Role, Usage};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAIN_LANE: &str = "main";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no session matches `{0}`")]
    NotFound(String),
    #[error("session id `{0}` is ambiguous")]
    Ambiguous(String),
    #[error("corrupt session state: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub created_at: String,
    pub updated_at: String,
    /// Set for pen children; the parent session's id.
    pub parent_session_id: Option<String>,
    /// Last recorded run status: idle | running | completed | failed.
    pub status: String,
    /// OS process id of the last run, for liveness checks.
    pub pid: Option<i64>,
}

/// One conversation entry. `payload` holds a [`Message`] for kind
/// `"message"`; future kinds (compaction) carry their own payloads.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub parent_id: Option<String>,
    pub seq: i64,
    pub kind: String,
    pub payload: Value,
}

/// An execution record. `run_id` is the owning operation's id (which is the
/// `operation_started` record's own id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub run_id: String,
    pub seq: i64,
    pub kind: String,
    pub payload: Value,
}

/// A suspended run discovered by reduction: an `operation_started` with no
/// matching `operation_finished`.
#[derive(Debug, Clone)]
pub struct OpenRun {
    pub run_id: String,
    pub source_leaf_id: Option<String>,
    pub records: Vec<Record>,
}

/// The directory holding bullpen's own state: `$BULLPEN_HOME` when set,
/// otherwise `~/.bullpen`.
pub fn home_dir() -> PathBuf {
    resolve_home(std::env::var_os("BULLPEN_HOME"), std::env::home_dir())
}

/// `bullpen_home` is `$BULLPEN_HOME` and `home` is `$HOME` (the caller reads
/// the environment). An empty `BULLPEN_HOME` counts as unset: taken
/// literally it would put the whole store in whatever directory the process
/// happened to start in.
fn resolve_home(bullpen_home: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    match bullpen_home {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.unwrap_or_else(|| PathBuf::from(".")).join(".bullpen"),
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Wait, rather than fail, when another writer holds the lock.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn default_path() -> PathBuf {
        home_dir().join("bullpen.db")
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let version: i64 = self
            .conn
            .query_row("SELECT * FROM pragma_user_version", [], |r| r.get(0))?;
        if version < 1 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE sessions (
                    id            TEXT PRIMARY KEY,
                    title         TEXT NOT NULL DEFAULT '',
                    cwd           TEXT NOT NULL,
                    model         TEXT NOT NULL,
                    input_tokens  INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE messages (
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    idx        INTEGER NOT NULL,
                    role       TEXT NOT NULL,
                    content    TEXT NOT NULL,
                    PRIMARY KEY (session_id, idx)
                );
                PRAGMA user_version = 1;",
            )?;
            tx.commit()?;
        }
        if version < 2 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN provider TEXT NOT NULL DEFAULT 'anthropic';
                 PRAGMA user_version = 2;",
            )?;
            tx.commit()?;
        }
        if version < 3 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN next_seq INTEGER NOT NULL DEFAULT 1;
                 CREATE TABLE entries (
                    id         TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    parent_id  TEXT,
                    seq        INTEGER NOT NULL,
                    kind       TEXT NOT NULL,
                    payload    TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    UNIQUE (session_id, seq)
                 );
                 CREATE INDEX entries_by_session ON entries(session_id, parent_id);
                 CREATE TABLE records (
                    id         TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    lane       TEXT NOT NULL DEFAULT 'main',
                    run_id     TEXT NOT NULL,
                    seq        INTEGER NOT NULL,
                    kind       TEXT NOT NULL,
                    payload    TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    UNIQUE (session_id, seq)
                 );
                 CREATE INDEX records_by_run ON records(session_id, run_id);
                 CREATE TABLE lanes (
                    session_id        TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    name              TEXT NOT NULL DEFAULT 'main',
                    leaf_id           TEXT,
                    open_operation_id TEXT,
                    PRIMARY KEY (session_id, name)
                 );",
            )?;
            migrate_messages_to_entries(&tx)?;
            tx.execute_batch("DROP TABLE messages; PRAGMA user_version = 3;")?;
            tx.commit()?;
        }
        if version < 4 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN parent_session_id TEXT;
                 PRAGMA user_version = 4;",
            )?;
            tx.commit()?;
        }
        if version < 5 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN status TEXT NOT NULL DEFAULT 'idle';
                 ALTER TABLE sessions ADD COLUMN pid INTEGER;
                 PRAGMA user_version = 5;",
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    // ── Sessions ────────────────────────────────────────────────────────────

    pub fn create_session(
        &self,
        cwd: &str,
        provider: &str,
        model: &str,
    ) -> Result<Session, StoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO sessions (id, cwd, provider, model) VALUES (?1, ?2, ?3, ?4)",
            params![id, cwd, provider, model],
        )?;
        self.conn.execute(
            "INSERT INTO lanes (session_id, name) VALUES (?1, ?2)",
            params![id, MAIN_LANE],
        )?;
        self.get_session(&id)
    }

    /// Create a pen child with a caller-provided (deterministic) id.
    /// Idempotent: if the id already exists, the existing session is
    /// returned — this is how a replayed spawn reattaches to its child.
    pub fn create_child_session(
        &self,
        child_id: &str,
        parent_id: &str,
        cwd: &str,
        provider: &str,
        model: &str,
    ) -> Result<Session, StoreError> {
        if let Ok(existing) = self.get_session(child_id) {
            return Ok(existing);
        }
        self.conn.execute(
            "INSERT INTO sessions (id, cwd, provider, model, parent_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![child_id, cwd, provider, model, parent_id],
        )?;
        self.conn.execute(
            "INSERT INTO lanes (session_id, name) VALUES (?1, ?2)",
            params![child_id, MAIN_LANE],
        )?;
        self.get_session(child_id)
    }

    /// How many children a session has spawned — the durable count budget.
    pub fn count_children(&self, parent_id: &str) -> Result<u64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE parent_session_id = ?1",
            params![parent_id],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// Outcome of the most recent finished operation, if any.
    pub fn last_run_outcome(&self, session_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT payload FROM records
                 WHERE session_id = ?1 AND kind = 'operation_finished'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|p| {
                serde_json::from_str::<Value>(&p)
                    .ok()?
                    .get("outcome")?
                    .as_str()
                    .map(str::to_string)
            }))
    }

    /// Record the run status (and process id) for the dashboard.
    pub fn set_run_status(
        &self,
        session_id: &str,
        status: &str,
        pid: Option<i64>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET status = ?2, pid = ?3, updated_at = datetime('now')
             WHERE id = ?1",
            params![session_id, status, pid],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> Result<Session, StoreError> {
        self.conn
            .query_row(
                "SELECT id, title, cwd, provider, model, input_tokens, output_tokens,
                        created_at, updated_at, parent_session_id, status, pid
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(id.to_string()),
                other => StoreError::Db(other),
            })
    }

    pub fn resolve_session(&self, prefix: &str) -> Result<Session, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, cwd, provider, model, input_tokens, output_tokens,
                    created_at, updated_at, parent_session_id, status, pid
             FROM sessions WHERE id LIKE ?1 || '%' LIMIT 2",
        )?;
        let mut matches: Vec<Session> = stmt
            .query_map(params![prefix], row_to_session)?
            .collect::<Result<_, _>>()?;
        match matches.len() {
            0 => Err(StoreError::NotFound(prefix.to_string())),
            1 => Ok(matches.remove(0)),
            _ => Err(StoreError::Ambiguous(prefix.to_string())),
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, cwd, provider, model, input_tokens, output_tokens,
                    created_at, updated_at, parent_session_id, status, pid
             FROM sessions ORDER BY updated_at DESC",
        )?;
        Ok(stmt
            .query_map([], row_to_session)?
            .collect::<Result<_, _>>()?)
    }

    /// Roll cumulative usage and (first-user-message) title forward.
    pub fn update_session_meta(
        &self,
        session_id: &str,
        usage: Usage,
        title: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions
             SET input_tokens = ?2, output_tokens = ?3,
                 title = CASE WHEN title = '' THEN ?4 ELSE title END,
                 updated_at = datetime('now')
             WHERE id = ?1",
            params![
                session_id,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                title
            ],
        )?;
        Ok(())
    }

    // ── Entries (the conversation tree) ─────────────────────────────────────

    /// Append an entry with a caller-provisioned id at the lane leaf and
    /// advance the leaf. Idempotent: if the id already exists, nothing is
    /// written. Callers never pass a parent — the leaf is the parent.
    pub fn append_entry(
        &mut self,
        session_id: &str,
        provisioned_id: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<(), StoreError> {
        // IMMEDIATE: take the write lock up front so a concurrent writer makes
        // us wait (honoring busy_timeout) rather than failing with
        // SQLITE_BUSY_SNAPSHOT when a deferred read-then-write upgrade races.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)",
            params![provisioned_id],
            |r| r.get(0),
        )?;
        if !exists {
            let parent: Option<String> = tx.query_row(
                "SELECT leaf_id FROM lanes WHERE session_id = ?1 AND name = ?2",
                params![session_id, MAIN_LANE],
                |r| r.get(0),
            )?;
            let seq = next_seq(&tx, session_id)?;
            tx.execute(
                "INSERT INTO entries (id, session_id, parent_id, seq, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    provisioned_id,
                    session_id,
                    parent,
                    seq,
                    kind,
                    payload.to_string()
                ],
            )?;
            tx.execute(
                "UPDATE lanes SET leaf_id = ?3 WHERE session_id = ?1 AND name = ?2",
                params![session_id, MAIN_LANE, provisioned_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn leaf(&self, session_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT leaf_id FROM lanes WHERE session_id = ?1 AND name = ?2",
                params![session_id, MAIN_LANE],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn entry_exists(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// The active path, root → leaf.
    pub fn path(&self, session_id: &str) -> Result<Vec<Entry>, StoreError> {
        let mut by_id = std::collections::HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT id, parent_id, seq, kind, payload FROM entries WHERE session_id = ?1",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    parent_id: row.get(1)?,
                    seq: row.get(2)?,
                    kind: row.get(3)?,
                    payload: serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or(Value::Null),
                })
            })?;
            for row in rows {
                let e = row?;
                by_id.insert(e.id.clone(), e);
            }
        }
        let mut path = Vec::new();
        let mut cursor = self.leaf(session_id)?;
        while let Some(id) = cursor {
            let entry = by_id.remove(&id).ok_or_else(|| {
                StoreError::Corrupt(format!("leaf path references missing entry {id}"))
            })?;
            cursor = entry.parent_id.clone();
            path.push(entry);
        }
        path.reverse();
        Ok(path)
    }

    /// The active path as provider messages (kind `"message"` only).
    pub fn path_messages(&self, session_id: &str) -> Result<Vec<Message>, StoreError> {
        self.path(session_id)?
            .into_iter()
            .filter(|e| e.kind == "message")
            .map(|e| serde_json::from_value(e.payload).map_err(StoreError::from))
            .collect()
    }

    // ── Records (the execution log) ─────────────────────────────────────────

    /// Append an execution record. Idempotent on `id`.
    pub fn append_record(
        &mut self,
        session_id: &str,
        id: &str,
        run_id: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM records WHERE id = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if !exists {
            let seq = next_seq(&tx, session_id)?;
            tx.execute(
                "INSERT INTO records (id, session_id, lane, run_id, seq, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    session_id,
                    MAIN_LANE,
                    run_id,
                    seq,
                    kind,
                    payload.to_string()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Open a new operation: `operation_started` record (its id IS the run
    /// id) plus the lane's open-operation marker.
    pub fn start_operation(
        &mut self,
        session_id: &str,
        payload: &Value,
    ) -> Result<String, StoreError> {
        if self.open_run(session_id)?.is_some() {
            return Err(StoreError::Corrupt(
                "cannot start an operation while one is open (recover first)".into(),
            ));
        }
        let run_id = uuid::Uuid::new_v4().to_string();
        let mut payload = payload.clone();
        payload["source_leaf_id"] = match self.leaf(session_id)? {
            Some(id) => Value::String(id),
            None => Value::Null,
        };
        self.append_record(
            session_id,
            &run_id.clone(),
            &run_id,
            "operation_started",
            &payload,
        )?;
        self.conn.execute(
            "UPDATE lanes SET open_operation_id = ?3 WHERE session_id = ?1 AND name = ?2",
            params![session_id, MAIN_LANE, run_id],
        )?;
        Ok(run_id)
    }

    /// Close an operation and clear the lane marker. Idempotent.
    pub fn finish_operation(
        &mut self,
        session_id: &str,
        run_id: &str,
        outcome: &str,
    ) -> Result<(), StoreError> {
        let finish_id = format!("{run_id}:finished");
        self.append_record(
            session_id,
            &finish_id,
            run_id,
            "operation_finished",
            &serde_json::json!({ "outcome": outcome }),
        )?;
        self.conn.execute(
            "UPDATE lanes SET open_operation_id = NULL
             WHERE session_id = ?1 AND name = ?2 AND open_operation_id = ?3",
            params![session_id, MAIN_LANE, run_id],
        )?;
        Ok(())
    }

    /// Reduction, coarse level: the lane's execution state derived from
    /// records. 0 open operations = idle (None); 1 = suspended (Some);
    /// more = corruption from a state a single writer cannot produce.
    pub fn open_run(&self, session_id: &str) -> Result<Option<OpenRun>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM records
             WHERE session_id = ?1 AND kind = 'operation_started'
               AND id NOT IN (
                   SELECT run_id FROM records
                   WHERE session_id = ?1 AND kind = 'operation_finished')
             ORDER BY seq LIMIT 3",
        )?;
        let open: Vec<String> = stmt
            .query_map(params![session_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        match open.len() {
            0 => Ok(None),
            1 => {
                let run_id = &open[0];
                let mut stmt = self.conn.prepare(
                    "SELECT id, run_id, seq, kind, payload FROM records
                     WHERE session_id = ?1 AND run_id = ?2 ORDER BY seq",
                )?;
                let records: Vec<Record> = stmt
                    .query_map(params![session_id, run_id], |row| {
                        Ok(Record {
                            id: row.get(0)?,
                            run_id: row.get(1)?,
                            seq: row.get(2)?,
                            kind: row.get(3)?,
                            payload: serde_json::from_str(&row.get::<_, String>(4)?)
                                .unwrap_or(Value::Null),
                        })
                    })?
                    .collect::<Result<_, _>>()?;
                let source_leaf_id = records
                    .first()
                    .and_then(|r| r.payload.get("source_leaf_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Ok(Some(OpenRun {
                    run_id: run_id.clone(),
                    source_leaf_id,
                    records,
                }))
            }
            n => Err(StoreError::Corrupt(format!(
                "{n}+ open operations on one lane — impossible for a single writer"
            ))),
        }
    }
}

/// Allocate the next per-session sequence number inside the caller's
/// transaction. Callers never read, reserve, or increment sequences.
fn next_seq(tx: &rusqlite::Transaction<'_>, session_id: &str) -> Result<i64, StoreError> {
    let seq: i64 = tx.query_row(
        "UPDATE sessions SET next_seq = next_seq + 1 WHERE id = ?1 RETURNING next_seq - 1",
        params![session_id],
        |r| r.get(0),
    )?;
    Ok(seq)
}

/// v2 → v3: turn each session's flat message list into an entry chain and
/// point the lane leaf at the last one.
fn migrate_messages_to_entries(tx: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    let sessions: Vec<String> = tx
        .prepare("SELECT id FROM sessions")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;

    for session_id in sessions {
        let rows: Vec<(String, String)> = tx
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY idx")?
            .query_map(params![session_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;

        let mut parent: Option<String> = None;
        for (role, content) in rows {
            let payload = serde_json::json!({
                "role": if role == "user" { "user" } else { "assistant" },
                "content": serde_json::from_str::<Value>(&content)?,
            });
            let id = uuid::Uuid::new_v4().to_string();
            let seq = next_seq(tx, &session_id)?;
            tx.execute(
                "INSERT INTO entries (id, session_id, parent_id, seq, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, 'message', ?5)",
                params![id, session_id, parent, seq, payload.to_string()],
            )?;
            parent = Some(id);
        }
        tx.execute(
            "INSERT INTO lanes (session_id, name, leaf_id) VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id, name) DO UPDATE SET leaf_id = excluded.leaf_id",
            params![session_id, MAIN_LANE, parent],
        )?;
    }
    Ok(())
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        cwd: row.get(2)?,
        provider: row.get(3)?,
        model: row.get(4)?,
        usage: Usage {
            input_tokens: row.get::<_, i64>(5)? as u64,
            output_tokens: row.get::<_, i64>(6)? as u64,
        },
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        parent_session_id: row.get(9)?,
        status: row.get(10)?,
        pid: row.get(11)?,
    })
}

/// Extract a display title from the first user message on the path.
pub fn title_from(messages: &[Message]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.text().chars().take(80).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bullpen_llm::ContentBlock;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        (dir, store)
    }

    fn msg_payload(m: &Message) -> Value {
        serde_json::to_value(m).unwrap()
    }

    #[test]
    fn unset_bullpen_home_still_resolves_under_dot_bullpen() {
        assert_eq!(
            resolve_home(None, Some(PathBuf::from("/h"))).join("bullpen.db"),
            PathBuf::from("/h/.bullpen/bullpen.db")
        );
        // No $HOME either — the long-standing relative fallback.
        assert_eq!(
            resolve_home(None, None).join("bullpen.db"),
            PathBuf::from("./.bullpen/bullpen.db")
        );
    }

    #[test]
    fn bullpen_home_overrides_the_default_directory() {
        assert_eq!(
            resolve_home(Some("/tmp/pen".into()), Some(PathBuf::from("/h"))).join("bullpen.db"),
            PathBuf::from("/tmp/pen/bullpen.db")
        );
        assert_eq!(
            resolve_home(Some("".into()), Some(PathBuf::from("/h"))).join("bullpen.db"),
            PathBuf::from("/h/.bullpen/bullpen.db")
        );
    }

    #[test]
    fn entry_chain_roundtrip() {
        let (_dir, mut store) = store();
        let s = store.create_session("/tmp", "anthropic", "m").unwrap();

        let m1 = Message::user_text("hello");
        let m2 = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        };
        store
            .append_entry(&s.id, "e1", "message", &msg_payload(&m1))
            .unwrap();
        store
            .append_entry(&s.id, "e2", "message", &msg_payload(&m2))
            .unwrap();

        assert_eq!(store.leaf(&s.id).unwrap().as_deref(), Some("e2"));
        let messages = store.path_messages(&s.id).unwrap();
        assert_eq!(messages, vec![m1, m2]);
    }

    #[test]
    fn provisioned_append_is_idempotent() {
        let (_dir, mut store) = store();
        let s = store.create_session("/tmp", "anthropic", "m").unwrap();
        let payload = msg_payload(&Message::user_text("once"));
        store
            .append_entry(&s.id, "e1", "message", &payload)
            .unwrap();
        store
            .append_entry(&s.id, "e1", "message", &payload)
            .unwrap();
        assert_eq!(store.path(&s.id).unwrap().len(), 1);

        store
            .append_record(&s.id, "r1", "run", "step_attempt", &json!({"n": 1}))
            .unwrap();
        store
            .append_record(&s.id, "r1", "run", "step_attempt", &json!({"n": 1}))
            .unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn operation_lifecycle_and_reduction() {
        let (_dir, mut store) = store();
        let s = store.create_session("/tmp", "anthropic", "m").unwrap();
        assert!(store.open_run(&s.id).unwrap().is_none());

        let run = store
            .start_operation(&s.id, &json!({"prompt": "go"}))
            .unwrap();
        let open = store.open_run(&s.id).unwrap().expect("suspended");
        assert_eq!(open.run_id, run);
        assert_eq!(open.records.len(), 1);

        // A second start while open is refused, not silently allowed.
        assert!(matches!(
            store.start_operation(&s.id, &json!({})),
            Err(StoreError::Corrupt(_))
        ));

        store.finish_operation(&s.id, &run, "completed").unwrap();
        assert!(store.open_run(&s.id).unwrap().is_none());
        // Finishing again is idempotent.
        store.finish_operation(&s.id, &run, "completed").unwrap();
    }

    #[test]
    fn seq_is_shared_and_monotonic_across_tables() {
        let (_dir, mut store) = store();
        let s = store.create_session("/tmp", "anthropic", "m").unwrap();
        store
            .append_entry(
                &s.id,
                "e1",
                "message",
                &msg_payload(&Message::user_text("a")),
            )
            .unwrap();
        let run = store.start_operation(&s.id, &json!({})).unwrap();
        store
            .append_entry(
                &s.id,
                "e2",
                "message",
                &msg_payload(&Message::user_text("b")),
            )
            .unwrap();

        let entry_seqs: Vec<i64> = store.path(&s.id).unwrap().iter().map(|e| e.seq).collect();
        let record_seq = store.open_run(&s.id).unwrap().unwrap().records[0].seq;
        assert_eq!(entry_seqs, vec![1, 3]);
        assert_eq!(record_seq, 2);
        store.finish_operation(&s.id, &run, "completed").unwrap();
    }

    #[test]
    fn migrates_v2_sessions_into_entry_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        // Build a v2 database by hand.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '',
                    cwd TEXT NOT NULL, model TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    provider TEXT NOT NULL DEFAULT 'anthropic'
                 );
                 CREATE TABLE messages (
                    session_id TEXT NOT NULL, idx INTEGER NOT NULL,
                    role TEXT NOT NULL, content TEXT NOT NULL,
                    PRIMARY KEY (session_id, idx)
                 );
                 INSERT INTO sessions (id, cwd, model) VALUES ('s1', '/tmp', 'm');
                 PRAGMA user_version = 2;",
            )
            .unwrap();
            let content =
                serde_json::to_string(&vec![ContentBlock::Text { text: "hi".into() }]).unwrap();
            conn.execute(
                "INSERT INTO messages (session_id, idx, role, content) VALUES ('s1', 0, 'user', ?1)",
                params![content],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (session_id, idx, role, content) VALUES ('s1', 1, 'assistant', ?1)",
                params![content],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let messages = store.path_messages("s1").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(store.open_run("s1").unwrap().is_none());
    }
}

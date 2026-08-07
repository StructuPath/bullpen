//! Durable local state.
//!
//! One SQLite database in WAL mode replaces neo-style per-session JSON files.
//! WAL gives cross-process safety: concurrent bullpen processes share the
//! store without losing updates (neo's known limitation #2). The schema is
//! versioned via `user_version` and will grow tables for agent runs and
//! workflow state in later milestones.

use std::path::{Path, PathBuf};

use bullpen_llm::{ContentBlock, Message, Role, Usage};
use rusqlite::{Connection, params};

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
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub usage: Usage,
    pub created_at: String,
    pub updated_at: String,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// The default store location: `~/.bullpen/bullpen.db`.
    pub fn default_path() -> PathBuf {
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".bullpen")
            .join("bullpen.db")
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let version: i64 =
            self.conn
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
        Ok(())
    }

    pub fn create_session(&self, cwd: &str, model: &str) -> Result<Session, StoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO sessions (id, cwd, model) VALUES (?1, ?2, ?3)",
            params![id, cwd, model],
        )?;
        self.get_session(&id)
    }

    /// Replace the stored transcript for a session in one transaction and
    /// roll the usage/title/updated_at metadata forward.
    pub fn save_transcript(
        &mut self,
        session_id: &str,
        messages: &[Message],
        usage: Usage,
    ) -> Result<(), StoreError> {
        let title = messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| {
                let t = m.text();
                t.chars().take(80).collect::<String>()
            })
            .unwrap_or_default();

        let tx = self.conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            params![session_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StoreError::NotFound(session_id.to_string()));
        }
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO messages (session_id, idx, role, content) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (idx, msg) in messages.iter().enumerate() {
                let role = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let content = serde_json::to_string(&msg.content)?;
                insert.execute(params![session_id, idx as i64, role, content])?;
            }
        }
        tx.execute(
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
        tx.commit()?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> Result<Session, StoreError> {
        self.conn
            .query_row(
                "SELECT id, title, cwd, model, input_tokens, output_tokens,
                        created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(id.to_string()),
                other => StoreError::Db(other),
            })
    }

    /// Resolve a full id or unique id prefix.
    pub fn resolve_session(&self, prefix: &str) -> Result<Session, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, cwd, model, input_tokens, output_tokens,
                    created_at, updated_at
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

    pub fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let role: String = row.get(0)?;
            let content: String = row.get(1)?;
            Ok((role, content))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (role, content) = row?;
            let blocks: Vec<ContentBlock> = serde_json::from_str(&content)?;
            messages.push(Message {
                role: if role == "user" { Role::User } else { Role::Assistant },
                content: blocks,
            });
        }
        Ok(messages)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, cwd, model, input_tokens, output_tokens,
                    created_at, updated_at
             FROM sessions ORDER BY updated_at DESC",
        )?;
        let sessions = stmt
            .query_map([], row_to_session)?
            .collect::<Result<_, _>>()?;
        Ok(sessions)
    }
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        cwd: row.get(2)?,
        model: row.get(3)?,
        usage: Usage {
            input_tokens: row.get::<_, i64>(4)? as u64,
            output_tokens: row.get::<_, i64>(5)? as u64,
        },
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        (dir, store)
    }

    fn sample_messages() -> Vec<Message> {
        vec![
            Message::user_text("fix the bug"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text { text: "on it".into() },
                    ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
            },
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: "main.rs".into(),
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn transcript_roundtrip_preserves_blocks() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "claude-sonnet-5").unwrap();
        let messages = sample_messages();
        store
            .save_transcript(
                &session.id,
                &messages,
                Usage { input_tokens: 100, output_tokens: 50 },
            )
            .unwrap();

        let loaded = store.load_transcript(&session.id).unwrap();
        assert_eq!(loaded, messages);

        let session = store.get_session(&session.id).unwrap();
        assert_eq!(session.usage.input_tokens, 100);
        assert_eq!(session.title, "fix the bug");
    }

    #[test]
    fn resave_replaces_not_duplicates() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "m").unwrap();
        let mut messages = sample_messages();
        store
            .save_transcript(&session.id, &messages, Usage::default())
            .unwrap();
        messages.push(Message::user_text("and another thing"));
        store
            .save_transcript(&session.id, &messages, Usage::default())
            .unwrap();
        assert_eq!(store.load_transcript(&session.id).unwrap().len(), 4);
    }

    #[test]
    fn prefix_resolution() {
        let (_dir, store) = store();
        let s = store.create_session("/tmp", "m").unwrap();
        let found = store.resolve_session(&s.id[..8]).unwrap();
        assert_eq!(found.id, s.id);
        assert!(matches!(
            store.resolve_session("zzzz"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn save_to_unknown_session_fails() {
        let (_dir, mut store) = store();
        let err = store
            .save_transcript("nope", &sample_messages(), Usage::default())
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[test]
    fn two_connections_share_the_store() {
        // The WAL claim in miniature: a second connection opened on the same
        // file sees the first one's committed writes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.db");
        let mut a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();

        let s = a.create_session("/tmp", "m").unwrap();
        a.save_transcript(&s.id, &sample_messages(), Usage::default())
            .unwrap();
        assert_eq!(b.list_sessions().unwrap().len(), 1);
        assert_eq!(b.load_transcript(&s.id).unwrap().len(), 3);
    }
}

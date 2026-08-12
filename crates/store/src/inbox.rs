//! Durable, ordered inputs waiting to become session operations.

use bullpen_llm::Message;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::{MAIN_LANE, Store, StoreError, next_seq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInputState {
    Pending,
    Started,
}

#[derive(Debug, Clone)]
pub struct SessionInput {
    /// Caller-provisioned idempotency id.
    pub id: String,
    pub session_id: String,
    pub position: i64,
    pub prompt: Value,
    pub state: SessionInputState,
    pub operation_id: Option<String>,
    pub enqueued_at: String,
    pub started_at: Option<String>,
}

impl Store {
    /// Durably enqueue one input. Reusing an id for the same session and
    /// payload returns the original row without allocating another position.
    pub fn enqueue_input(
        &mut self,
        session_id: &str,
        provisioned_id: &str,
        prompt: &Value,
    ) -> Result<SessionInput, StoreError> {
        validate_user_prompt(prompt)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        if let Some(existing) = query_input_by_id(&tx, provisioned_id)? {
            if existing.session_id != session_id || existing.prompt != *prompt {
                return Err(StoreError::Corrupt(format!(
                    "session input id `{provisioned_id}` was reused with different input"
                )));
            }
            tx.commit()?;
            return Ok(existing);
        }

        let position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1
             FROM session_inputs WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO session_inputs (id, session_id, position, prompt)
             VALUES (?1, ?2, ?3, ?4)",
            params![provisioned_id, session_id, position, prompt.to_string()],
        )?;
        let input = query_input_by_id(&tx, provisioned_id)?.ok_or_else(|| {
            StoreError::Corrupt(format!("new session input `{provisioned_id}` disappeared"))
        })?;
        tx.commit()?;
        Ok(input)
    }

    /// All durable inputs for a session in their allocated FIFO order.
    pub fn list_inputs(&self, session_id: &str) -> Result<Vec<SessionInput>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, position, prompt, state, operation_id,
                    enqueued_at, started_at
             FROM session_inputs WHERE session_id = ?1 ORDER BY position",
        )?;
        Ok(stmt
            .query_map(params![session_id], row_to_input)?
            .collect::<Result<_, _>>()?)
    }

    /// Inspect one provisioned input id within its session.
    pub fn inspect_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<SessionInput, StoreError> {
        self.conn
            .query_row(
                "SELECT id, session_id, position, prompt, state, operation_id,
                        enqueued_at, started_at
                 FROM session_inputs WHERE session_id = ?1 AND id = ?2",
                params![session_id, input_id],
                row_to_input,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(input_id.to_string()),
                other => StoreError::Db(other),
            })
    }

    /// Start `expected_input_id` only if it is the session's oldest pending
    /// input. The state transition, operation record, user entry, input link,
    /// and both lane pointers commit atomically.
    pub fn start_oldest_pending_input(
        &mut self,
        session_id: &str,
        expected_input_id: &str,
    ) -> Result<Option<SessionInput>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let Some(mut input) = tx
            .query_row(
                "SELECT id, session_id, position, prompt, state, operation_id,
                        enqueued_at, started_at
                 FROM session_inputs
                 WHERE session_id = ?1 AND state = 'pending'
                 ORDER BY position LIMIT 1",
                params![session_id],
                row_to_input,
            )
            .optional()?
        else {
            tx.commit()?;
            return Ok(None);
        };
        if input.id != expected_input_id {
            return Err(StoreError::Corrupt(format!(
                "session input `{expected_input_id}` is not the oldest pending input"
            )));
        }

        let open_operations: i64 = tx.query_row(
            "SELECT COUNT(*) FROM records started
             WHERE started.session_id = ?1
               AND started.kind = 'operation_started'
               AND NOT EXISTS (
                   SELECT 1 FROM records finished
                   WHERE finished.session_id = started.session_id
                     AND finished.run_id = started.id
                     AND finished.kind = 'operation_finished'
               )",
            params![session_id],
            |row| row.get(0),
        )?;
        if open_operations != 0 {
            return Err(StoreError::Corrupt(
                "cannot start a session input while an operation is open (recover first)".into(),
            ));
        }

        let source_leaf_id: Option<String> = tx.query_row(
            "SELECT leaf_id FROM lanes WHERE session_id = ?1 AND name = ?2",
            params![session_id, MAIN_LANE],
            |row| row.get(0),
        )?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let operation_seq = next_seq(&tx, session_id)?;
        tx.execute(
            "INSERT INTO records (id, session_id, lane, run_id, seq, kind, payload)
             VALUES (?1, ?2, ?3, ?1, ?4, 'operation_started', ?5)",
            params![
                run_id,
                session_id,
                MAIN_LANE,
                operation_seq,
                serde_json::json!({
                    "source_leaf_id": source_leaf_id,
                    "input_id": input.id,
                })
                .to_string()
            ],
        )?;

        // The entry id is derived from the operation id. A committed start is
        // never replayed, while any failure here rolls the entire start back.
        let entry_id = format!("{run_id}:user");
        let entry_seq = next_seq(&tx, session_id)?;
        validate_user_prompt(&input.prompt)?;
        tx.execute(
            "INSERT INTO entries (id, session_id, parent_id, seq, kind, payload)
             VALUES (?1, ?2, ?3, ?4, 'message', ?5)",
            params![
                entry_id,
                session_id,
                source_leaf_id,
                entry_seq,
                input.prompt.to_string()
            ],
        )?;
        tx.execute(
            "UPDATE lanes SET leaf_id = ?3, open_operation_id = ?4
             WHERE session_id = ?1 AND name = ?2",
            params![session_id, MAIN_LANE, entry_id, run_id],
        )?;
        let changed = tx.execute(
            "UPDATE session_inputs
             SET state = 'started', operation_id = ?2, started_at = datetime('now')
             WHERE id = ?1 AND state = 'pending'",
            params![input.id, run_id],
        )?;
        if changed != 1 {
            return Err(StoreError::Corrupt(format!(
                "pending session input `{expected_input_id}` changed during start"
            )));
        }

        input.state = SessionInputState::Started;
        input.operation_id = Some(run_id);
        input.started_at = tx.query_row(
            "SELECT started_at FROM session_inputs WHERE id = ?1",
            params![input.id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some(input))
    }
}

fn validate_user_prompt(prompt: &Value) -> Result<Message, StoreError> {
    let message: Message = serde_json::from_value(prompt.clone())?;
    if message.role != bullpen_llm::Role::User {
        return Err(StoreError::Corrupt(
            "session input prompt must be a user message".into(),
        ));
    }
    Ok(message)
}

fn query_input_by_id(
    conn: &rusqlite::Connection,
    input_id: &str,
) -> Result<Option<SessionInput>, rusqlite::Error> {
    conn.query_row(
        "SELECT id, session_id, position, prompt, state, operation_id,
                enqueued_at, started_at
         FROM session_inputs WHERE id = ?1",
        params![input_id],
        row_to_input,
    )
    .optional()
}

fn row_to_input(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionInput> {
    let prompt = serde_json::from_str(&row.get::<_, String>(3)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let state = match row.get::<_, String>(4)?.as_str() {
        "pending" => SessionInputState::Pending,
        "started" => SessionInputState::Started,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                format!("invalid session input state `{other}`").into(),
            ));
        }
    };
    Ok(SessionInput {
        id: row.get(0)?,
        session_id: row.get(1)?,
        position: row.get(2)?,
        prompt,
        state,
        operation_id: row.get(5)?,
        enqueued_at: row.get(6)?,
        started_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use bullpen_llm::{Message, Role};
    use serde_json::Value;

    use super::*;
    use crate::recover;

    fn prompt(text: &str) -> Value {
        serde_json::to_value(Message::user_text(text)).unwrap()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn duplicate_provisioned_id_enqueues_once() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();

        let first = store
            .enqueue_input(&session.id, "input-1", &prompt("once"))
            .unwrap();
        let duplicate = store
            .enqueue_input(&session.id, "input-1", &prompt("once"))
            .unwrap();

        assert_eq!(first.position, 1);
        assert_eq!(duplicate.position, first.position);
        assert_eq!(store.list_inputs(&session.id).unwrap().len(), 1);
        assert!(matches!(
            store.enqueue_input(&session.id, "input-1", &prompt("different")),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn concurrent_enqueue_allocates_unique_fifo_positions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        let barrier = Arc::new(Barrier::new(9));
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let barrier = barrier.clone();
                let path = path.clone();
                let session_id = session.id.clone();
                std::thread::spawn(move || {
                    let mut store = Store::open(&path).unwrap();
                    barrier.wait();
                    store
                        .enqueue_input(
                            &session_id,
                            &format!("input-{index}"),
                            &prompt(&index.to_string()),
                        )
                        .unwrap()
                })
            })
            .collect();
        barrier.wait();

        let mut positions: Vec<i64> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().position)
            .collect();
        positions.sort_unstable();
        assert_eq!(positions, (1..=8).collect::<Vec<_>>());

        let ordered = store.list_inputs(&session.id).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|input| input.position)
                .collect::<Vec<_>>(),
            positions
        );
    }

    #[test]
    fn pending_input_survives_reopen_and_exposes_inspection_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let mut store = Store::open(&path).unwrap();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        store
            .enqueue_input(&session.id, "input-1", &prompt("persist"))
            .unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let input = store.inspect_input(&session.id, "input-1").unwrap();
        assert_eq!(input.state, SessionInputState::Pending);
        assert!(!input.enqueued_at.is_empty());
        assert_eq!(input.started_at, None);
    }

    #[test]
    fn only_the_oldest_pending_input_can_start() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        store
            .enqueue_input(&session.id, "first", &prompt("first"))
            .unwrap();
        store
            .enqueue_input(&session.id, "second", &prompt("second"))
            .unwrap();

        assert!(matches!(
            store.start_oldest_pending_input(&session.id, "second"),
            Err(StoreError::Corrupt(_))
        ));
        let started = store
            .start_oldest_pending_input(&session.id, "first")
            .unwrap()
            .unwrap();
        assert_eq!(started.id, "first");
        assert_eq!(
            store.inspect_input(&session.id, "second").unwrap().state,
            SessionInputState::Pending
        );
    }

    #[test]
    fn start_atomically_creates_one_operation_and_one_user_entry() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        store
            .enqueue_input(&session.id, "input-1", &prompt("go"))
            .unwrap();

        let started = store
            .start_oldest_pending_input(&session.id, "input-1")
            .unwrap()
            .unwrap();
        let run = store.open_run(&session.id).unwrap().unwrap();
        assert_eq!(started.operation_id.as_deref(), Some(run.run_id.as_str()));
        assert_eq!(run.records.len(), 1);
        assert_eq!(run.records[0].payload["input_id"], "input-1");
        let messages = store.path_messages(&session.id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].text(), "go");
        assert!(
            store
                .start_oldest_pending_input(&session.id, "input-1")
                .unwrap()
                .is_none()
        );
        assert_eq!(store.path_messages(&session.id).unwrap().len(), 1);
    }

    #[test]
    fn failed_start_rolls_back_every_write_and_leaves_input_pending() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        store
            .enqueue_input(&session.id, "input-1", &prompt("rollback"))
            .unwrap();
        // Fail the entry insert after the operation record and sequence update.
        // The enclosing start transaction must roll every write back.
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_inbox_user_entry
                 BEFORE INSERT ON entries
                 WHEN NEW.id LIKE '%:user'
                 BEGIN
                     SELECT RAISE(ABORT, 'simulated inbox start failure');
                 END;",
            )
            .unwrap();

        assert!(matches!(
            store.start_oldest_pending_input(&session.id, "input-1"),
            Err(StoreError::Db(_))
        ));
        let input = store.inspect_input(&session.id, "input-1").unwrap();
        assert_eq!(input.state, SessionInputState::Pending);
        assert_eq!(input.operation_id, None);
        assert_eq!(input.started_at, None);
        assert!(store.path_messages(&session.id).unwrap().is_empty());
        assert!(store.open_run(&session.id).unwrap().is_none());
        let record_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM records", [], |row| row.get(0))
            .unwrap();
        assert_eq!(record_count, 0);
    }

    #[test]
    fn enqueue_rejects_non_user_messages() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        let assistant = serde_json::to_value(Message {
            role: Role::Assistant,
            content: vec![],
        })
        .unwrap();

        assert!(matches!(
            store.enqueue_input(&session.id, "input-1", &assistant),
            Err(StoreError::Corrupt(_))
        ));
        assert!(store.list_inputs(&session.id).unwrap().is_empty());
    }

    #[test]
    fn started_input_does_not_prevent_execution_log_deletion() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        store
            .enqueue_input(&session.id, "input-1", &prompt("keep conversation"))
            .unwrap();
        store
            .start_oldest_pending_input(&session.id, "input-1")
            .unwrap()
            .unwrap();

        store
            .conn
            .execute(
                "DELETE FROM records WHERE session_id = ?1",
                params![session.id],
            )
            .unwrap();

        assert_eq!(store.path_messages(&session.id).unwrap().len(), 1);
        assert_eq!(
            store.inspect_input(&session.id, "input-1").unwrap().state,
            SessionInputState::Started
        );
    }

    #[test]
    fn recovery_after_started_input_never_duplicates_prompt() {
        let (_dir, mut store) = store();
        let session = store.create_session("/tmp", "anthropic", "m").unwrap();
        store
            .enqueue_input(&session.id, "input-1", &prompt("recover me"))
            .unwrap();
        let started = store
            .start_oldest_pending_input(&session.id, "input-1")
            .unwrap()
            .unwrap();

        let recovery = recover(&mut store, &session.id).unwrap().unwrap();
        assert_eq!(
            started.operation_id.as_deref(),
            Some(recovery.run_id.as_str())
        );
        assert!(recover(&mut store, &session.id).unwrap().is_none());
        let messages = store.path_messages(&session.id).unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.text() == "recover me")
                .count(),
            1
        );
        assert_eq!(messages.len(), 2);
        let input = store.inspect_input(&session.id, "input-1").unwrap();
        assert_eq!(input.state, SessionInputState::Started);
        assert!(
            store
                .start_oldest_pending_input(&session.id, "input-1")
                .unwrap()
                .is_none()
        );
    }
}

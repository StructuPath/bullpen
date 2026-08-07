//! Crash recovery.
//!
//! Reduction discovered a suspended run (an `operation_started` with no
//! `operation_finished`). Recovery makes the session usable again without
//! re-running anything: unfulfilled tool intents get synthetic
//! `interrupted` results at their provisioned entry ids, the transcript is
//! closed so it never ends on a dangling user message, and the operation is
//! finished as `aborted`. Every append is idempotent on its provisioned id,
//! so re-running recovery after a crash *during* recovery is safe.
//!
//! Conservative v1: replay-safe tools are not re-executed; their intents
//! synthesize as interrupted like everything else. The replay declaration
//! is already snapshotted in the intent record for when that lands.

use bullpen_llm::{ContentBlock, Message, Role};
use serde_json::Value;

use crate::{Store, StoreError};

#[derive(Debug)]
pub struct Recovery {
    pub run_id: String,
    /// Tool calls that got synthetic interrupted results.
    pub interrupted_tools: usize,
    /// Whether a closing assistant entry was appended.
    pub closed_with_note: bool,
}

pub fn recover(store: &mut Store, session_id: &str) -> Result<Option<Recovery>, StoreError> {
    let Some(open) = store.open_run(session_id)? else {
        return Ok(None);
    };

    // Group tool intents by their shared provisioned results-entry id.
    let mut batches: Vec<(String, Vec<Value>)> = Vec::new();
    for record in &open.records {
        if record.kind != "tool_started" {
            continue;
        }
        let Some(results_entry_id) = record
            .payload
            .get("results_entry_id")
            .and_then(Value::as_str)
        else {
            continue;
        };
        match batches.iter_mut().find(|(id, _)| id == results_entry_id) {
            Some((_, intents)) => intents.push(record.payload.clone()),
            None => batches.push((results_entry_id.to_string(), vec![record.payload.clone()])),
        }
    }

    let mut interrupted_tools = 0;
    for (results_entry_id, intents) in &batches {
        if store.entry_exists(results_entry_id)? {
            continue; // intent fulfilled — results landed before the crash
        }
        let results: Vec<ContentBlock> = intents
            .iter()
            .map(|intent| ContentBlock::ToolResult {
                tool_use_id: intent
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content: "interrupted: bullpen exited before this tool finished".into(),
                is_error: true,
            })
            .collect();
        interrupted_tools += results.len();
        let message = Message::tool_results(results);
        store.append_entry(
            session_id,
            results_entry_id,
            "message",
            &serde_json::to_value(&message)?,
        )?;
    }

    // Never leave the transcript ending on a user message (a prompt or
    // synthesized results) — the next run would produce two consecutive
    // user turns.
    let closed_with_note = match store.path_messages(session_id)?.last() {
        Some(last) if last.role == Role::User => {
            let closing = Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "[this run was interrupted and recovered; state above may be incomplete]"
                        .into(),
                }],
            };
            store.append_entry(
                session_id,
                &format!("{}:closing", open.run_id),
                "message",
                &serde_json::to_value(&closing)?,
            )?;
            true
        }
        _ => false,
    };

    store.finish_operation(session_id, &open.run_id, "aborted")?;
    Ok(Some(Recovery {
        run_id: open.run_id,
        interrupted_tools,
        closed_with_note,
    }))
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

    fn payload(m: &Message) -> Value {
        serde_json::to_value(m).unwrap()
    }

    /// Build the durable state of a run that crashed mid-tool-batch:
    /// prompt + assistant(tool_use) entries landed, intents recorded,
    /// results never appended.
    fn crashed_mid_batch(store: &mut Store) -> (String, String) {
        let s = store.create_session("/tmp", "anthropic", "m").unwrap();
        let run = store.start_operation(&s.id, &json!({})).unwrap();
        store
            .append_entry(&s.id, "e-user", "message", &payload(&Message::user_text("go")))
            .unwrap();
        let assistant = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "bash".into(),
                input: json!({"command": "make deploy"}),
            }],
        };
        store
            .append_entry(&s.id, "e-assistant", "message", &payload(&assistant))
            .unwrap();
        store
            .append_record(
                &s.id,
                "r-intent",
                &run,
                "tool_started",
                &json!({
                    "tool_use_id": "tu_1",
                    "name": "bash",
                    "input": {"command": "make deploy"},
                    "replay_safe": false,
                    "results_entry_id": "e-results",
                }),
            )
            .unwrap();
        (s.id, run)
    }

    #[test]
    fn recovers_crashed_batch_with_synthetic_results() {
        let (_dir, mut store) = store();
        let (session, run) = crashed_mid_batch(&mut store);

        let recovery = recover(&mut store, &session).unwrap().expect("suspended run");
        assert_eq!(recovery.run_id, run);
        assert_eq!(recovery.interrupted_tools, 1);
        assert!(recovery.closed_with_note);

        // Transcript is structurally valid: tool_use paired with an
        // interrupted result, then a closing assistant message.
        let messages = store.path_messages(&session).unwrap();
        assert_eq!(messages.len(), 4);
        let ContentBlock::ToolResult { tool_use_id, is_error, content } = &messages[2].content[0]
        else {
            panic!("expected synthetic tool result");
        };
        assert_eq!(tool_use_id, "tu_1");
        assert!(is_error);
        assert!(content.contains("interrupted"));
        assert_eq!(messages[3].role, Role::Assistant);

        // Reduction is idle again; a new run can start.
        assert!(store.open_run(&session).unwrap().is_none());
        store.start_operation(&session, &json!({})).unwrap();
    }

    #[test]
    fn recovery_is_idempotent() {
        let (_dir, mut store) = store();
        let (session, _) = crashed_mid_batch(&mut store);

        recover(&mut store, &session).unwrap().unwrap();
        // A crash during recovery means recovery runs again.
        assert!(recover(&mut store, &session).unwrap().is_none());
        assert_eq!(store.path_messages(&session).unwrap().len(), 4);
    }

    #[test]
    fn fulfilled_intents_are_left_alone() {
        let (_dir, mut store) = store();
        let (session, run) = crashed_mid_batch(&mut store);
        // The results actually landed before the crash.
        let results = Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: "deployed".into(),
            is_error: false,
        }]);
        store
            .append_entry(&session, "e-results", "message", &payload(&results))
            .unwrap();

        let recovery = recover(&mut store, &session).unwrap().unwrap();
        assert_eq!(recovery.run_id, run);
        assert_eq!(recovery.interrupted_tools, 0);
        // Real result survives untouched; closing note still appended
        // because the leaf ended on a user(tool-results) message.
        let messages = store.path_messages(&session).unwrap();
        let ContentBlock::ToolResult { content, is_error, .. } = &messages[2].content[0] else {
            panic!("expected result");
        };
        assert_eq!(content, "deployed");
        assert!(!is_error);
    }

    #[test]
    fn crash_before_any_entry_just_closes() {
        let (_dir, mut store) = store();
        let s = store.create_session("/tmp", "anthropic", "m").unwrap();
        store.start_operation(&s.id, &json!({})).unwrap();

        let recovery = recover(&mut store, &s.id).unwrap().unwrap();
        assert_eq!(recovery.interrupted_tools, 0);
        assert!(!recovery.closed_with_note);
        assert!(store.open_run(&s.id).unwrap().is_none());
        assert!(store.path_messages(&s.id).unwrap().is_empty());
    }

    #[test]
    fn idle_session_needs_no_recovery() {
        let (_dir, mut store) = store();
        let s = store.create_session("/tmp", "anthropic", "m").unwrap();
        assert!(recover(&mut store, &s.id).unwrap().is_none());
    }
}

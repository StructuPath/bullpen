//! The todo tool: a durable session plan.
//!
//! The plan lives in the store, not in the model's context window — it
//! survives crashes, resumes, and compaction like everything else in the
//! session. Two choices carry the design:
//!
//! - **Deterministic item ids** (`uuidv5(session, call_id, index)`) make a
//!   replayed `add` converge on the same rows instead of duplicating them,
//!   so the whole tool is replay-safe.
//! - **The store owns the one-active-item invariant**: marking an item
//!   `in_progress` returns any other active item to `pending`. The model
//!   cannot talk itself into three parallel "current" tasks.
//!
//! Every action returns the rendered plan, so the model always acts on the
//! current state rather than its memory of it.

use std::path::PathBuf;

use bullpen_llm::ToolSpec;
use bullpen_store::{Store, StoreError, Todo};
use bullpen_tools::{Tool, ToolCtx, ToolError};
use serde_json::{Value, json};

pub struct TodoTool {
    store_path: PathBuf,
    session_id: String,
}

impl TodoTool {
    pub fn new(store_path: PathBuf, session_id: impl Into<String>) -> Self {
        Self {
            store_path,
            session_id: session_id.into(),
        }
    }
}

/// Deterministic item identity: same session + same tool call + same index
/// → same todo. What makes a replayed `add` reattach instead of duplicate.
pub fn todo_id(session_id: &str, call_id: &str, index: usize) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("bullpen-todo:{session_id}:{call_id}:{index}").as_bytes(),
    )
    .to_string()
}

fn terr(e: StoreError) -> ToolError {
    match e {
        StoreError::NotFound(p) => ToolError::InvalidInput(format!("no todo matches `{p}`")),
        StoreError::Ambiguous(p) => ToolError::InvalidInput(format!(
            "todo id `{p}` is ambiguous — use a longer prefix from the list"
        )),
        other => ToolError::Failed(format!("store: {other}")),
    }
}

fn render(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "The plan is empty.".into();
    }
    let done = todos.iter().filter(|t| t.status == "completed").count();
    let mut out = format!("Plan ({done} of {} done):\n", todos.len());
    for t in todos {
        let mark = match t.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[>]",
            _ => "[ ]",
        };
        out.push_str(&format!("  {} {mark} {}\n", &t.id[..8], t.content));
    }
    out
}

#[async_trait::async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo".into(),
            description: "Track the plan for this session as a durable todo list. \
                          `add` appends items, `start` marks one in progress (any \
                          other active item returns to pending — one thing at a \
                          time), `done` completes one, `remove` drops one, `list` \
                          shows the plan. Items are addressed by the id prefix \
                          shown in the list. Every action returns the current \
                          plan. Use it for multi-step work; keep it current as \
                          steps finish."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "start", "done", "remove", "list"],
                        "description": "What to do to the plan"
                    },
                    "items": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "For `add`: the items to append, in order"
                    },
                    "id": {
                        "type": "string",
                        "description": "For `start`/`done`/`remove`: the item's id (any unique prefix)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        // Reading the plan can ride alongside anything; mutations keep the
        // default serial ordering so a batch applies in the order the model
        // issued it.
        input.get("action").and_then(Value::as_str) == Some("list")
    }

    fn replay_safe(&self) -> bool {
        // Adds converge via deterministic ids; status changes and removes
        // are idempotent by construction.
        true
    }

    async fn run(&self, _ctx: &ToolCtx, call_id: &str, input: Value) -> Result<String, ToolError> {
        let action = input.get("action").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidInput("missing required string field `action`".into())
        })?;
        let mut store = Store::open(&self.store_path).map_err(terr)?;

        match action {
            "add" => {
                let items: Vec<&str> = input
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if items.is_empty() {
                    return Err(ToolError::InvalidInput(
                        "`add` needs a non-empty `items` array of strings".into(),
                    ));
                }
                for (index, content) in items.iter().enumerate() {
                    let id = todo_id(&self.session_id, call_id, index);
                    store
                        .add_todo(&self.session_id, &id, content)
                        .map_err(terr)?;
                }
            }
            "start" | "done" => {
                let prefix = input.get("id").and_then(Value::as_str).ok_or_else(|| {
                    ToolError::InvalidInput(format!("`{action}` needs an `id` prefix"))
                })?;
                let status = if action == "start" {
                    "in_progress"
                } else {
                    "completed"
                };
                store
                    .set_todo_status(&self.session_id, prefix, status)
                    .map_err(terr)?;
            }
            "remove" => {
                let prefix = input.get("id").and_then(Value::as_str).ok_or_else(|| {
                    ToolError::InvalidInput("`remove` needs an `id` prefix".into())
                })?;
                store.remove_todo(&self.session_id, prefix).map_err(terr)?;
            }
            "list" => {}
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unknown action `{other}` (expected add, start, done, remove, or list)"
                )));
            }
        }

        Ok(render(&store.list_todos(&self.session_id).map_err(terr)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, TodoTool, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let session = Store::open(&path)
            .unwrap()
            .create_session("/tmp", "fake", "m")
            .unwrap();
        let tool = TodoTool::new(path, &session.id);
        (dir, tool, session.id)
    }

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn add_appends_and_renders_the_plan() {
        let (_dir, tool, _) = setup();
        let out = tool
            .run(
                &ctx(),
                "c1",
                json!({"action": "add", "items": ["read", "write"]}),
            )
            .await
            .unwrap();
        assert!(out.contains("Plan (0 of 2 done):"), "{out}");
        assert!(out.contains("[ ] read"), "{out}");
        assert!(out.contains("[ ] write"), "{out}");
    }

    #[tokio::test]
    async fn replayed_add_does_not_duplicate() {
        let (_dir, tool, _) = setup();
        let input = json!({"action": "add", "items": ["once"]});
        tool.run(&ctx(), "c1", input.clone()).await.unwrap();
        let out = tool.run(&ctx(), "c1", input).await.unwrap();
        assert!(out.contains("of 1 done"), "{out}");
    }

    #[tokio::test]
    async fn start_and_done_by_prefix_keep_one_item_active() {
        let (_dir, tool, session) = setup();
        tool.run(&ctx(), "c1", json!({"action": "add", "items": ["a", "b"]}))
            .await
            .unwrap();
        let first = &todo_id(&session, "c1", 0)[..8];
        let second = &todo_id(&session, "c1", 1)[..8];

        let out = tool
            .run(&ctx(), "c2", json!({"action": "start", "id": first}))
            .await
            .unwrap();
        assert!(out.contains(&format!("{first} [>] a")), "{out}");

        // Starting the second returns the first to pending.
        let out = tool
            .run(&ctx(), "c3", json!({"action": "start", "id": second}))
            .await
            .unwrap();
        assert!(out.contains(&format!("{first} [ ] a")), "{out}");
        assert!(out.contains(&format!("{second} [>] b")), "{out}");

        let out = tool
            .run(&ctx(), "c4", json!({"action": "done", "id": second}))
            .await
            .unwrap();
        assert!(out.contains("Plan (1 of 2 done):"), "{out}");
        assert!(out.contains(&format!("{second} [x] b")), "{out}");
    }

    #[tokio::test]
    async fn remove_and_empty_render() {
        let (_dir, tool, session) = setup();
        tool.run(&ctx(), "c1", json!({"action": "add", "items": ["only"]}))
            .await
            .unwrap();
        let id = &todo_id(&session, "c1", 0)[..8];
        let out = tool
            .run(&ctx(), "c2", json!({"action": "remove", "id": id}))
            .await
            .unwrap();
        assert_eq!(out, "The plan is empty.");
    }

    #[tokio::test]
    async fn bad_inputs_are_invalid_not_failed() {
        let (_dir, tool, _) = setup();
        for input in [
            json!({}),
            json!({"action": "add"}),
            json!({"action": "add", "items": []}),
            json!({"action": "start"}),
            json!({"action": "done", "id": "zzz"}),
            json!({"action": "yolo"}),
        ] {
            let err = tool.run(&ctx(), "c", input).await.unwrap_err();
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
        }
    }

    #[tokio::test]
    async fn ambiguous_prefix_asks_for_more() {
        let (_dir, tool, session) = setup();
        // Two items from one call share the session/call prefix only if the
        // uuids collide on the first character — force ambiguity with the
        // empty prefix instead, which matches everything.
        tool.run(&ctx(), "c1", json!({"action": "add", "items": ["a", "b"]}))
            .await
            .unwrap();
        let err = tool
            .run(&ctx(), "c2", json!({"action": "done", "id": ""}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
        drop(session);
    }

    #[test]
    fn only_list_is_parallel_safe_and_replay_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        let tool = TodoTool::new(dir.path().join("t.db"), "s");
        assert!(tool.parallel_safe(&json!({"action": "list"})));
        assert!(!tool.parallel_safe(&json!({"action": "add", "items": ["x"]})));
        assert!(!tool.parallel_safe(&json!({"action": "done", "id": "a"})));
        assert!(tool.replay_safe());
    }

    #[test]
    fn todo_ids_are_deterministic() {
        let a = todo_id("s1", "c1", 0);
        assert_eq!(a, todo_id("s1", "c1", 0));
        assert_ne!(a, todo_id("s1", "c1", 1));
        assert_ne!(a, todo_id("s1", "c2", 0));
        assert_ne!(a, todo_id("s2", "c1", 0));
    }
}

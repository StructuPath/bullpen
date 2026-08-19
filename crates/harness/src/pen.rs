//! The pen: durable subagents.
//!
//! A child is an ordinary session in the same store, linked to its parent
//! and named **deterministically** from the invocation:
//! `child_id = uuidv5(parent_session_id, tool_call_id)`. That single choice
//! does most of the work:
//!
//! - a replayed spawn (same tool call) *reattaches* to the same child
//!   instead of spawning a twin — a completed child returns its recorded
//!   answer without touching the provider;
//! - a child whose process died mid-run is recovered by the same machinery
//!   as any session, then *continued* rather than restarted;
//! - children survive process exit and are inspectable with
//!   `bullpen sessions` / resumable with `bullpen run -r` like any session.
//!
//! Budgets are durable too: the child count is a database count, not an
//! in-process counter, so a crash-restart loop cannot reset it.
//!
//! Work children can be **isolated** (`worktree: true`): each runs in its
//! own git worktree on a `bullpen/<child-id>` branch, exactly like
//! `bullpen run --bg --worktree`, which is what makes isolated work
//! children safe to run in parallel. And any child can be **dispatched to
//! the background** (`background: true`): the spawn returns immediately
//! and the child runs in this process, coordinated — like everything else
//! — through the store, where the `job` tool finds it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bullpen_agent::{Agent, AgentConfig};
use bullpen_llm::{Provider, Role, ToolSpec};
use bullpen_store::{Session, SessionWorker, Store};
use bullpen_tools::{AstGrep, Glob, Grep, ReadFile, Registry, Tool, ToolCtx, ToolError};
use serde_json::{Value, json};

use crate::{StoreJournal, prepare_session, worktree};

/// Cancellation handles for background children running in this process,
/// shared with the `job` tool. A child's entry exists exactly while its
/// task runs here; children run by other processes are never in it.
pub(crate) type Cancels = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

#[derive(Clone)]
pub struct PenConfig {
    pub store_path: PathBuf,
    pub workspace: PathBuf,
    /// Recorded on child session rows (matches the parent's provider).
    pub provider_name: String,
    pub model: String,
    /// Base system prompt; the pen appends the relief-agent role text.
    pub system: String,
    pub max_children: u64,
    pub child_timeout: Duration,
    pub child_max_turns: u32,
    /// Write-confinement applied to work children (inspect children are
    /// read-only regardless).
    pub sandbox: Option<Arc<bullpen_sandbox::Sandbox>>,
}

impl PenConfig {
    pub fn new(
        store_path: PathBuf,
        workspace: PathBuf,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        system: impl Into<String>,
    ) -> Self {
        Self {
            store_path,
            workspace,
            provider_name: provider_name.into(),
            model: model.into(),
            system: system.into(),
            max_children: 20,
            child_timeout: Duration::from_secs(900),
            child_max_turns: 200,
            sandbox: None,
        }
    }

    pub fn with_sandbox(mut self, sandbox: Arc<bullpen_sandbox::Sandbox>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }
}

/// The `agent` tool: delegate a bounded task to a durable child agent.
pub struct PenTool {
    provider: Arc<dyn Provider>,
    parent_session: String,
    config: PenConfig,
    cancels: Cancels,
}

impl PenTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        parent_session: impl Into<String>,
        config: PenConfig,
    ) -> Self {
        Self {
            provider,
            parent_session: parent_session.into(),
            config,
            cancels: Cancels::default(),
        }
    }

    /// The `job` tool over this pen's children, sharing its cancellation
    /// registry so background children dispatched here can be cancelled.
    pub fn job_tool(&self) -> crate::job::JobTool {
        crate::job::JobTool::new(
            self.config.store_path.clone(),
            &self.parent_session,
            self.cancels.clone(),
        )
    }

    /// Where a child runs. First worktree dispatch records then creates the
    /// tree; after that the recorded state decides, exactly as it does for
    /// a CLI resume — a replayed spawn reattaches to the same worktree.
    fn place_child(
        &self,
        store: &Store,
        child: &Session,
        use_worktree: bool,
    ) -> Result<PathBuf, ToolError> {
        if use_worktree && child.worktree_path.is_none() {
            let root = worktree::repo_root(&self.config.workspace)
                .map_err(|e| ToolError::Failed(e.to_string()))?;
            let path = worktree::worktree_path_for_store(&self.config.store_path, &child.id);
            let branch = worktree::branch_for(&child.id);
            // Recorded before it is created (see the CLI dispatch path): a
            // failed creation leaves a row naming a missing directory, which
            // resume refuses, never a row naming none.
            store
                .set_worktree(&child.id, &path.display().to_string(), &branch)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
            worktree::create(&root, &path, &branch)
                .map_err(|e| ToolError::Failed(e.to_string()))?;
            return Ok(path);
        }
        match worktree::locate(
            child.worktree_path.as_deref(),
            child.worktree_branch.as_deref(),
            Path::new(&child.cwd),
        ) {
            worktree::Location::Shared => Ok(self.config.workspace.clone()),
            worktree::Location::Use(path) => Ok(path),
            worktree::Location::Recreate { path, branch } => {
                let root = worktree::repo_root(Path::new(&child.cwd))
                    .map_err(|e| ToolError::Failed(e.to_string()))?;
                worktree::recreate(&root, &path, &branch)
                    .map_err(|e| ToolError::Failed(e.to_string()))?;
                Ok(path)
            }
            worktree::Location::Fail { path, branch } => Err(ToolError::Failed(format!(
                "child worktree {} and branch {branch} are both gone; restore \
                 them or use a new task",
                path.display()
            ))),
            worktree::Location::Occupied { path, .. } => Err(ToolError::Failed(format!(
                "something that is not the child's worktree occupies {}; move \
                 it aside or use a new task",
                path.display()
            ))),
        }
    }
}

/// Deterministic child identity: same parent + same tool call → same child.
pub fn child_session_id(parent_session: &str, tool_call_id: &str) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("bullpen:{parent_session}:{tool_call_id}").as_bytes(),
    )
    .to_string()
}

fn registry_for_mode(mode: &str) -> Result<Registry, ToolError> {
    match mode {
        // Read-only capability set; no bash, no writes, and never a nested pen.
        "inspect" => {
            let mut r = Registry::new();
            r.register(Arc::new(ReadFile));
            r.register(Arc::new(Grep));
            r.register(Arc::new(Glob));
            r.register(Arc::new(AstGrep::new()));
            Ok(r)
        }
        // Full workspace tools — still no nested pen.
        "work" => Ok(Registry::standard()),
        other => Err(ToolError::InvalidInput(format!(
            "unknown mode `{other}` (expected \"inspect\" or \"work\")"
        ))),
    }
}

#[async_trait::async_trait]
impl Tool for PenTool {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent".into(),
            description: "Delegate a bounded task to a child agent with its own \
                          context window. Mode `inspect` (default) gives it \
                          read-only tools (read_file, grep, glob); mode `work` \
                          adds bash and file editing. By default the child runs \
                          to completion and returns its final report. \
                          `worktree: true` (work mode only) runs the child in \
                          its own git worktree on its own branch, so isolated \
                          work children can run in parallel. `background: true` \
                          dispatches the child and returns immediately — use \
                          the job tool to list, wait on, or cancel it. Use for \
                          research across many files or self-contained subtasks."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Complete, self-contained task description — the child cannot see this conversation"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["inspect", "work"],
                        "description": "Capability set for the child (default: inspect)"
                    },
                    "worktree": {
                        "type": "boolean",
                        "description": "Run a work child in its own git worktree (default: false)"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Dispatch and return immediately instead of waiting (default: false)"
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        // Inspect children are read-only in the workspace and each writes
        // only to its own child session (WAL handles the store contention),
        // so they can run alongside each other. So can isolated work
        // children — each mutates only its own worktree — and background
        // dispatches, which only spawn and return. A work child in the
        // shared checkout stays serial.
        let mode = input
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("inspect");
        let flag = |key| input.get(key).and_then(Value::as_bool).unwrap_or(false);
        mode == "inspect" || flag("worktree") || flag("background")
    }

    async fn run(&self, _ctx: &ToolCtx, call_id: &str, input: Value) -> Result<String, ToolError> {
        let prompt = input.get("prompt").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidInput("missing required string field `prompt`".into())
        })?;
        let mode = input
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("inspect");
        let flag = |key| input.get(key).and_then(Value::as_bool).unwrap_or(false);
        let (use_worktree, background) = (flag("worktree"), flag("background"));
        if use_worktree && mode != "work" {
            return Err(ToolError::InvalidInput(
                "worktree isolation is for `work` children; inspect children \
                 read the shared checkout"
                    .into(),
            ));
        }
        let registry = registry_for_mode(mode)?;

        let child_id = child_session_id(&self.parent_session, call_id);
        let short = &child_id[..8];

        // A child this process already has in flight: report, don't respawn.
        if self.cancels.lock().unwrap().contains_key(&child_id) {
            return Ok(format!(
                "child {short} is already running in the background; use the \
                 job tool to wait on or cancel it"
            ));
        }

        let store = Store::open(&self.config.store_path)
            .map_err(|e| ToolError::Failed(format!("store: {e}")))?;

        // Budget applies to *new* children only; reattaching to an existing
        // child (a replay) is always allowed.
        let exists = store.get_session(&child_id).is_ok();
        if !exists {
            let children = store
                .count_children(&self.parent_session)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
            if children >= self.config.max_children {
                return Err(ToolError::Failed(format!(
                    "child budget exhausted ({} of {} used for this session)",
                    children, self.config.max_children
                )));
            }
        }

        let child = store
            .create_child_session(
                &child_id,
                &self.parent_session,
                &self.config.workspace.display().to_string(),
                &self.config.provider_name,
                &self.config.model,
            )
            .map_err(|e| ToolError::Failed(format!("store: {e}")))?;

        // Reattach: the child already finished — return its recorded answer
        // without another provider call.
        if exists
            && store
                .last_run_outcome(&child_id)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?
                .as_deref()
                == Some("completed")
        {
            let messages = store
                .path_messages(&child_id)
                .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
            let answer = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.text())
                .unwrap_or_default();
            return Ok(format!(
                "{answer}\n\n[child {short} · reattached to completed run]"
            ));
        }

        let child_cwd = self.place_child(&store, &child, use_worktree)?;
        drop(store);

        let spec = ChildSpec {
            provider: self.provider.clone(),
            config: self.config.clone(),
            child_id: child_id.clone(),
            child_cwd,
            registry,
            mode: mode.to_string(),
            prompt: prompt.to_string(),
            usage: child.usage,
        };

        if background {
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            self.cancels
                .lock()
                .unwrap()
                .insert(child_id.clone(), cancel_tx);
            let cancels = self.cancels.clone();
            let isolated = spec.child_cwd != self.config.workspace;
            let short = short.to_string();
            tokio::spawn(async move {
                // The outcome text is discarded — the durable child session
                // is the record, and the job tool reads it from there.
                let _ = run_child(spec, Some(cancel_rx)).await;
                cancels.lock().unwrap().remove(&child_id);
            });
            return Ok(format!(
                "dispatched child {short} in the background (mode {mode}{}); \
                 use the job tool to list, wait on, or cancel it",
                if isolated {
                    " · isolated worktree"
                } else {
                    ""
                }
            ));
        }

        run_child(spec, None).await
    }
}

/// Everything one child run needs, owned, so a background dispatch can move
/// it into its task.
struct ChildSpec {
    provider: Arc<dyn Provider>,
    config: PenConfig,
    child_id: String,
    child_cwd: PathBuf,
    registry: Registry,
    mode: String,
    prompt: String,
    usage: bullpen_llm::Usage,
}

/// Run one child to its outcome: acquire exclusive ownership, recover and
/// continue anything a previous process left open, run the agent, record
/// the terminal state. `cancel` (background children only) resolves the run
/// early: the child finishes as failed and stays resumable.
async fn run_child(
    spec: ChildSpec,
    cancel: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<String, ToolError> {
    let ChildSpec {
        provider,
        config,
        child_id,
        child_cwd,
        registry,
        mode,
        prompt,
        usage,
    } = spec;
    let short = child_id[..8].to_string();

    // Fresh or interrupted child: acquire the same exclusive ownership used
    // by top-level CLI runs before recovery or provider activity. This
    // prevents a manual `bullpen run -r <child>` from racing the pen.
    let mut store =
        Store::open(&config.store_path).map_err(|e| ToolError::Failed(format!("store: {e}")))?;
    let mut session_worker = SessionWorker::acquire(&config.store_path, &child_id)
        .map_err(|e| ToolError::Failed(format!("child {short}: {e}")))?;
    session_worker
        .start(&mut store)
        .map_err(|e| ToolError::Failed(format!("child {short}: {e}")))?;
    let (transcript, recovery) = prepare_session(&mut store, &child_id)
        .map_err(|e| ToolError::Failed(format!("store: {e}")))?;
    let task = if transcript.is_empty() {
        prompt
    } else {
        "The previous attempt above was interrupted. Continue and complete the \
         original task, then give your final report."
            .to_string()
    };
    drop(store);

    let system = format!(
        "{}\n\nYou are a bullpen relief agent handling one delegated task. \
         Work it to completion and end with a final report — your last \
         message goes back to the coordinating agent, which cannot see \
         your intermediate steps.{}",
        config.system,
        if mode == "inspect" {
            " You have read-only tools."
        } else {
            ""
        }
    );

    // An isolated child gets a sandbox rebased onto its worktree (same
    // network policy), widened to the git dirs a linked worktree commits
    // through; a shared-checkout child inherits the parent's as-is.
    let sandbox = config.sandbox.as_ref().map(|sb| {
        if child_cwd == config.workspace {
            sb.clone()
        } else {
            let base = if sb.capabilities().allow_network {
                bullpen_sandbox::Sandbox::workspace(&child_cwd)
            } else {
                bullpen_sandbox::Sandbox::strict(&child_cwd)
            };
            Arc::new(base.allowing_writes(worktree::git_write_roots(&child_cwd)))
        }
    });

    let journal = StoreJournal::new(
        Store::open(&config.store_path).map_err(|e| ToolError::Failed(format!("store: {e}")))?,
        &child_id,
    );
    let mut child_ctx = ToolCtx::new(child_cwd);
    if let Some(sandbox) = sandbox {
        child_ctx = child_ctx.with_sandbox(sandbox);
    }
    let mut agent = Agent::new(
        provider,
        registry,
        child_ctx,
        AgentConfig {
            model: config.model.clone(),
            system,
            max_turns: config.child_max_turns,
            ..Default::default()
        },
    )
    .with_transcript(transcript, usage)
    .with_journal(Box::new(journal));

    enum Ran {
        Done(Result<String, bullpen_agent::AgentError>),
        TimedOut,
        Cancelled,
    }
    let work = tokio::time::timeout(config.child_timeout, agent.send(&task));
    let ran = match cancel {
        Some(cancel) => tokio::select! {
            result = work => match result {
                Ok(r) => Ran::Done(r),
                Err(_) => Ran::TimedOut,
            },
            _ = cancel => Ran::Cancelled,
        },
        None => match work.await {
            Ok(r) => Ran::Done(r),
            Err(_) => Ran::TimedOut,
        },
    };
    let usage = agent.usage();
    let outcome = match ran {
        Ran::Done(Ok(answer)) => Ok(format!(
            "{answer}\n\n[child {short} · mode {mode} · {} in / {} out tokens{}]",
            usage.input_tokens,
            usage.output_tokens,
            if recovery.is_some() {
                " · recovered"
            } else {
                ""
            },
        )),
        Ran::Done(Err(e)) => Err(ToolError::Failed(format!(
            "child {short} failed: {e} (session is saved and resumable)"
        ))),
        Ran::TimedOut => Err(ToolError::Failed(format!(
            "child {short} timed out after {}s; its session is saved and \
             recoverable — calling agent again with the same task will continue it",
            config.child_timeout.as_secs()
        ))),
        Ran::Cancelled => Err(ToolError::Failed(format!(
            "child {short} was cancelled; its session is saved and resumable"
        ))),
    };
    let status = if outcome.is_ok() {
        "completed"
    } else {
        "failed"
    };
    session_worker
        .finish(status)
        .map_err(|e| ToolError::Failed(format!("child {short}: {e}")))?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeProvider, response, text_response};
    use bullpen_llm::{ContentBlock, StopReason};

    fn config(dir: &tempfile::TempDir) -> PenConfig {
        PenConfig::new(
            dir.path().join("t.db"),
            std::env::temp_dir(),
            "fake",
            "test-model",
            "base system",
        )
    }

    fn parent(dir: &tempfile::TempDir) -> String {
        Store::open(&dir.path().join("t.db"))
            .unwrap()
            .create_session("/tmp", "fake", "test-model")
            .unwrap()
            .id
    }

    fn tool_ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[test]
    fn inspect_children_are_parallel_safe_work_children_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let pen = PenTool::new(FakeProvider::new(vec![]), "p", config(&dir));
        assert!(pen.parallel_safe(&json!({"mode": "inspect", "prompt": "x"})));
        assert!(pen.parallel_safe(&json!({"prompt": "x"}))); // default mode
        assert!(!pen.parallel_safe(&json!({"mode": "work", "prompt": "x"})));
    }

    #[test]
    fn child_ids_are_deterministic() {
        let a = child_session_id("parent-1", "call-1");
        assert_eq!(a, child_session_id("parent-1", "call-1"));
        assert_ne!(a, child_session_id("parent-1", "call-2"));
        assert_ne!(a, child_session_id("parent-2", "call-1"));
    }

    #[tokio::test]
    async fn child_runs_and_is_durably_linked() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("child answer")]),
            &parent,
            config(&dir),
        );

        let out = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "look into it"}))
            .await
            .unwrap();
        assert!(out.contains("child answer"), "{out}");

        let store = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(store.count_children(&parent).unwrap(), 1);
        let child = store
            .get_session(&child_session_id(&parent, "call_1"))
            .unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.as_str()));
        assert_eq!(child.status, "completed");
        assert_eq!(child.pid, None);
        assert_eq!(
            store.last_run_outcome(&child.id).unwrap().as_deref(),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn a_cli_owned_child_cannot_also_run_in_the_pen() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let child_id = child_session_id(&parent, "call_1");
        let owner = SessionWorker::acquire(&config(&dir).store_path, &child_id).unwrap();
        let pen = PenTool::new(FakeProvider::new(vec![]), &parent, config(&dir));

        let error = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "task"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already has a running worker"));

        drop(owner);
        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("after release")]),
            &parent,
            config(&dir),
        );
        let output = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "task"}))
            .await
            .unwrap();
        assert!(output.contains("after release"));
    }

    #[tokio::test]
    async fn replay_reattaches_without_provider_calls() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("first answer")]),
            &parent,
            config(&dir),
        );
        pen.run(&tool_ctx(), "call_1", json!({"prompt": "task"}))
            .await
            .unwrap();

        // Empty script: any provider call would fail the run. Reattach must
        // return the recorded answer instead.
        let pen = PenTool::new(FakeProvider::new(vec![]), &parent, config(&dir));
        let out = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "task"}))
            .await
            .unwrap();
        assert!(out.contains("first answer"), "{out}");
        assert!(out.contains("reattached"), "{out}");
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        assert_eq!(store.count_children(&parent).unwrap(), 1);
    }

    #[tokio::test]
    async fn child_budget_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let mut cfg = config(&dir);
        cfg.max_children = 1;

        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("one")]),
            &parent,
            cfg.clone(),
        );
        pen.run(&tool_ctx(), "call_1", json!({"prompt": "a"}))
            .await
            .unwrap();

        // A NEW pen instance (fresh process) still sees the budget spent.
        let pen = PenTool::new(FakeProvider::new(vec![text_response("two")]), &parent, cfg);
        let err = pen
            .run(&tool_ctx(), "call_2", json!({"prompt": "b"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("budget exhausted"), "{err}");
    }

    #[tokio::test]
    async fn interrupted_child_is_recovered_and_continued() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let child_id = child_session_id(&parent, "call_1");

        // Fabricate the durable state of a child that crashed mid-run.
        {
            let mut store = Store::open(&dir.path().join("t.db")).unwrap();
            store
                .create_child_session(&child_id, &parent, "/tmp", "fake", "test-model")
                .unwrap();
            store.start_operation(&child_id, &json!({})).unwrap();
            store
                .append_entry(
                    &child_id,
                    "e-user",
                    "message",
                    &serde_json::to_value(bullpen_llm::Message::user_text("original task"))
                        .unwrap(),
                )
                .unwrap();
        }

        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("finished it")]),
            &parent,
            config(&dir),
        );
        let out = pen
            .run(&tool_ctx(), "call_1", json!({"prompt": "original task"}))
            .await
            .unwrap();
        assert!(out.contains("finished it"), "{out}");
        assert!(out.contains("recovered"), "{out}");

        // The child's transcript shows the whole story: original attempt,
        // recovery closure, continuation.
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let messages = store.path_messages(&child_id).unwrap();
        assert!(messages.iter().any(|m| m.text().contains("interrupted")));
        assert_eq!(
            store.last_run_outcome(&child_id).unwrap().as_deref(),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn inspect_mode_has_no_bash() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![
                response(
                    vec![ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "rm -rf /"}),
                    }],
                    StopReason::ToolUse,
                ),
                text_response("could not run that"),
            ]),
            &parent,
            config(&dir),
        );

        let out = pen
            .run(
                &tool_ctx(),
                "call_1",
                json!({"prompt": "task", "mode": "inspect"}),
            )
            .await
            .unwrap();
        assert!(out.contains("could not run that"), "{out}");

        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let child_id = child_session_id(&parent, "call_1");
        let messages = store.path_messages(&child_id).unwrap();
        // The bash attempt got an unknown-tool error result, not execution.
        assert!(messages.iter().any(|m| m.content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("unknown tool"))
        )),);
    }

    #[test]
    fn isolated_and_background_children_are_parallel_safe() {
        let dir = tempfile::tempdir().unwrap();
        let pen = PenTool::new(FakeProvider::new(vec![]), "p", config(&dir));
        assert!(pen.parallel_safe(&json!({"mode": "work", "worktree": true, "prompt": "x"})));
        assert!(pen.parallel_safe(&json!({"mode": "work", "background": true, "prompt": "x"})));
        assert!(!pen.parallel_safe(&json!({"mode": "work", "prompt": "x"})));
    }

    /// A repository at `root` with one committed file (see worktree tests).
    fn init_repo(root: &std::path::Path) {
        std::fs::create_dir_all(root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init"]);
        std::fs::write(root.join("f.txt"), "x").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-m", "init"]);
    }

    #[tokio::test]
    async fn worktree_child_works_in_its_own_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        let mut cfg = config(&dir);
        cfg.workspace = repo.clone();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![
                response(
                    vec![ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "echo done > marker.txt"}),
                    }],
                    StopReason::ToolUse,
                ),
                text_response("isolated work done"),
            ]),
            &parent,
            cfg,
        );

        let out = pen
            .run(
                &tool_ctx(),
                "call_1",
                json!({"prompt": "work", "mode": "work", "worktree": true}),
            )
            .await
            .unwrap();
        assert!(out.contains("isolated work done"), "{out}");

        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let child = store
            .get_session(&child_session_id(&parent, "call_1"))
            .unwrap();
        assert_eq!(
            child.worktree_branch.as_deref(),
            Some(format!("bullpen/{}", child.id).as_str())
        );
        let wt = PathBuf::from(child.worktree_path.unwrap());
        // The marker landed in the worktree beside the store, not in the
        // shared checkout.
        assert!(wt.join("marker.txt").is_file());
        assert!(!repo.join("marker.txt").exists());
        assert!(wt.starts_with(dir.path()));
    }

    #[tokio::test]
    async fn worktree_needs_work_mode_and_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(FakeProvider::new(vec![]), &parent, config(&dir));

        let err = pen
            .run(&tool_ctx(), "c1", json!({"prompt": "x", "worktree": true}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");

        // Work mode, but the workspace (a temp dir) is not a repository —
        // and isolation never degrades to the shared checkout.
        let err = pen
            .run(
                &tool_ctx(),
                "c2",
                json!({"prompt": "x", "mode": "work", "worktree": true}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not inside a git repository"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn background_dispatch_returns_immediately_and_job_waits() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            FakeProvider::new(vec![text_response("bg answer")]),
            &parent,
            config(&dir),
        );
        let job = pen.job_tool();

        let out = pen
            .run(
                &tool_ctx(),
                "call_1",
                json!({"prompt": "task", "background": true}),
            )
            .await
            .unwrap();
        assert!(out.contains("dispatched child"), "{out}");

        let short = child_session_id(&parent, "call_1")[..8].to_string();
        let waited = job
            .run(
                &tool_ctx(),
                "j1",
                json!({"action": "wait", "id": short, "timeout_seconds": 30}),
            )
            .await
            .unwrap();
        assert!(waited.contains("bg answer"), "{waited}");
        assert!(waited.contains("completed"), "{waited}");
    }

    #[tokio::test]
    async fn cancelled_background_child_finishes_failed_and_stays_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(
            Arc::new(crate::testutil::HangingProvider),
            &parent,
            config(&dir),
        );
        let job = pen.job_tool();

        pen.run(
            &tool_ctx(),
            "call_1",
            json!({"prompt": "never ends", "background": true}),
        )
        .await
        .unwrap();
        let short = child_session_id(&parent, "call_1")[..8].to_string();

        // The dispatch is visible on the coordination plane...
        let listed = job
            .run(&tool_ctx(), "j1", json!({"action": "list"}))
            .await
            .unwrap();
        assert!(listed.contains(&short), "{listed}");

        // ...and a second spawn of the same call reports, not respawns.
        let again = pen
            .run(
                &tool_ctx(),
                "call_1",
                json!({"prompt": "never ends", "background": true}),
            )
            .await
            .unwrap();
        assert!(again.contains("already running"), "{again}");

        let out = job
            .run(&tool_ctx(), "j2", json!({"action": "cancel", "id": short}))
            .await
            .unwrap();
        assert!(out.contains("cancel signalled"), "{out}");

        // wait observes the terminal state the cancelled child recorded.
        let err = job
            .run(
                &tool_ctx(),
                "j3",
                json!({"action": "wait", "id": short, "timeout_seconds": 30}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed"), "{err}");
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let child = store
            .get_session(&child_session_id(&parent, "call_1"))
            .unwrap();
        assert_eq!(child.status, "failed");
    }

    #[tokio::test]
    async fn bad_mode_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let parent = parent(&dir);
        let pen = PenTool::new(FakeProvider::new(vec![]), &parent, config(&dir));
        let err = pen
            .run(&tool_ctx(), "c", json!({"prompt": "x", "mode": "yolo"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}

//! Executable capabilities.
//!
//! Tools implement one trait and are composed into a [`Registry`] by the
//! application, never by the agent loop. The runtime — not the model — owns
//! the parallel-safety decision via [`Tool::parallel_safe`].

mod ask;
mod ast;
mod bash;
mod fs;
mod github;
mod search;

pub use ask::{Ask, Asker};
pub use ast::{AstEdit, AstGrep};
pub use bash::Bash;
pub use fs::{EditFile, ReadFile, WriteFile};
pub use github::GitHub;
pub use search::{Glob, Grep};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bullpen_llm::ToolSpec;
use serde_json::Value;

/// Ambient context every tool run receives.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// Workspace root. Relative paths in tool inputs resolve against this.
    pub workspace: PathBuf,
    /// Optional write-confinement boundary. When set, `bash` runs under it
    /// and the file-editing tools refuse writes it disallows.
    pub sandbox: Option<Arc<bullpen_sandbox::Sandbox>>,
}

impl ToolCtx {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            sandbox: None,
        }
    }

    pub fn with_sandbox(mut self, sandbox: Arc<bullpen_sandbox::Sandbox>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Reject a write the sandbox disallows. No sandbox → all writes allowed.
    pub(crate) fn check_write(&self, path: &Path) -> Result<(), ToolError> {
        match &self.sandbox {
            Some(sb) if !sb.allows_write(path) => Err(ToolError::Failed(format!(
                "sandbox: writing outside the workspace is not permitted: {}",
                path.display()
            ))),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{0}")]
    Failed(String),
    #[error("timed out after {0} seconds")]
    Timeout(u64),
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn spec(&self) -> ToolSpec;
    /// Whether this concrete invocation is safe to run concurrently with
    /// other parallel-safe calls. The runtime owns this decision — model-
    /// supplied arguments cannot declare themselves parallel-safe. Mutating
    /// tools and shell execution stay serial. Defaults closed.
    fn parallel_safe(&self, _input: &Value) -> bool {
        false
    }
    /// Whether re-executing this tool with the same input after a crash is
    /// safe (no side effects). Snapshotted into durability intent records.
    /// Defaults closed.
    fn replay_safe(&self) -> bool {
        false
    }
    /// `call_id` is the provider-assigned tool_use id for this invocation —
    /// stable across a replay, which is what lets effectful tools (like the
    /// pen) derive deterministic identities.
    async fn run(&self, ctx: &ToolCtx, call_id: &str, input: Value) -> Result<String, ToolError>;
}

#[derive(Default, Clone)]
pub struct Registry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Tool specs in stable (sorted) order for provider requests.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// The standard workspace registry: shell, file I/O, and search — both
    /// textual and structural.
    pub fn standard() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(Bash));
        r.register(Arc::new(ReadFile));
        r.register(Arc::new(WriteFile));
        r.register(Arc::new(EditFile));
        r.register(Arc::new(Grep));
        r.register(Arc::new(Glob));
        r.register(Arc::new(AstGrep::new()));
        r.register(Arc::new(AstEdit::new()));
        r
    }
}

/// Resolve a possibly-relative path against the workspace root.
pub(crate) fn resolve_path(ctx: &ToolCtx, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.workspace.join(p)
    }
}

/// Cap text for the transcript, keeping the head and tail around a marker.
pub(crate) fn truncate_middle(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let keep = max_bytes / 2;
    let mut head_end = keep;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - keep;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n\n[... output truncated: {} of {} bytes elided ...]\n\n{}",
        &s[..head_end],
        tail_start - head_end,
        s.len(),
        &s[tail_start..]
    )
}

pub(crate) fn required_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput(format!("missing required string field `{key}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_specs_sorted_and_complete() {
        let r = Registry::standard();
        let names: Vec<String> = r.specs().iter().map(|s| s.name.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert_eq!(
            names,
            vec![
                "ast_edit",
                "ast_grep",
                "bash",
                "edit_file",
                "glob",
                "grep",
                "read_file",
                "write_file"
            ]
        );
    }

    #[test]
    fn truncation_keeps_head_and_tail() {
        let s = "a".repeat(100) + &"z".repeat(100);
        let t = truncate_middle(s, 50);
        assert!(t.starts_with("aaaa"));
        assert!(t.ends_with("zzzz"));
        assert!(t.contains("truncated"));
    }

    #[test]
    fn short_output_untouched() {
        assert_eq!(truncate_middle("hi".into(), 100), "hi");
    }
}

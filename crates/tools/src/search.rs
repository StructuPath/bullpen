//! Workspace search: regex grep and glob, both gitignore-aware.

use bullpen_llm::ToolSpec;
use globset::GlobBuilder;
use ignore::WalkBuilder;
use serde_json::{Value, json};

use crate::{Tool, ToolCtx, ToolError, required_str};

const MAX_RESULTS: usize = 200;

pub struct Grep;

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: format!(
                "Regex search file contents under the workspace (gitignore-aware). \
                 Returns `path:line: text`, capped at {MAX_RESULTS} matches."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex syntax"},
                    "path": {"type": "string", "description": "Optional subdirectory to search"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn replay_safe(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let pattern = required_str(&input, "pattern")?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| ToolError::InvalidInput(format!("bad regex: {e}")))?;
        let root = match input.get("path").and_then(Value::as_str) {
            Some(sub) => crate::resolve_path(ctx, sub),
            None => ctx.workspace.clone(),
        };
        let workspace = ctx.workspace.clone();

        // File walking and matching are blocking work; keep them off the runtime.
        let result = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            for entry in WalkBuilder::new(&root).hidden(true).build() {
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue; // binary or unreadable
                };
                let display = entry
                    .path()
                    .strip_prefix(&workspace)
                    .unwrap_or(entry.path())
                    .display()
                    .to_string();
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        hits.push(format!("{display}:{}: {}", i + 1, line.trim_end()));
                        if hits.len() >= MAX_RESULTS {
                            hits.push(format!("[... capped at {MAX_RESULTS} matches ...]"));
                            return hits;
                        }
                    }
                }
            }
            hits
        })
        .await
        .map_err(|e| ToolError::Failed(format!("search task failed: {e}")))?;

        if result.is_empty() {
            Ok("no matches".into())
        } else {
            Ok(result.join("\n"))
        }
    }
}

pub struct Glob;

#[async_trait::async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: format!(
                "Find files by glob pattern (e.g. `**/*.rs`) under the workspace \
                 (gitignore-aware), capped at {MAX_RESULTS} results."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn replay_safe(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let pattern = required_str(&input, "pattern")?;
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError::InvalidInput(format!("bad glob: {e}")))?
            .compile_matcher();
        let workspace = ctx.workspace.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            for entry in WalkBuilder::new(&workspace).hidden(true).build() {
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = entry.path().strip_prefix(&workspace).unwrap_or(entry.path());
                if glob.is_match(rel) {
                    hits.push(rel.display().to_string());
                    if hits.len() >= MAX_RESULTS {
                        hits.push(format!("[... capped at {MAX_RESULTS} results ...]"));
                        break;
                    }
                }
            }
            hits.sort();
            hits
        })
        .await
        .map_err(|e| ToolError::Failed(format!("glob task failed: {e}")))?;

        if result.is_empty() {
            Ok("no matches".into())
        } else {
            Ok(result.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    steel();\n}\n").unwrap();
        std::fs::write(root.join("notes.md"), "steel beams\n").unwrap();
        let ctx = ToolCtx {
            workspace: root.to_path_buf(),
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn grep_finds_matches_with_locations() {
        let (_dir, ctx) = fixture().await;
        let out = Grep.run(&ctx, "t", json!({"pattern": "steel"})).await.unwrap();
        assert!(out.contains("src/main.rs:2:"), "{out}");
        assert!(out.contains("notes.md:1:"), "{out}");
    }

    #[tokio::test]
    async fn grep_rejects_bad_regex() {
        let (_dir, ctx) = fixture().await;
        let err = Grep.run(&ctx, "t", json!({"pattern": "["})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn glob_matches_relative_paths() {
        let (_dir, ctx) = fixture().await;
        let out = Glob.run(&ctx, "t", json!({"pattern": "**/*.rs"})).await.unwrap();
        assert_eq!(out, "src/main.rs");
        let out = Glob.run(&ctx, "t", json!({"pattern": "*.md"})).await.unwrap();
        assert_eq!(out, "notes.md");
    }
}

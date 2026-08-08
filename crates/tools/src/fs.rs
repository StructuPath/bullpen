//! File read, write, and single-occurrence edit.

use bullpen_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Tool, ToolCtx, ToolError, required_str, resolve_path, truncate_middle};

const MAX_READ_BYTES: usize = 262_144; // 256 KiB

pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a file, optionally a line window. Output is 1-indexed \
                          `line<TAB>content` and capped at 256 KiB."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "1-indexed first line"},
                    "limit": {"type": "integer", "description": "Max lines to return"}
                },
                "required": ["path"]
            }),
        }
    }

    fn parallel_safe(&self, _input: &Value) -> bool {
        true
    }

    fn replay_safe(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let path = resolve_path(ctx, required_str(&input, "path")?);
        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("cannot read {}: {e}", path.display())))?;
        let text = String::from_utf8_lossy(&raw);

        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX) as usize;

        let numbered: String = text
            .lines()
            .enumerate()
            .skip(offset - 1)
            .take(limit)
            .map(|(i, line)| format!("{}\t{line}\n", i + 1))
            .collect();

        if numbered.is_empty() {
            return Ok(format!("(empty selection: file has {} lines)", text.lines().count()));
        }
        Ok(truncate_middle(numbered, MAX_READ_BYTES))
    }
}

pub struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Write content to a file, creating parent directories. \
                          Overwrites existing content."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let path = resolve_path(ctx, required_str(&input, "path")?);
        let content = required_str(&input, "content")?;
        ctx.check_write(&path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Failed(format!("cannot create {}: {e}", parent.display())))?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| ToolError::Failed(format!("cannot write {}: {e}", path.display())))?;
        Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
    }
}

pub struct EditFile;

#[async_trait::async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace exactly one occurrence of `old_string` with \
                          `new_string`. Fails if the string is absent or ambiguous."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let path = resolve_path(ctx, required_str(&input, "path")?);
        let old = required_str(&input, "old_string")?;
        let new = required_str(&input, "new_string")?;
        if old.is_empty() {
            return Err(ToolError::InvalidInput("old_string must not be empty".into()));
        }
        ctx.check_write(&path)?;

        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("cannot read {}: {e}", path.display())))?;

        match text.matches(old).count() {
            0 => Err(ToolError::Failed(format!(
                "old_string not found in {}",
                path.display()
            ))),
            1 => {
                let updated = text.replacen(old, new, 1);
                tokio::fs::write(&path, updated)
                    .await
                    .map_err(|e| ToolError::Failed(format!("cannot write {}: {e}", path.display())))?;
                Ok(format!("edited {}", path.display()))
            }
            n => Err(ToolError::Failed(format!(
                "old_string appears {n} times in {}; provide more context to disambiguate",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &tempfile::TempDir) -> ToolCtx {
        ToolCtx::new(dir.path().to_path_buf())
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        WriteFile
            .run(&c, "t", json!({"path": "sub/f.txt", "content": "one\ntwo\nthree"}))
            .await
            .unwrap();
        let out = ReadFile.run(&c, "t", json!({"path": "sub/f.txt"})).await.unwrap();
        assert_eq!(out, "1\tone\n2\ttwo\n3\tthree\n");
    }

    #[tokio::test]
    async fn read_line_window() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        WriteFile
            .run(&c, "t", json!({"path": "f.txt", "content": "a\nb\nc\nd"}))
            .await
            .unwrap();
        let out = ReadFile
            .run(&c, "t", json!({"path": "f.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert_eq!(out, "2\tb\n3\tc\n");
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        WriteFile
            .run(&c, "t", json!({"path": "f.txt", "content": "x = 1\nx = 1\n"}))
            .await
            .unwrap();
        let err = EditFile
            .run(&c, "t", json!({"path": "f.txt", "old_string": "x = 1", "new_string": "x = 2"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2 times"));

        EditFile
            .run(&c, "t", json!({"path": "f.txt", "old_string": "x = 1\nx = 1", "new_string": "x = 2\nx = 1"}))
            .await
            .unwrap();
        let out = ReadFile.run(&c, "t", json!({"path": "f.txt"})).await.unwrap();
        assert!(out.contains("x = 2"));
    }

    #[tokio::test]
    async fn sandbox_blocks_write_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = std::sync::Arc::new(bullpen_sandbox::Sandbox::workspace(dir.path()));
        let c = ToolCtx::new(dir.path()).with_sandbox(sandbox);

        // Inside the workspace: allowed.
        WriteFile
            .run(&c, "t", json!({"path": "ok.txt", "content": "hi"}))
            .await
            .unwrap();

        // Absolute path outside the workspace: refused in-process.
        let err = WriteFile
            .run(&c, "t", json!({"path": "/etc/bullpen-escape", "content": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("sandbox"), "{err}");
    }

    #[tokio::test]
    async fn edit_missing_string_fails() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        WriteFile
            .run(&c, "t", json!({"path": "f.txt", "content": "hello"}))
            .await
            .unwrap();
        let err = EditFile
            .run(&c, "t", json!({"path": "f.txt", "old_string": "absent", "new_string": "y"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}

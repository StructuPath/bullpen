//! File read, write, and edit.
//!
//! Reads are **hashline**: every line carries a `line#hash` anchor (the
//! first four hex chars of the line's SHA-256), and `edit_file` accepts a
//! patch of hunks addressed by those anchors. An anchor is a claim about
//! content, not just a position — so an edit against a file that drifted
//! is *detected*, and when the anchored line still exists uniquely
//! elsewhere it is *recovered* rather than misapplied. The exact-string
//! mode stays for one-off replacements.

use bullpen_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Tool, ToolCtx, ToolError, required_str, resolve_path, truncate_middle};

const MAX_READ_BYTES: usize = 262_144; // 256 KiB

/// A line's anchor hash: the first two bytes of its SHA-256, in hex.
/// Anchors pair it with a line number, and recovery trusts it only when
/// it matches exactly one line — a four-hex-char collision inside one
/// file therefore degrades to an explicit error, never a wrong edit.
pub(crate) fn line_hash(line: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(line.as_bytes());
    format!("{:02x}{:02x}", digest[0], digest[1])
}

/// Render `lines[range]` as hashline output, 1-indexed.
fn hashlines(lines: &[&str], first: usize, limit: usize) -> String {
    lines
        .iter()
        .enumerate()
        .skip(first - 1)
        .take(limit)
        .map(|(i, line)| format!("{}#{}\t{line}\n", i + 1, line_hash(line)))
        .collect()
}

pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read through one path: a file, a directory, a SQLite \
                          database, or an http(s) URL. Files render as 1-indexed \
                          `line#hash<TAB>content`, capped at 256 KiB — the \
                          `line#hash` token is an anchor that `edit_file` \
                          patches accept — with an optional line window. \
                          Directories render a sorted listing. A SQLite file \
                          (detected by content) renders its schema and row \
                          counts, or runs a read-only `query` (writes are \
                          rejected by the engine). URLs are fetched with GET \
                          (capped, 30s timeout); a sandbox that denies network \
                          refuses them."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File, directory, or SQLite path, or an http(s):// URL"},
                    "offset": {"type": "integer", "description": "1-indexed first line (files only)"},
                    "limit": {"type": "integer", "description": "Max lines to return (files only)"},
                    "query": {"type": "string", "description": "Read-only SQL to run (SQLite files only)"}
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
        let raw_path = required_str(&input, "path")?;
        if raw_path.starts_with("http://") || raw_path.starts_with("https://") {
            return read_url(ctx, raw_path).await;
        }
        let path = resolve_path(ctx, raw_path);
        if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
            return list_dir(&path).await;
        }
        if crate::sqlite::is_sqlite(&path) {
            let query = input
                .get("query")
                .and_then(Value::as_str)
                .map(str::to_string);
            let db = path.clone();
            return tokio::task::spawn_blocking(move || {
                crate::sqlite::read_sqlite(&db, query.as_deref())
            })
            .await
            .map_err(|e| ToolError::Failed(format!("sqlite task failed: {e}")))?;
        }
        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("cannot read {}: {e}", path.display())))?;
        let text = String::from_utf8_lossy(&raw);

        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX) as usize;

        let lines: Vec<&str> = text.lines().collect();
        let numbered = hashlines(&lines, offset, limit);

        if numbered.is_empty() {
            return Ok(format!(
                "(empty selection: file has {} lines)",
                text.lines().count()
            ));
        }
        Ok(truncate_middle(numbered, MAX_READ_BYTES))
    }
}

/// How much of a URL body is kept. Bounded while streaming, not after: a
/// response has no business filling memory just to be truncated.
const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;

/// GET a URL. The sandbox's network capability governs this exactly as it
/// governs shell commands: `--sandbox-strict` cuts it.
async fn read_url(ctx: &ToolCtx, url: &str) -> Result<String, ToolError> {
    if let Some(sandbox) = &ctx.sandbox
        && !sandbox.capabilities().allow_network
    {
        return Err(ToolError::Failed(
            "sandbox: network access is disabled, so URLs cannot be fetched".into(),
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ToolError::Failed(format!("http client: {e}")))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| ToolError::Failed(format!("GET {url}: {e}")))?;
    let status = response.status();

    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    let mut clipped = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ToolError::Failed(format!("GET {url}: {e}")))?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_FETCH_BYTES {
            body.truncate(MAX_FETCH_BYTES);
            clipped = true;
            break;
        }
    }
    let text = String::from_utf8_lossy(&body);
    if !status.is_success() {
        return Err(ToolError::Failed(format!(
            "GET {url} returned {status}\n{}",
            truncate_middle(text.into_owned(), 2_000)
        )));
    }
    Ok(truncate_middle(
        format!(
            "GET {url} → {status}{}\n\n{text}",
            if clipped { " (body clipped)" } else { "" }
        ),
        MAX_READ_BYTES,
    ))
}

/// A directory as a sorted listing: directories first, sizes for files.
async fn list_dir(path: &std::path::Path) -> Result<String, ToolError> {
    let mut reader = tokio::fs::read_dir(path)
        .await
        .map_err(|e| ToolError::Failed(format!("cannot read {}: {e}", path.display())))?;
    let mut entries = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| ToolError::Failed(format!("cannot read {}: {e}", path.display())))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().await;
        let is_dir = meta.as_ref().is_ok_and(|m| m.is_dir());
        let size = meta.map(|m| m.len()).unwrap_or(0);
        entries.push((!is_dir, name, size));
    }
    if entries.is_empty() {
        return Ok(format!("(empty directory {})", path.display()));
    }
    entries.sort();
    let mut out = format!(
        "directory {} ({} entries):\n",
        path.display(),
        entries.len()
    );
    for (is_file, name, size) in entries {
        if is_file {
            out.push_str(&format!("  {name}  {size} bytes\n"));
        } else {
            out.push_str(&format!("  {name}/\n"));
        }
    }
    Ok(truncate_middle(out, MAX_READ_BYTES))
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
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::Failed(format!("cannot create {}: {e}", parent.display()))
            })?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| ToolError::Failed(format!("cannot write {}: {e}", path.display())))?;
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}

pub struct EditFile;

/// A parsed anchor: 1-indexed line plus its content hash. Line 0 is the
/// virtual top-of-file anchor, valid only for `insert_after`.
fn parse_anchor(raw: &str) -> Result<(usize, String), ToolError> {
    if raw == "0" {
        return Ok((0, String::new()));
    }
    let invalid = || {
        ToolError::InvalidInput(format!(
            "bad anchor `{raw}` — use the `line#hash` token from read_file (or \"0\" \
             with insert_after for the top of the file)"
        ))
    };
    let (line, hash) = raw.split_once('#').ok_or_else(invalid)?;
    let line: usize = line.parse().map_err(|_| invalid())?;
    if line == 0 || hash.len() != 4 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok((line, hash.to_ascii_lowercase()))
}

/// Resolve an anchor to a 0-based index. The hash decides, the line number
/// only locates: a line that *moved* (hash found on exactly one other line)
/// is followed there; a line that *changed* (hash on zero or several lines)
/// fails with fresh context to re-anchor from, never a misapplied edit.
fn resolve_anchor(lines: &[&str], line: usize, hash: &str) -> Result<(usize, bool), ToolError> {
    if line >= 1 && line <= lines.len() && line_hash(lines[line - 1]) == hash {
        return Ok((line - 1, false));
    }
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| line_hash(l) == hash)
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [only] => Ok((*only, true)),
        _ => {
            let center = line.clamp(1, lines.len().max(1));
            let first = center.saturating_sub(3).max(1);
            Err(ToolError::Failed(format!(
                "stale anchor {line}#{hash}: {} — re-anchor from the current \
                 content:\n{}",
                if matches.is_empty() {
                    "that line's content is no longer in the file"
                } else {
                    "that content now appears on several lines"
                },
                hashlines(lines, first, 7),
            )))
        }
    }
}

/// One resolved hunk as a splice: replace `start..end` with `content`.
struct Splice {
    start: usize,
    end: usize,
    content: Vec<String>,
}

fn parse_hunks(lines: &[&str], hunks: &[Value]) -> Result<(Vec<Splice>, usize), ToolError> {
    let mut splices = Vec::new();
    let mut recovered = 0;
    for hunk in hunks {
        let op = hunk.get("op").and_then(Value::as_str).unwrap_or("replace");
        let anchor = hunk
            .get("anchor")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("each hunk needs an `anchor`".into()))?;
        let (line, hash) = parse_anchor(anchor)?;
        let content: Option<Vec<String>> = hunk
            .get("content")
            .and_then(Value::as_str)
            .map(|s| s.split('\n').map(str::to_string).collect());
        let to = hunk.get("to").and_then(Value::as_str);

        let mut resolve = |line, hash: &str| {
            resolve_anchor(lines, line, hash).inspect(|(_, moved)| recovered += *moved as usize)
        };
        match op {
            "insert_after" => {
                if to.is_some() {
                    return Err(ToolError::InvalidInput(
                        "insert_after takes no `to` — it inserts at one point".into(),
                    ));
                }
                let content = content.ok_or_else(|| {
                    ToolError::InvalidInput("insert_after needs `content`".into())
                })?;
                let at = if line == 0 {
                    0
                } else {
                    resolve(line, &hash)?.0 + 1
                };
                splices.push(Splice {
                    start: at,
                    end: at,
                    content,
                });
            }
            "replace" | "delete" => {
                if line == 0 {
                    return Err(ToolError::InvalidInput(
                        "anchor \"0\" is only for insert_after".into(),
                    ));
                }
                let content = match (op, content) {
                    ("replace", Some(content)) => content,
                    ("replace", None) => {
                        return Err(ToolError::InvalidInput("replace needs `content`".into()));
                    }
                    ("delete", None) => Vec::new(),
                    ("delete", Some(_)) => {
                        return Err(ToolError::InvalidInput(
                            "delete takes no `content` — use replace to substitute".into(),
                        ));
                    }
                    _ => unreachable!(),
                };
                let (start, _) = resolve(line, &hash)?;
                let end = match to {
                    None => start + 1,
                    Some(to) => {
                        let (line, hash) = parse_anchor(to)?;
                        if line == 0 {
                            return Err(ToolError::InvalidInput(
                                "`to` must be a real line anchor".into(),
                            ));
                        }
                        let (end, _) = resolve(line, &hash)?;
                        if end < start {
                            return Err(ToolError::InvalidInput(format!(
                                "`to` anchor resolves above `anchor` ({} < {})",
                                end + 1,
                                start + 1
                            )));
                        }
                        end + 1
                    }
                };
                splices.push(Splice {
                    start,
                    end,
                    content,
                });
            }
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unknown op `{other}` (expected replace, insert_after, or delete)"
                )));
            }
        }
    }

    // Hunks must not touch — including two insertions at one point, whose
    // order the input cannot express.
    splices.sort_by_key(|s| (s.start, s.end));
    for pair in splices.windows(2) {
        if pair[1].start < pair[0].end || pair[1].start == pair[0].start {
            return Err(ToolError::InvalidInput(
                "hunks overlap — merge them or patch in two calls".into(),
            ));
        }
    }
    Ok((splices, recovered))
}

#[async_trait::async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Edit a file, two ways. Exact string: replace exactly one \
                          occurrence of `old_string` with `new_string` (fails if \
                          absent or ambiguous). Hashline patch: `patch` is an array \
                          of hunks addressed by the `line#hash` anchors read_file \
                          shows — {anchor, op: replace|insert_after|delete, to?, \
                          content?}. `to` extends replace/delete to a span; anchor \
                          \"0\" with insert_after prepends at the top; multi-line \
                          `content` uses newlines. A moved anchor is followed while \
                          its content is unique; a changed one fails with fresh \
                          context to re-anchor from. Use exactly one mode per call."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "patch": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "anchor": {"type": "string", "description": "`line#hash` token from read_file"},
                                "op": {"type": "string", "enum": ["replace", "insert_after", "delete"]},
                                "to": {"type": "string", "description": "End anchor for a replace/delete span (inclusive)"},
                                "content": {"type": "string", "description": "Replacement or inserted lines"}
                            },
                            "required": ["anchor", "op"]
                        }
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let path = resolve_path(ctx, required_str(&input, "path")?);
        ctx.check_write(&path)?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("cannot read {}: {e}", path.display())))?;

        let updated = match (input.get("old_string"), input.get("patch")) {
            (Some(_), Some(_)) => {
                return Err(ToolError::InvalidInput(
                    "use either old_string/new_string or `patch`, not both".into(),
                ));
            }
            (Some(_), None) => {
                let old = required_str(&input, "old_string")?;
                let new = required_str(&input, "new_string")?;
                if old.is_empty() {
                    return Err(ToolError::InvalidInput(
                        "old_string must not be empty".into(),
                    ));
                }
                match text.matches(old).count() {
                    0 => {
                        return Err(ToolError::Failed(format!(
                            "old_string not found in {}",
                            path.display()
                        )));
                    }
                    1 => (text.replacen(old, new, 1), String::new()),
                    n => {
                        return Err(ToolError::Failed(format!(
                            "old_string appears {n} times in {}; provide more \
                             context to disambiguate",
                            path.display()
                        )));
                    }
                }
            }
            (None, Some(patch)) => {
                let hunks = patch.as_array().filter(|h| !h.is_empty()).ok_or_else(|| {
                    ToolError::InvalidInput("`patch` must be a non-empty array of hunks".into())
                })?;
                let lines: Vec<&str> = text.lines().collect();
                let (splices, recovered) = parse_hunks(&lines, hunks)?;
                let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
                for splice in splices.iter().rev() {
                    out.splice(splice.start..splice.end, splice.content.iter().cloned());
                }
                let mut new_text = out.join("\n");
                if text.ends_with('\n') && !new_text.is_empty() {
                    new_text.push('\n');
                }
                let note = match recovered {
                    0 => String::new(),
                    n => format!(" ({n} moved anchor(s) followed by content hash)"),
                };
                (new_text, format!(", {} hunk(s){note}", splices.len()))
            }
            (None, None) => {
                return Err(ToolError::InvalidInput(
                    "provide old_string/new_string or a `patch`".into(),
                ));
            }
        };

        let (new_text, detail) = updated;
        tokio::fs::write(&path, new_text)
            .await
            .map_err(|e| ToolError::Failed(format!("cannot write {}: {e}", path.display())))?;
        Ok(format!("edited {}{detail}", path.display()))
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
            .run(
                &c,
                "t",
                json!({"path": "sub/f.txt", "content": "one\ntwo\nthree"}),
            )
            .await
            .unwrap();
        let out = ReadFile
            .run(&c, "t", json!({"path": "sub/f.txt"}))
            .await
            .unwrap();
        assert_eq!(
            out,
            format!(
                "1#{}\tone\n2#{}\ttwo\n3#{}\tthree\n",
                line_hash("one"),
                line_hash("two"),
                line_hash("three")
            )
        );
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
        assert_eq!(
            out,
            format!("2#{}\tb\n3#{}\tc\n", line_hash("b"), line_hash("c"))
        );
    }

    /// The anchor for 1-indexed `line` of `content`, as read_file shows it.
    fn anchor(content: &str, line: usize) -> String {
        let text = content.lines().nth(line - 1).unwrap();
        format!("{line}#{}", line_hash(text))
    }

    async fn file_with(c: &ToolCtx, content: &str) -> String {
        WriteFile
            .run(c, "t", json!({"path": "f.txt", "content": content}))
            .await
            .unwrap();
        content.to_string()
    }

    async fn read_back(c: &ToolCtx) -> String {
        tokio::fs::read_to_string(c.workspace.join("f.txt"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn patch_applies_replace_insert_and_delete_in_one_call() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let text = file_with(&c, "one\ntwo\nthree\nfour\n").await;

        let out = EditFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "patch": [
                    {"anchor": anchor(&text, 2), "op": "replace", "content": "TWO\nTWO-B"},
                    {"anchor": anchor(&text, 4), "op": "delete"},
                    {"anchor": "0", "op": "insert_after", "content": "zero"},
                ]}),
            )
            .await
            .unwrap();
        assert!(out.contains("3 hunk(s)"), "{out}");
        assert_eq!(read_back(&c).await, "zero\none\nTWO\nTWO-B\nthree\n");
    }

    #[tokio::test]
    async fn patch_replaces_a_span_and_preserves_no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let text = file_with(&c, "a\nb\nc\nd").await;

        EditFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "patch": [
                    {"anchor": anchor(&text, 2), "to": anchor(&text, 3), "op": "replace", "content": "BC"},
                ]}),
            )
            .await
            .unwrap();
        assert_eq!(read_back(&c).await, "a\nBC\nd");
    }

    #[tokio::test]
    async fn a_moved_anchor_is_followed_by_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        // Anchors taken from this layout...
        let stale = "target\nfiller\n";
        // ...but lines were inserted above before the patch lands.
        file_with(&c, "new-top\nalso-new\ntarget\nfiller\n").await;

        let out = EditFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "patch": [
                    {"anchor": anchor(stale, 1), "op": "replace", "content": "hit"},
                ]}),
            )
            .await
            .unwrap();
        assert!(out.contains("1 moved anchor"), "{out}");
        assert_eq!(read_back(&c).await, "new-top\nalso-new\nhit\nfiller\n");
    }

    #[tokio::test]
    async fn a_changed_anchor_fails_with_fresh_context_not_a_wrong_edit() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let stale = "original\nkeep\n";
        file_with(&c, "rewritten\nkeep\n").await;

        let err = EditFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "patch": [
                    {"anchor": anchor(stale, 1), "op": "replace", "content": "x"},
                ]}),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stale anchor"), "{msg}");
        // The error carries current hashlines so the model can re-anchor.
        assert!(
            msg.contains(&format!("1#{}\trewritten", line_hash("rewritten"))),
            "{msg}"
        );
        assert_eq!(read_back(&c).await, "rewritten\nkeep\n");
    }

    #[tokio::test]
    async fn ambiguous_recovery_fails_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let stale = "dup\nunique\n";
        // The anchored content now appears twice, in neither original spot.
        file_with(&c, "moved-a\ndup\ndup\n").await;

        let err = EditFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "patch": [
                    {"anchor": anchor(stale, 1), "op": "delete"},
                ]}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("several lines"), "{err}");
    }

    #[tokio::test]
    async fn malformed_patches_are_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let text = file_with(&c, "a\nb\nc\n").await;
        let a1 = anchor(&text, 1);
        let a2 = anchor(&text, 2);

        for patch in [
            json!([]),
            json!([{"op": "replace", "content": "x"}]),
            json!([{"anchor": "nope", "op": "replace", "content": "x"}]),
            json!([{"anchor": a1, "op": "explode"}]),
            json!([{"anchor": a1, "op": "replace"}]),
            json!([{"anchor": a1, "op": "delete", "content": "x"}]),
            json!([{"anchor": a1, "op": "insert_after"}]),
            json!([{"anchor": "0", "op": "delete"}]),
            json!([{"anchor": a2, "to": a1, "op": "delete"}]),
            // Overlapping hunks, and two insertions at one point.
            json!([
                {"anchor": a1, "to": a2, "op": "delete"},
                {"anchor": a2, "op": "replace", "content": "x"},
            ]),
            json!([
                {"anchor": a1, "op": "insert_after", "content": "x"},
                {"anchor": a1, "op": "insert_after", "content": "y"},
            ]),
        ] {
            let err = EditFile
                .run(&c, "t", json!({"path": "f.txt", "patch": patch}))
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidInput(_)), "{patch}: {err}");
        }

        // Both modes at once is also a caller error.
        let err = EditFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "old_string": "a", "new_string": "b", "patch": [{"anchor": a1, "op": "delete"}]}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        WriteFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "content": "x = 1\nx = 1\n"}),
            )
            .await
            .unwrap();
        let err = EditFile
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "old_string": "x = 1", "new_string": "x = 2"}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2 times"));

        EditFile
            .run(&c, "t", json!({"path": "f.txt", "old_string": "x = 1\nx = 1", "new_string": "x = 2\nx = 1"}))
            .await
            .unwrap();
        let out = ReadFile
            .run(&c, "t", json!({"path": "f.txt"}))
            .await
            .unwrap();
        assert!(out.contains("x = 2"));
    }

    #[tokio::test]
    async fn a_directory_reads_as_a_sorted_listing() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "12345")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "1")
            .await
            .unwrap();

        let out = ReadFile.run(&c, "t", json!({"path": "."})).await.unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("directory"), "{out}");
        assert!(lines[0].contains("3 entries"), "{out}");
        // Directories first, then files alphabetically, with sizes.
        assert_eq!(
            &lines[1..],
            ["  sub/", "  a.txt  1 bytes", "  b.txt  5 bytes"]
        );
    }

    #[tokio::test]
    async fn a_sqlite_database_reads_as_schema_then_queries() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let conn = rusqlite::Connection::open(dir.path().join("s.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE parts (id INTEGER PRIMARY KEY, sku TEXT);
             INSERT INTO parts (sku) VALUES ('W12x26');",
        )
        .unwrap();
        drop(conn);

        let out = ReadFile
            .run(&c, "t", json!({"path": "s.db"}))
            .await
            .unwrap();
        assert!(out.contains("parts (1 rows): id, sku"), "{out}");

        let out = ReadFile
            .run(
                &c,
                "t",
                json!({"path": "s.db", "query": "SELECT sku FROM parts"}),
            )
            .await
            .unwrap();
        assert!(out.contains("W12x26"), "{out}");
    }

    /// One canned HTTP exchange on a local port; returns the URL to hit.
    fn serve_once(response: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn urls_fetch_through_the_same_path() {
        let url =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
        let dir = tempfile::tempdir().unwrap();
        let out = ReadFile
            .run(&ctx(&dir), "t", json!({"path": url}))
            .await
            .unwrap();
        assert!(out.contains("200"), "{out}");
        assert!(out.contains("hello"), "{out}");
    }

    #[tokio::test]
    async fn a_failing_url_is_an_error_carrying_the_status() {
        let url = serve_once(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone",
        );
        let dir = tempfile::tempdir().unwrap();
        let err = ReadFile
            .run(&ctx(&dir), "t", json!({"path": url}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"), "{msg}");
        assert!(msg.contains("gone"), "{msg}");
    }

    #[tokio::test]
    async fn a_network_denying_sandbox_refuses_urls() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = std::sync::Arc::new(bullpen_sandbox::Sandbox::strict(dir.path()));
        let c = ToolCtx::new(dir.path()).with_sandbox(sandbox);
        let err = ReadFile
            .run(&c, "t", json!({"path": "http://127.0.0.1:1/never"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("network"), "{err}");
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
            .run(
                &c,
                "t",
                json!({"path": "/etc/bullpen-escape", "content": "x"}),
            )
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
            .run(
                &c,
                "t",
                json!({"path": "f.txt", "old_string": "absent", "new_string": "y"}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}

//! SQLite databases behind the one read path.
//!
//! A database file (recognized by its magic header, not its extension)
//! renders as a schema overview — tables with their columns and row
//! counts — and an optional `query` runs read-only SQL against it.
//! Read-only is enforced by the engine, not by string inspection: the
//! connection opens `mode=ro` with `query_only` on, so a write fails in
//! SQLite itself no matter how it is spelled. A database another process
//! is writing (WAL) falls back to the `immutable=1` open bullpen's own
//! docs recommend for inspecting its store.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::{ToolError, truncate_middle};

const MAX_ROWS: usize = 200;
const MAX_OUTPUT_BYTES: usize = 100_000;

/// The 16-byte header every SQLite 3 database starts with.
pub(crate) fn is_sqlite(path: &Path) -> bool {
    std::fs::File::open(path)
        .and_then(|mut f| {
            use std::io::Read;
            let mut magic = [0u8; 16];
            f.read_exact(&mut magic)?;
            Ok(&magic == b"SQLite format 3\0")
        })
        .unwrap_or(false)
}

/// Open read-only; when WAL machinery blocks that (a live writer, a
/// missing shm), reopen immutable — a point-in-time snapshot.
fn open_read_only(path: &Path) -> Result<Connection, ToolError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(path, flags) {
        Ok(conn) => conn,
        Err(_) => Connection::open_with_flags(
            format!("file:{}?immutable=1", path.display()),
            flags | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| ToolError::Failed(format!("cannot open {}: {e}", path.display())))?,
    };
    // Belt and braces on top of mode=ro; also downgrades any accidental
    // write into an immediate engine error rather than a lock attempt.
    conn.pragma_update(None, "query_only", "ON")
        .map_err(|e| ToolError::Failed(format!("cannot open {}: {e}", path.display())))?;
    Ok(conn)
}

fn db_err(path: &Path, e: rusqlite::Error) -> ToolError {
    ToolError::Failed(format!("sqlite {}: {e}", path.display()))
}

/// The default view: every table with its columns and row count.
fn overview(path: &Path, conn: &Connection) -> Result<String, ToolError> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(|e| db_err(path, e))?;
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| db_err(path, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| db_err(path, e))?;

    if tables.is_empty() {
        return Ok(format!("SQLite database {} (no tables)", path.display()));
    }
    let mut out = format!(
        "SQLite database {} ({} table(s)) — pass `query` to run read-only SQL:\n",
        path.display(),
        tables.len()
    );
    for table in tables {
        // The table name comes from sqlite_master itself; quoting guards
        // names with spaces or keywords, not injection.
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })
            .unwrap_or(-1);
        let mut cols = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))
            .map_err(|e| db_err(path, e))?;
        let columns: Vec<String> = cols
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| db_err(path, e))?
            .collect::<Result<_, _>>()
            .map_err(|e| db_err(path, e))?;
        out.push_str(&format!(
            "  {table} ({count} rows): {}\n",
            columns.join(", ")
        ));
    }
    Ok(truncate_middle(out, MAX_OUTPUT_BYTES))
}

/// Run one read-only statement and render rows as TSV under a header.
fn run_query(path: &Path, conn: &Connection, query: &str) -> Result<String, ToolError> {
    let mut stmt = conn.prepare(query).map_err(|e| db_err(path, e))?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let width = names.len();
    let mut rows = stmt.query([]).map_err(|e| db_err(path, e))?;

    let mut lines = vec![names.join("\t")];
    let mut total = 0usize;
    while let Some(row) = rows.next().map_err(|e| db_err(path, e))? {
        total += 1;
        if total > MAX_ROWS {
            continue; // keep counting, stop rendering
        }
        let mut cells = Vec::with_capacity(width);
        for i in 0..width {
            use rusqlite::types::ValueRef;
            cells.push(match row.get_ref(i).map_err(|e| db_err(path, e))? {
                ValueRef::Null => "NULL".to_string(),
                ValueRef::Integer(v) => v.to_string(),
                ValueRef::Real(v) => v.to_string(),
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
            });
        }
        lines.push(cells.join("\t"));
    }

    let mut out = format!("{total} row(s):\n{}", lines.join("\n"));
    if total > MAX_ROWS {
        out.push_str(&format!("\n[rendered the first {MAX_ROWS}]"));
    }
    Ok(truncate_middle(out, MAX_OUTPUT_BYTES))
}

/// Entry point from `read_file`: overview without a query, TSV with one.
pub(crate) fn read_sqlite(path: &Path, query: Option<&str>) -> Result<String, ToolError> {
    let conn = open_read_only(path)?;
    match query {
        None => overview(path, &conn),
        Some(query) => run_query(path, &conn, query),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("t.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE jobs (id INTEGER PRIMARY KEY, name TEXT, done INTEGER);
             INSERT INTO jobs (name, done) VALUES ('weld', 1), ('paint', 0);
             CREATE TABLE empty_one (x TEXT);",
        )
        .unwrap();
        path
    }

    #[test]
    fn detects_sqlite_by_magic_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        assert!(is_sqlite(&db));

        let plain = dir.path().join("notes.db");
        std::fs::write(&plain, "just text with a .db name").unwrap();
        assert!(!is_sqlite(&plain));
        assert!(!is_sqlite(&dir.path().join("missing.db")));
    }

    #[test]
    fn overview_lists_tables_columns_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let out = read_sqlite(&sample(&dir), None).unwrap();
        assert!(out.contains("2 table(s)"), "{out}");
        assert!(out.contains("jobs (2 rows): id, name, done"), "{out}");
        assert!(out.contains("empty_one (0 rows): x"), "{out}");
    }

    #[test]
    fn queries_render_tsv_and_cap_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let out = read_sqlite(&db, Some("SELECT name, done FROM jobs ORDER BY id")).unwrap();
        assert_eq!(out, "2 row(s):\nname\tdone\nweld\t1\npaint\t0");

        let conn = Connection::open(&db).unwrap();
        for i in 0..250 {
            conn.execute(
                "INSERT INTO jobs (name, done) VALUES (?1, 0)",
                [format!("j{i}")],
            )
            .unwrap();
        }
        let out = read_sqlite(&db, Some("SELECT id FROM jobs")).unwrap();
        assert!(out.starts_with("252 row(s):"), "{out}");
        assert!(out.contains("[rendered the first 200]"), "{out}");
    }

    #[test]
    fn writes_are_rejected_by_the_engine_not_string_matching() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        for sql in [
            "INSERT INTO jobs (name) VALUES ('sneak')",
            "DELETE FROM jobs",
            "DROP TABLE jobs",
            // Even spelled unusually, the engine sees a write.
            "  \n/* c */ UPDATE jobs SET done = 1",
        ] {
            let err = read_sqlite(&db, Some(sql)).unwrap_err();
            let msg = err.to_string().to_lowercase();
            assert!(
                msg.contains("readonly") || msg.contains("read-only") || msg.contains("query_only"),
                "{sql}: {msg}"
            );
        }
        // Nothing changed.
        let out = read_sqlite(&db, Some("SELECT COUNT(*) AS n FROM jobs")).unwrap();
        assert!(out.contains("\n2"), "{out}");
    }

    #[test]
    fn bad_sql_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_sqlite(&sample(&dir), Some("SELEKT nope")).unwrap_err();
        assert!(err.to_string().contains("sqlite"), "{err}");
    }
}

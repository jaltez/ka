//! Full-text search over ka strands. Walks the strands directory,
//! indexes user/assistant message text into SQLite FTS5 (bundled — no
//! external SQLite), and answers queries with ranked snippets.
//!
//! Incremental: per-file mtime is tracked; unchanged files are skipped,
//! changed ones are re-indexed (old rows deleted first).

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// One search result row.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRow {
    /// Strand id.
    pub session: String,
    /// Message timestamp (RFC 3339 from the strand header era).
    pub ts: String,
    /// `user` or `assistant`.
    pub role: String,
    /// FTS5 snippet around the match.
    pub snippet: String,
}

/// Rebuild outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildStats {
    /// Files (re)indexed (new or changed).
    pub indexed: usize,
    /// Files skipped (unchanged mtime).
    pub skipped: usize,
}

/// Default index database location: `<data>/ka/index.db`.
pub fn default_db_path() -> PathBuf {
    if let Ok(dir) = std::env::var("KA_DATA_DIR") {
        return PathBuf::from(dir).join("index.db");
    }
    ka_strand::data_dir().join("index.db")
}

/// Open (creating) the index database and ensure the schema.
pub fn open_db(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
             path TEXT PRIMARY KEY,
             mtime_ns INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY,
             ts TEXT NOT NULL,
             title TEXT NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS messages USING fts5(
             session,
             role,
             content,
             tokenize = 'porter unicode61'
         );",
    )
    .map_err(|e| format!("schema: {e}"))?;
    Ok(conn)
}

fn mtime_ns(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos() as i64)
}

/// Index every `.jsonl` strand under `strands_root` (one level of
/// per-cwd subdirectories). Incremental by mtime.
pub fn rebuild(strands_root: &Path, db: &Connection) -> Result<RebuildStats, String> {
    let mut indexed = 0usize;
    let mut skipped = 0usize;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut loose: Vec<PathBuf> = Vec::new();
    let entries = match std::fs::read_dir(strands_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RebuildStats {
                indexed: 0,
                skipped: 0,
            });
        }
        Err(e) => return Err(format!("read {}: {e}", strands_root.display())),
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            dirs.push(p);
        } else if p.extension().is_none_or(|e| e != "jsonl") {
            continue;
        } else {
            loose.push(p);
        }
    }
    for file in loose {
        // one malformed strand (torn write, future record format) must
        // not poison the whole index — skip it and keep going
        match index_one_file(&file, db) {
            Ok(true) => indexed += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                skipped += 1;
                eprintln!("ka index: skipping {}: {e}", file.display());
            }
        }
    }
    for dir in dirs {
        let files = match std::fs::read_dir(&dir) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            match index_one_file(&path, db) {
                Ok(true) => indexed += 1,
                Ok(false) => skipped += 1,
                Err(e) => {
                    skipped += 1;
                    eprintln!("ka index: skipping {}: {e}", path.display());
                }
            }
        }
    }
    Ok(RebuildStats { indexed, skipped })
}

/// Per-file index step: mtime check, (re)index, bookkeeping. Returns
/// true when the file was (re)indexed, false when skipped as unchanged.
fn index_one_file(path: &Path, db: &Connection) -> Result<bool, String> {
    let Some(current_ns) = mtime_ns(path) else {
        return Ok(false);
    };
    let known: Option<i64> = db
        .query_row(
            "SELECT mtime_ns FROM files WHERE path = ?1",
            [path.to_string_lossy().as_ref()],
            |r| r.get(0),
        )
        .ok();
    if known == Some(current_ns) {
        return Ok(false);
    }
    index_file(path, db)?;
    db.execute(
        "INSERT INTO files (path, mtime_ns) VALUES (?1, ?2)
         ON CONFLICT(path) DO UPDATE SET mtime_ns = excluded.mtime_ns",
        [path.to_string_lossy().as_ref(), &current_ns.to_string()],
    )
    .map_err(|e| format!("files update: {e}"))?;
    Ok(true)
}

/// Index one strand file: replace its rows and register the session.
fn index_file(path: &Path, db: &Connection) -> Result<(), String> {
    let strand = ka_strand::StrandFile::open(path).map_err(|e| format!("open strand: {e}"))?;
    let records = strand.records();

    // header: id + ts
    let (id, ts) = records
        .iter()
        .find_map(|r| match r {
            ka_strand::Record::Header { id, ts, .. } => Some((id.0.clone(), ts.clone())),
            _ => None,
        })
        .ok_or_else(|| "strand has no header".to_string())?;

    // collect (role, content) pairs
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut title = String::new();
    for r in records.iter() {
        match r {
            ka_strand::Record::Message { role, content, .. } => {
                let role = match role {
                    ka_strand::Role::User => "user",
                    ka_strand::Role::Assistant | ka_strand::Role::System => "assistant",
                    ka_strand::Role::Tool => continue,
                };
                if !content.trim().is_empty() {
                    rows.push((role.to_string(), content.clone()));
                }
            }
            ka_strand::Record::Title { title: t, .. } => title = t.clone(),
            _ => {}
        }
    }

    db.execute_batch("BEGIN")
        .map_err(|e| format!("begin: {e}"))?;
    let result = (|| -> Result<(), rusqlite::Error> {
        db.execute("DELETE FROM messages WHERE session = ?1", [&id])?;
        db.execute(
            "INSERT INTO sessions (id, ts, title) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET ts = excluded.ts, title = excluded.title",
            [&id, &ts, &title],
        )?;
        for (role, content) in &rows {
            db.execute(
                "INSERT INTO messages (session, role, content) VALUES (?1, ?2, ?3)",
                [&id, role, content],
            )?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => db
            .execute_batch("COMMIT")
            .map_err(|e| format!("commit: {e}")),
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(format!("index {path:?}: {e}"))
        }
    }
}

/// FTS5 query → top-N rows with snippets. `session` filters to one
/// strand id.
pub fn search(
    db: &Connection,
    query: &str,
    session: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchRow>, String> {
    let limit_i = limit as i64;
    let mut stmt = db
        .prepare(
            "SELECT messages.session, sessions.ts, messages.role,
                    snippet(messages, 2, '[', ']', '…', 12)
             FROM messages
             JOIN sessions ON sessions.id = messages.session
             WHERE messages MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let quoted = format!("\"{}\"", query.replace('"', "\"\""));
    let pattern = match session {
        Some(s) => format!("{quoted} AND session:\"{s}\""),
        None => quoted,
    };
    let rows = stmt
        .query_map([&pattern, &limit_i.to_string()], |r| {
            Ok(SearchRow {
                session: r.get::<_, String>(0)?,
                ts: r.get::<_, String>(1)?,
                role: r.get::<_, String>(2)?,
                snippet: r.get::<_, String>(3)?,
            })
        })
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn rebuild_and_search_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ka-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("strands").join("test-session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = format!(
            "{{\"record\":\"header\",\"id\":\"s-test-1\",\"ts\":\"2026-09-07T00:00:00Z\",\"cwd\":\"{}\",\"version\":1,\"repo\":null,\"parent\":null}}",
            dir.display()
        );
        let m1 = r#"{"record":"message","id":"m1","role":"user","content":"Where is the QUANTUM_TUNNEL constant defined?"}"#;
        let m2 = r#"{"record":"message","id":"m2","role":"assistant","content":"It lives in src/tunnel.rs near the parser."}"#;
        std::fs::write(&path, format!("{header}\n{m1}\n{m2}\n")).unwrap();

        let db = open_db(&dir.join("index.db")).unwrap();
        // rebuild walks one level of per-cwd subdirectories
        let stats = rebuild(&dir.join("strands"), &db).unwrap();
        assert_eq!(stats.indexed, 1);

        let hits = search(&db, "QUANTUM_TUNNEL", None, 20).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].role, "user");
        assert!(hits[0].snippet.contains("QUANTUM_TUNNEL"));

        // incremental: unchanged file skipped, changed file re-indexed
        let stats = rebuild(&dir.join("strands"), &db).unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.indexed, 0);
        std::fs::write(
            &path,
            format!(
                "{}\n{{\"record\":\"message\",\"id\":\"m2\",\"role\":\"assistant\",\"content\":\"moved to src/deep/tunnel.rs\"}}\n",
                std::fs::read_to_string(&path).unwrap().lines().next().unwrap()
            ),
        )
        .unwrap();
        let stats = rebuild(&dir.join("strands"), &db).unwrap();
        assert_eq!(stats.indexed, 1);
        let hits = search(&db, "tunnel.rs", None, 20).unwrap();
        assert!(hits.iter().any(|h| h.snippet.contains("deep")));

        // session filter
        let strand_id = {
            let first = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
            v["id"].as_str().unwrap().to_string()
        };
        let hits = search(&db, "tunnel", Some(&strand_id), 20).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.session == strand_id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fts5_available_in_bundled_build() {
        let conn = open_db(&std::env::temp_dir().join("ka-index-ftsq.db")).unwrap();
        let ok: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_compile_options WHERE compile_options LIKE 'ENABLE_FTS5'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        assert!(ok, "bundled SQLite must enable FTS5");
    }
}

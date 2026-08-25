//! The read hand: line-ranged file reads with caps and ledger minting.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};

/// Maximum lines returned in one read.
pub const MAX_LINES: usize = 2_000;
/// Maximum bytes returned in one read.
pub const MAX_BYTES: usize = 256_000;

/// The read tool.
pub struct ReadHand;

impl Hand for ReadHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "read",
            description: "Read a file. Returns numbered lines. Use offset/limit for ranges. \
                Directories return a shallow listing."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to cwd or absolute)" },
                    "offset": { "type": "integer", "description": "1-based first line" },
                    "limit": { "type": "integer", "description": "Max lines to return" }
                },
                "required": ["path"]
            }),
            clearance: super::Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(path_str) = args.get("path").and_then(Value::as_str) else {
                return ToolOutput::err("read: missing required 'path'");
            };
            let path = resolve(ctx, path_str);
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
            };
            if meta.is_dir() {
                return list_dir(&path);
            }
            ctx.ledger.lock().mint(&path, &meta);

            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
            };
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1) as usize;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|l| (l as usize).min(MAX_LINES))
                .unwrap_or(MAX_LINES);

            let lines: Vec<&str> = text.lines().collect();
            let total = lines.len();
            let start = (offset - 1).min(total);
            let end = (start + limit).min(total);
            let mut out = String::new();
            let mut bytes = 0usize;
            for (i, line) in lines[start..end].iter().enumerate() {
                let numbered = format!("{}\t{}\n", start + i + 1, line);
                bytes += numbered.len();
                if bytes > MAX_BYTES {
                    out.push_str("[...byte cap reached; narrow with offset/limit]\n");
                    break;
                }
                out.push_str(&numbered);
            }
            if end < total {
                out.push_str(&format!(
                    "[...{} of {} lines shown; lines {}-{} remain]\n",
                    end - start,
                    total,
                    end + 1,
                    total
                ));
            }
            if out.is_empty() {
                out.push_str("(empty file)\n");
            }
            ToolOutput::ok(out)
        })
    }
}

fn list_dir(path: &std::path::Path) -> ToolOutput {
    let Ok(entries) = std::fs::read_dir(path) else {
        return ToolOutput::err(format!("read {}: cannot list", path.display()));
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .take(500)
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    if names.is_empty() {
        return ToolOutput::ok("(empty directory)");
    }
    ToolOutput::ok(names.join("\n"))
}

pub(crate) fn resolve(ctx: &HandContext, p: &str) -> PathBuf {
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|_| PathBuf::from(p))
    } else {
        PathBuf::from(p)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        ctx.cwd.join(expanded)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;
    use crate::hands::Ledger;

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(super::super::Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
        }
    }

    #[tokio::test]
    async fn reads_numbered_lines_with_range() {
        let dir = std::env::temp_dir().join(format!("ka-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand
            .execute(&json!({"path": "f.txt", "offset": 2, "limit": 2}), &ctx)
            .await;
        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "2\tl2\n3\tl3\n[...2 of 5 lines shown; lines 4-5 remain]\n"
        );
        assert!(
            ctx.ledger.lock().verify(&dir.join("f.txt")).is_ok(),
            "read must mint ledger"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let ctx = ctx_for(std::path::Path::new("/tmp"));
        let out = ReadHand
            .execute(&json!({"path": "definitely-missing-ka"}), &ctx)
            .await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn lists_directories() {
        let dir = std::env::temp_dir().join(format!("ka-read-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand.execute(&json!({"path": "."}), &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("a.txt"));
        assert!(out.content.contains("sub/"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

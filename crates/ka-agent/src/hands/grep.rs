//! The grep hand: Rust-regex content search over gitignore-aware walks,
//! with instructive errors for unsupported constructs.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};

/// Maximum matching lines returned per file.
pub const PER_FILE_CAP: usize = 20;
/// Maximum matching lines returned in total.
pub const TOTAL_CAP: usize = 200;
/// Per-line character cap.
pub const LINE_CAP: usize = 512;
/// Maximum files scanned.
pub const FILE_CAP: usize = 2_000;

/// The grep tool.
pub struct GrepHand;

impl Hand for GrepHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "grep",
            description: "Search file contents with a regular expression (Rust regex syntax: \
                no lookaround/backreferences). Returns `path:line:text`, capped. Supports an \
                optional glob filter on file names."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex to search for" },
                    "path": { "type": "string", "description": "Root directory or file (default: cwd)" },
                    "glob": { "type": "string", "description": "Only search files matching this glob (e.g. *.rs)" }
                },
                "required": ["pattern"]
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
            let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
                return ToolOutput::err("grep: missing required 'pattern'");
            };
            let re = match regex::Regex::new(pattern) {
                Ok(r) => r,
                Err(e) => {
                    return ToolOutput::err(format!(
                        "grep: invalid regex: {e}\nhint: lookarounds ((?=..)) and backreferences (\\1) are not supported; restructure the pattern"
                    ));
                }
            };
            let root = args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| super::read::resolve(ctx, p))
                .unwrap_or_else(|| ctx.cwd.clone());
            let glob_filter = args.get("glob").and_then(Value::as_str).and_then(|g| {
                // reuse the glob translator by matching on the file NAME
                name_regex(g)
            });

            let mut out = String::new();
            let mut total = 0;
            let mut files = 0;
            let mut truncated = false;

            if root.is_file() {
                scan_file(&root, &root, &re, &mut out, &mut total);
            } else {
                let walker = ignore::WalkBuilder::new(&root)
                    .hidden(true)
                    .git_ignore(true)
                    .git_global(true)
                    .build();
                for entry in walker.flatten() {
                    if total >= TOTAL_CAP || files >= FILE_CAP {
                        truncated = true;
                        break;
                    }
                    let path = entry.path();
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        continue;
                    }
                    if let Some(filter) = &glob_filter {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if !filter.is_match(&name) {
                            continue;
                        }
                    }
                    files += 1;
                    scan_file(path, &root, &re, &mut out, &mut total);
                }
            }

            if out.is_empty() {
                return ToolOutput::ok("no matches");
            }
            if total >= TOTAL_CAP || truncated {
                out.push_str("[...results capped]\n");
            }
            ToolOutput::ok(out)
        })
    }
}

fn scan_file(
    path: &std::path::Path,
    root: &std::path::Path,
    re: &regex::Regex,
    out: &mut String,
    total: &mut usize,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return; // binary or unreadable: skip silently
    };
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut per_file = 0;
    for (i, line) in text.lines().enumerate() {
        if re.is_match(line) {
            let capped: String = line.chars().take(LINE_CAP).collect();
            out.push_str(&format!("{}:{}:{}\n", rel.display(), i + 1, capped));
            *total += 1;
            per_file += 1;
            if per_file >= PER_FILE_CAP {
                out.push_str(&format!("{}:[...per-file cap]\n", rel.display()));
                break;
            }
            if *total >= TOTAL_CAP {
                break;
            }
        }
    }
}

fn name_regex(glob: &str) -> Option<regex::Regex> {
    let mut re = String::from("(?s)^");
    for c in glob.chars() {
        match c {
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    regex::Regex::new(&re).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;
    use crate::hands::{Ledger, Spill};

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
        }
    }

    #[tokio::test]
    async fn finds_matches_with_locations() {
        let dir = std::env::temp_dir().join(format!("ka-grep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        std::fs::write(dir.join("src/b.txt"), "nothing here\n").unwrap();

        let ctx = ctx_for(&dir);
        let out = GrepHand
            .execute(&json!({"pattern": "fn \\w+\\(\\)"}), &ctx)
            .await;
        assert!(
            out.content.contains("src/a.rs:1:fn one() {}"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("src/a.rs:2:fn two() {}"),
            "{}",
            out.content
        );
        assert_eq!(out.content.matches("b.txt").count(), 0);

        let out = GrepHand
            .execute(&json!({"pattern": "fn", "glob": "*.txt"}), &ctx)
            .await;
        assert_eq!(out.content.trim(), "no matches");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn instructive_error_on_lookaround() {
        let ctx = ctx_for(std::path::Path::new("/tmp"));
        let out = GrepHand.execute(&json!({"pattern": "(?=foo)"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("not supported"), "{}", out.content);
    }
}

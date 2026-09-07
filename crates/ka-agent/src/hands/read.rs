//! The read hand: line-ranged file reads with caps and ledger minting.
//! Image files (png/jpg/webp/gif, detected by magic bytes) come back as
//! a placeholder plus an `ImagePart` for vision models.
use std::future::Future;
use std::io::Read;
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
            name: "read".to_string(),
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

            // image files: sniff by magic bytes, cap size, return a
            // placeholder plus the base64 payload for the vision wire
            if let Some(media_type) = sniff_image_type(&path) {
                let cap_mb = ctx.max_image_mb;
                if cap_mb > 0 && meta.len() > cap_mb as u64 * 1024 * 1024 {
                    return ToolOutput::err(format!(
                        "read {}: image is {:.1} MB, over the {} MB cap \
                         ([tools.read] max_image_mb)",
                        path.display(),
                        meta.len() as f64 / (1024.0 * 1024.0),
                        cap_mb
                    ));
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => return ToolOutput::err(format!("read {}: {e}", path.display())),
                };
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
                let kb = bytes.len().div_ceil(1024);
                return ToolOutput {
                    content: format!("[image {name} {kb}KB]\n"),
                    is_error: false,
                    spill: None,
                    images: vec![ka_protocol::ImagePart {
                        data: b64_encode(&bytes),
                        media_type: media_type.to_string(),
                    }],
                };
            }

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

/// Detect an image file by its leading magic bytes. Returns the IANA
/// media type.
fn sniff_image_type(path: &std::path::Path) -> Option<&'static str> {
    let mut head = [0u8; 12];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let h = &head[..n];
    if h.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if h.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if h.starts_with(b"GIF87a") || h.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if h.len() >= 12 && h.starts_with(b"RIFF") && &h[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Minimal standard base64 encoder (keeps ka dependency-free).
fn b64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
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
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
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

    #[tokio::test]
    async fn read_returns_image_part_for_png() {
        let dir = std::env::temp_dir().join(format!("ka-read-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 1x1 transparent PNG
        let png: &[u8] = &[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        std::fs::write(dir.join("pic.png"), png).unwrap();
        let ctx = ctx_for(&dir);
        let out = ReadHand
            .execute(&serde_json::json!({"path": "pic.png"}), &ctx)
            .await;
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!(out.images[0].data, b64_encode(png));
        assert!(out.content.contains("[image pic.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_image_respects_max_image_mb() {
        let dir = std::env::temp_dir().join(format!("ka-read-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jpeg: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        std::fs::write(dir.join("big.jpg"), jpeg).unwrap();
        let mut ctx = ctx_for(&dir);
        ctx.max_image_mb = 0; // 0 = unlimited
        let out = ReadHand
            .execute(&serde_json::json!({"path": "big.jpg"}), &ctx)
            .await;
        assert!(!out.is_error);
        assert_eq!(out.images[0].media_type, "image/jpeg");

        // 1.1 MB file against a 1 MB cap
        let mut big: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        big.resize(1100 * 1024, 0);
        std::fs::write(dir.join("big.jpg"), &big).unwrap();
        let mut ctx = ctx_for(&dir);
        ctx.max_image_mb = 1;
        let out = ReadHand
            .execute(&serde_json::json!({"path": "big.jpg"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("max_image_mb"), "{}", out.content);
        assert!(out.images.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn b64_encode_matches_known_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn sniff_rejects_text_files() {
        let dir = std::env::temp_dir().join(format!("ka-read-sniff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plain.txt");
        std::fs::write(&p, "just text\n").unwrap();
        assert_eq!(sniff_image_type(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

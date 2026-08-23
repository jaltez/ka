//! Strand store: one session = one append-only JSONL file ("strand"), a tree
//! of [`Record`]s with a single live tip. Branching ("offshoot") is a tip
//! move; splitting copies a prefix into a new file. This module freezes the
//! record taxonomy in Phase 0; persistence wiring lands in Phase 3.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use ka_protocol::{Effort, Mode, RecordId, StrandId};
use serde::{Deserialize, Serialize};

/// Git snapshot carried in the strand header (read-only awareness).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// Current branch name (`HEAD` when detached).
    pub branch: String,
    /// Dirty (modified/untracked) file paths, capped in count.
    pub dirty: Vec<String>,
}

/// Who authored a message record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The human user.
    User,
    /// The assistant/model.
    Assistant,
    /// Tool output rendered as a message.
    Tool,
    /// System-level text.
    System,
}

/// One line of a strand file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    /// First line of every strand.
    Header {
        /// Strand identifier.
        id: StrandId,
        /// Creation timestamp (RFC 3339).
        ts: String,
        /// Working directory at creation.
        cwd: String,
        /// Strand format version.
        version: u32,
        /// Repo state at creation, if inside a work tree.
        repo: Option<RepoSnapshot>,
    },
    /// A conversation message.
    Message {
        /// Record identifier.
        id: RecordId,
        /// Authoring role.
        role: Role,
        /// Message content.
        content: String,
    },
    /// A settings change (model / effort / mode).
    Change {
        /// Record identifier.
        id: RecordId,
        /// New model selector, if changed.
        model: Option<String>,
        /// New effort, if changed.
        effort: Option<Effort>,
        /// New permission mode, if changed.
        mode: Option<Mode>,
    },
    /// A digest (compaction) boundary.
    Digest {
        /// Record identifier.
        id: RecordId,
        /// Summary replacing the collapsed history.
        summary: String,
        /// Kept history starts at this record.
        kept_from: RecordId,
    },
    /// A clear/reset boundary marker.
    Boundary {
        /// Record identifier.
        id: RecordId,
    },
    /// Extension-owned opaque record (namespaced; engine reserves `x.ka.*`).
    Custom {
        /// Record identifier.
        id: RecordId,
        /// Reverse-domain-ish namespace.
        ns: String,
        /// Opaque payload.
        data: serde_json::Value,
    },
}

impl Record {
    /// The record's identifier, regardless of kind.
    pub fn id(&self) -> Option<&RecordId> {
        match self {
            Record::Header { .. } => None,
            Record::Message { id, .. }
            | Record::Change { id, .. }
            | Record::Digest { id, .. }
            | Record::Boundary { id, .. }
            | Record::Custom { id, .. } => Some(id),
        }
    }
}

/// Append-only writer for a strand file.
pub struct StrandWriter {
    file: File,
}

impl StrandWriter {
    /// Open (or create) a strand file for appending.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// Append one record and flush. Appends are the only writes strands get.
    pub fn append(&mut self, record: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::other(format!("record serialize: {e}")))?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()
    }
}

/// Read every record from a strand file. Malformed lines fail with the
/// offending line number; a missing file is an empty vec (fresh strand).
pub fn read(path: &Path) -> std::io::Result<Vec<Record>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(&line).map_err(|e| {
            std::io::Error::other(format!(
                "{}:{}: malformed record: {e}",
                path.display(),
                idx + 1
            ))
        })?;
        out.push(record);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;

    use super::*;

    fn temp_strand(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-strand-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("strand.jsonl")
    }

    #[test]
    fn roundtrip_through_file() {
        let path = temp_strand("roundtrip");
        let records = vec![
            Record::Header {
                id: StrandId("s1".into()),
                ts: "2026-08-23T00:00:00Z".into(),
                cwd: "/tmp/proj".into(),
                version: 1,
                repo: Some(RepoSnapshot {
                    branch: "main".into(),
                    dirty: vec!["src/x.rs".into()],
                }),
            },
            Record::Message {
                id: RecordId("r1".into()),
                role: Role::User,
                content: "hi".into(),
            },
            Record::Message {
                id: RecordId("r2".into()),
                role: Role::Assistant,
                content: "hello".into(),
            },
            Record::Change {
                id: RecordId("r3".into()),
                model: Some("openai/gpt-5.1:high".into()),
                effort: Some(Effort::High),
                mode: Some(Mode::Free),
            },
            Record::Digest {
                id: RecordId("r4".into()),
                summary: "so far: greetings".into(),
                kept_from: RecordId("r3".into()),
            },
            Record::Boundary {
                id: RecordId("r5".into()),
            },
            Record::Custom {
                id: RecordId("r6".into()),
                ns: "x.example.state".into(),
                data: serde_json::json!({"n": 1}),
            },
        ];
        {
            let mut w = StrandWriter::open(&path).unwrap();
            for r in &records {
                w.append(r).unwrap();
            }
        }
        let back = read(&path).unwrap();
        assert_eq!(records, back);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty() {
        let path = temp_strand("missing").join("nope.jsonl");
        assert!(read(&path).unwrap().is_empty());
    }

    #[test]
    fn malformed_line_reports_number() {
        let path = temp_strand("malformed");
        let valid_header = r#"{"record":"header","id":"s1","ts":"t","cwd":"/","version":1}"#;
        std::fs::write(&path, format!("{valid_header}\ngarbage\n")).unwrap();
        let err = read(&path).unwrap_err().to_string();
        assert!(err.contains(":2:"), "got: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_id_accessor() {
        let r = Record::Boundary {
            id: RecordId("b".into()),
        };
        assert_eq!(r.id(), Some(&RecordId("b".into())));
        let h = Record::Header {
            id: StrandId("s".into()),
            ts: "t".into(),
            cwd: "/".into(),
            version: 1,
            repo: None,
        };
        assert_eq!(h.id(), None);
    }
}

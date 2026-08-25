//! Strand store: one session = one append-only JSONL file ("strand"), a tree
//! of [`Record`]s with a single live tip. Branching ("offshoot") is a tip
//! move; splitting copies a prefix into a new file. Phase 3 adds the
//! session-file management used by the engine: ids, settings replay,
//! listing, and dangling-turn synthesis.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// A tool call persisted with an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCall {
    /// Provider call id.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Parsed arguments.
    pub arguments: serde_json::Value,
}

/// A tool result persisted with a tool-role message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredResult {
    /// The call this answers.
    pub call_id: String,
    /// Output text.
    pub content: String,
    /// Whether the tool reported an error.
    pub is_error: bool,
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
    /// A conversation message (with tool calls/results when present).
    Message {
        /// Record identifier.
        id: RecordId,
        /// Authoring role.
        role: Role,
        /// Message content.
        content: String,
        /// Tool calls issued with this assistant message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<StoredCall>,
        /// Tool results carried by a tool-role message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        results: Vec<StoredResult>,
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
    /// Conversation rewind: history is truncated after `kept_from`.
    Rewind {
        /// Record identifier.
        id: RecordId,
        /// Last kept message record; everything after it is discarded.
        kept_from: RecordId,
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
            | Record::Rewind { id, .. }
            | Record::Custom { id, .. } => Some(id),
        }
    }
}

static RECORD_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh, roughly-sortable record id.
pub fn new_record_id() -> RecordId {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = RECORD_COUNTER.fetch_add(1, Ordering::Relaxed);
    RecordId(format!("r{millis:x}-{n:x}"))
}

/// Generate a fresh strand id: `s{millis:x}-{seq:02x}{rand:010x}` — the
/// millis head keeps listing order stable across seconds, the per-process
/// sequence orders bursts inside one millisecond, and the 40 random bits
/// make ids referenceable by prefix (`ka --session 806890f4`).
pub fn new_strand_id() -> StrandId {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    // RandomState seeds differ per process (and per call); mixing in the
    // counter keeps same-process ids distinct too.
    use std::hash::{BuildHasher, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(millis.rotate_left(32) ^ n);
    let rand40 = hasher.finish() & 0xFF_FFFF_FFFF;
    StrandId(format!("s{millis:x}-{n:02x}{rand40:010x}"))
}

thread_local! {
    static DATA_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Override the data root for this thread (engine tests).
#[doc(hidden)]
pub fn set_data_dir_for_tests(dir: PathBuf) {
    DATA_DIR_OVERRIDE.with(|d| *d.borrow_mut() = Some(dir));
}

/// Data root: `KA_DATA_DIR` (tests) > `XDG_DATA_HOME/ka` > `~/.local/share/ka`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = DATA_DIR_OVERRIDE.with(|d| d.borrow().clone()) {
        return dir;
    }
    if let Ok(dir) = std::env::var("KA_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("ka")
}

fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy().replace('/', "-");
    format!("-{s}")
}

/// The strand directory for a working directory.
pub fn strand_dir(cwd: &Path) -> PathBuf {
    data_dir().join("strands").join(encode_cwd(cwd))
}

/// A strand under management: path (None until first append for fresh
/// strands — opening the TUI no longer litters empty session files),
/// records, live settings.
#[derive(Debug)]
pub struct StrandFile {
    path: Option<PathBuf>,
    /// Working directory, kept for lazy materialization.
    cwd: PathBuf,
    records: Vec<Record>,
    settings: Settings,
}

/// Replayed session settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// Last-set model selector.
    pub model: Option<String>,
    /// Last-set effort.
    pub effort: Option<Effort>,
    /// Last-set permission mode.
    pub mode: Option<Mode>,
    /// True when the file ended on a dangling (user-tail) turn.
    pub dangling: bool,
}

impl StrandFile {
    /// Create a fresh strand for `cwd`. The file is materialized on the
    /// first append — a session that never gets a message leaves nothing
    /// on disk.
    pub fn create(cwd: &Path, repo: Option<RepoSnapshot>) -> std::io::Result<Self> {
        let id = new_strand_id();
        let header = Record::Header {
            id: id.clone(),
            ts: now_rfc3339(),
            cwd: cwd.to_string_lossy().into_owned(),
            version: 1,
            repo,
        };
        Ok(Self {
            path: None,
            cwd: cwd.to_path_buf(),
            records: vec![header],
            settings: Settings::default(),
        })
    }

    /// Materialize the strand file on first use: directory + header.
    fn materialize(&mut self) -> std::io::Result<PathBuf> {
        if let Some(path) = &self.path {
            return Ok(path.clone());
        }
        let dir = strand_dir(&self.cwd);
        std::fs::create_dir_all(&dir)?;
        let header = self
            .records
            .first()
            .and_then(|r| match r {
                Record::Header { id, ts, .. } => {
                    Some(format!("{}_{}.jsonl", ts.replace(':', ""), id.0))
                }
                _ => None,
            })
            .ok_or_else(|| std::io::Error::other("strand missing header"))?;
        let path = dir.join(header);
        let mut writer = StrandWriter::open(&path)?;
        let first = self
            .records
            .first()
            .ok_or_else(|| std::io::Error::other("strand missing header"))?;
        writer.append(first)?;
        self.path = Some(path.clone());
        Ok(path)
    }

    /// Open an existing strand, replaying settings and detecting dangling
    /// turns. Missing file is an error (use listing to pick paths).
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let records = read(path)?;
        if records.is_empty() || !matches!(records[0], Record::Header { .. }) {
            return Err(std::io::Error::other(
                "not a strand file (missing header record)",
            ));
        }
        let mut settings = Settings::default();
        for record in &records {
            if let Record::Change {
                model,
                effort,
                mode,
                ..
            } = record
            {
                if model.is_some() {
                    settings.model = model.clone();
                }
                if effort.is_some() {
                    settings.effort = *effort;
                }
                if mode.is_some() {
                    settings.mode = *mode;
                }
            }
        }
        settings.dangling = matches!(
            records.last(),
            Some(Record::Message {
                role: Role::User,
                ..
            })
        );
        Ok(Self {
            path: Some(path.to_path_buf()),
            cwd: path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
            records,
            settings,
        })
    }

    /// Synthesize an aborted marker for a dangling turn, if present.
    pub fn synthesize_aborted(&mut self) -> std::io::Result<bool> {
        if !self.settings.dangling {
            return Ok(false);
        }
        let record = Record::Message {
            id: new_record_id(),
            role: Role::Assistant,
            content: "(turn interrupted)".to_string(),
            calls: Vec::new(),
            results: Vec::new(),
        };
        self.append(record)?;
        self.settings.dangling = false;
        Ok(true)
    }

    /// Append a record (writes through to disk).
    pub fn append(&mut self, record: Record) -> std::io::Result<()> {
        if let Record::Change {
            model,
            effort,
            mode,
            ..
        } = &record
        {
            if model.is_some() {
                self.settings.model = model.clone();
            }
            if effort.is_some() {
                self.settings.effort = *effort;
            }
            if mode.is_some() {
                self.settings.mode = *mode;
            }
        }
        if matches!(
            record,
            Record::Message {
                role: Role::User,
                ..
            }
        ) {
            self.settings.dangling = true;
        } else if matches!(record, Record::Message { .. }) {
            self.settings.dangling = false;
        }
        let path = self.materialize()?;
        let mut writer = StrandWriter::open(&path)?;
        writer.append(&record)?;
        self.records.push(record);
        Ok(())
    }

    /// The strand's file path (None until the first append).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// All records in order.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Replayed settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Take the settings (for engine bootstrap).
    pub fn into_settings(self) -> Settings {
        self.settings
    }

    /// Records appended after `since` (index into records).
    pub fn records_since(&self, since: usize) -> &[Record] {
        self.records.get(since..).unwrap_or(&[])
    }

    /// Current record count.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the strand holds only its header.
    pub fn is_empty(&self) -> bool {
        self.records.len() <= 1
    }
}

/// One line of the writer. Kept separate so raw appending stays cheap.
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

/// Summary of a strand for pickers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrandSummary {
    /// File path.
    pub path: PathBuf,
    /// Strand id.
    pub id: String,
    /// Creation timestamp.
    pub ts: String,
    /// First user message (truncated) or "(empty)".
    pub title: String,
    /// Message count.
    pub messages: usize,
}

/// List strands for a working directory, newest first. Scans cheaply:
/// full parse for the header line only, a `"record":"message"` prefix
/// test for counting, and one partial probe for the title — no
/// per-record deserialization of calls/results.
pub fn list(cwd: &Path) -> std::io::Result<Vec<StrandSummary>> {
    /// Just the fields a summary needs off a message line.
    #[derive(serde::Deserialize)]
    struct TitleProbe {
        role: Option<Role>,
        content: Option<String>,
    }

    let dir = strand_dir(cwd);
    let mut summaries = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(summaries),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut lines = BufReader::new(file).lines();
        let Some(Ok(first)) = lines.next() else {
            continue;
        };
        let Ok(Record::Header { id, ts, .. }) = serde_json::from_str(&first) else {
            continue;
        };
        let mut title = String::new();
        let mut messages = 0usize;
        for line in lines.map_while(Result::ok) {
            if line.starts_with("{\"record\":\"message\"") {
                messages += 1;
                if title.is_empty() {
                    if let Ok(probe) = serde_json::from_str::<TitleProbe>(&line) {
                        if probe.role == Some(Role::User) {
                            if let Some(content) = probe.content {
                                let first_line = content
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .chars()
                                    .take(60)
                                    .collect::<String>();
                                if !first_line.is_empty() {
                                    title = first_line;
                                }
                            }
                        }
                    }
                }
            }
        }
        if title.is_empty() {
            title = "(empty)".to_string();
        }
        summaries.push(StrandSummary {
            path,
            id: id.0.clone(),
            ts: ts.clone(),
            title,
            messages,
        });
    }
    // ids embed millisecond timestamps (s{millis:x}), so they sort newer
    // first even when RFC 3339 timestamps tie at second granularity.
    summaries.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(summaries)
}

/// Most recent strand for a working directory.
pub fn latest(cwd: &Path) -> std::io::Result<Option<StrandSummary>> {
    Ok(list(cwd)?.into_iter().next())
}

/// Outcome of resolving a user-supplied session reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdMatch {
    /// No strand matched.
    None,
    /// Exactly one strand matched the prefix.
    Unique(StrandSummary),
    /// Several matched; the user must be more specific.
    Ambiguous(Vec<StrandSummary>),
}

/// Resolve a session reference: full id, id prefix, or an existing file
/// path. Ids embed their creation millis, so any prefix of the random
/// tail (or the whole id) works.
pub fn resolve_id(cwd: &Path, needle: &str) -> std::io::Result<IdMatch> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(IdMatch::None);
    }
    let path = Path::new(needle);
    if path.exists() {
        return Ok(IdMatch::Unique(StrandSummary {
            path: path.to_path_buf(),
            id: needle.to_string(),
            ts: String::new(),
            title: String::new(),
            messages: 0,
        }));
    }
    let mut matches: Vec<StrandSummary> = strands_filter(cwd, needle)?;
    match matches.len() {
        0 => Ok(IdMatch::None),
        1 => Ok(IdMatch::Unique(matches.swap_remove(0))),
        _ => Ok(IdMatch::Ambiguous(matches)),
    }
}

fn strands_filter(cwd: &Path, needle: &str) -> std::io::Result<Vec<StrandSummary>> {
    Ok(list(cwd)?
        .into_iter()
        .filter(|s| {
            s.id.starts_with(needle)
                || s.id
                    .split_once('-')
                    .is_some_and(|(_, tail)| tail.starts_with(needle))
        })
        .collect())
}

impl IdMatch {
    /// The matched summary (None/Ambiguous → None).
    pub fn into_summary(self) -> Option<StrandSummary> {
        match self {
            IdMatch::Unique(s) => Some(s),
            _ => None,
        }
    }
}

fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // days→date civil algorithm (Howard Hinnant) compressed
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
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

    use super::*;

    fn temp_data(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-strand3-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        DATA_DIR_OVERRIDE.with(|d| *d.borrow_mut() = Some(dir.clone()));
        dir
    }

    #[test]
    fn strandfile_roundtrip_with_tools_and_settings() {
        let data = temp_data("roundtrip");
        let cwd = PathBuf::from("/tmp/proj");
        let mut strand = StrandFile::create(
            &cwd,
            Some(RepoSnapshot {
                branch: "main".into(),
                dirty: vec!["x".into()],
            }),
        )
        .unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "go".into(),
                calls: Vec::new(),
                results: Vec::new(),
            })
            .unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::Assistant,
                content: "working".into(),
                calls: vec![StoredCall {
                    id: "c1".into(),
                    tool: "read".into(),
                    arguments: serde_json::json!({"path": "x"}),
                }],
                results: Vec::new(),
            })
            .unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::Tool,
                content: String::new(),
                calls: Vec::new(),
                results: vec![StoredResult {
                    call_id: "c1".into(),
                    content: "file body".into(),
                    is_error: false,
                }],
            })
            .unwrap();
        strand
            .append(Record::Change {
                id: new_record_id(),
                model: Some("openai/gpt-5.1@high".into()),
                effort: Some(Effort::High),
                mode: Some(Mode::Free),
            })
            .unwrap();

        let reopened = StrandFile::open(strand.path().unwrap()).unwrap();
        assert_eq!(reopened.records().len(), 5);
        let settings = reopened.settings();
        assert_eq!(settings.model.as_deref(), Some("openai/gpt-5.1@high"));
        assert_eq!(settings.mode, Some(Mode::Free));
        assert!(!settings.dangling);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn dangling_turn_detected_and_synthesized() {
        let data = temp_data("dangling");
        let cwd = PathBuf::from("/tmp/proj2");
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "hello?".into(),
                calls: Vec::new(),
                results: Vec::new(),
            })
            .unwrap();
        assert!(
            StrandFile::open(strand.path().unwrap())
                .unwrap()
                .settings()
                .dangling
        );

        let mut resumed = StrandFile::open(strand.path().unwrap()).unwrap();
        assert!(resumed.synthesize_aborted().unwrap());
        let clean = StrandFile::open(strand.path().unwrap()).unwrap();
        assert!(!clean.settings().dangling);
        match clean.records().last() {
            Some(Record::Message { content, .. }) => assert_eq!(content, "(turn interrupted)"),
            other => panic!("expected synthesized message, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn listing_newest_first_with_titles() {
        let data = temp_data("listing");
        let cwd = PathBuf::from("/tmp/proj3");
        for prompt in ["first session query", "second session query"] {
            let mut strand = StrandFile::create(&cwd, None).unwrap();
            strand
                .append(Record::Message {
                    id: new_record_id(),
                    role: Role::User,
                    content: prompt.into(),
                    calls: Vec::new(),
                    results: Vec::new(),
                })
                .unwrap();
        }
        let listed = list(&cwd).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].title.contains("second"), "{}", listed[0].title);
        assert!(listed[0].messages >= 1);
        assert!(latest(&cwd).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn fresh_strand_materializes_on_first_append() {
        let data = temp_data("lazy");
        let cwd = PathBuf::from("/tmp/proj-lazy");
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        assert!(strand.path().is_none(), "no file before the first message");
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "first message".into(),
                calls: Vec::new(),
                results: Vec::new(),
            })
            .unwrap();
        let path = strand.path().expect("materialized").to_path_buf();
        assert!(path.exists());
        let records = read(&path).unwrap();
        assert!(matches!(records[0], Record::Header { .. }));
        assert_eq!(records.len(), 2, "header + first message");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn listing_survives_a_corrupted_tail() {
        let data = temp_data("corrupt");
        let cwd = PathBuf::from("/tmp/proj-corrupt");
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "survives corruption".into(),
                calls: Vec::new(),
                results: Vec::new(),
            })
            .unwrap();
        let path = strand.path().unwrap().to_path_buf();
        // append garbage mid-file (simulates a torn write)
        let mut writer = StrandWriter::open(&path).unwrap();
        writer
            .append(&Record::Message {
                id: new_record_id(),
                role: Role::Assistant,
                content: "fine".into(),
                calls: Vec::new(),
                results: Vec::new(),
            })
            .unwrap();
        drop(writer);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{not json at all\n").unwrap();
        }
        let listed = list(&cwd).unwrap();
        assert_eq!(listed.len(), 1, "corrupted tail must not hide the strand");
        assert!(listed[0].title.contains("survives corruption"));
        assert!(listed[0].messages >= 2);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn strand_ids_are_long_hashed_and_distinct() {
        let a = new_strand_id().0;
        let b = new_strand_id().0;
        assert_ne!(a, b);
        assert!(a.starts_with('s'));
        // s{millis:x}-{12 hex}
        let (head, tail) = a.split_once('-').unwrap();
        assert!(head.starts_with('s') && head.len() > 1, "{a}");
        assert_eq!(tail.len(), 12, "random tail is 12 hex chars: {a}");
        assert!(tail.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_id_matches_full_prefix_and_tail() {
        let data = temp_data("resolve");
        let cwd = PathBuf::from("/tmp/proj-resolve");
        let mut strands = Vec::new();
        for prompt in ["alpha session", "beta session"] {
            let mut s = StrandFile::create(&cwd, None).unwrap();
            s.append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: prompt.into(),
                calls: Vec::new(),
                results: Vec::new(),
            })
            .unwrap();
            strands.push(s);
        }
        let listed = list(&cwd).unwrap();
        let target = &listed[1]; // oldest = "alpha"
        // full id
        match resolve_id(&cwd, &target.id).unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, target.id),
            other => panic!("full id must resolve uniquely: {other:?}"),
        }
        // leading prefix (millis heads collide for same-ms sessions, so
        // include the tail start to pin one session)
        let head: String = target.id.chars().take(6).collect();
        match resolve_id(&cwd, &head).unwrap() {
            IdMatch::Ambiguous(cands) => assert_eq!(cands.len(), 2),
            other => panic!("same-millis head should be ambiguous: {other:?}"),
        }
        let (head_part, tail_part) = target.id.split_once('-').unwrap();
        let pinned = format!("{head_part}-{tail_part}");
        match resolve_id(&cwd, &pinned).unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, target.id),
            other => panic!("full id must resolve: {other:?}"),
        }
        // random-tail prefix
        let tail = target.id.split_once('-').unwrap().1;
        let tail4: String = tail.chars().take(4).collect();
        match resolve_id(&cwd, &tail4).unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, target.id),
            other => panic!("tail prefix must resolve: {other:?}"),
        }
        // garbage
        assert!(matches!(
            resolve_id(&cwd, "zz-nothing").unwrap(),
            IdMatch::None
        ));
        // empty
        assert!(matches!(resolve_id(&cwd, "  ").unwrap(), IdMatch::None));
        drop(strands);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn ids_are_fresh_and_sortableish() {
        let a = new_record_id();
        let b = new_record_id();
        assert_ne!(a, b);
        assert!(a.0.starts_with('r'));
        assert!(new_strand_id().0.starts_with('s'));
    }
}

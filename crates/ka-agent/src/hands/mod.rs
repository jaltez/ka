//! Hands: ka's tools. Every Hand declares a clearance tier and annotations;
//! the engine gates execution through them and routes results through the
//! caps/spill hygiene pipe.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub mod bash;
pub mod delegate;
pub use bash::BashHand;
pub mod bashp;
pub mod edit;
pub mod git;
pub mod glob;
pub mod grep;
pub mod jobs;
pub mod pathfinder;
pub mod read;
pub mod secrets;
pub mod snapshots;
pub mod todo;
pub mod web;
pub mod write;

/// Execution clearance tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Clearance {
    /// Read-only tools: always allowed.
    Read,
    /// Mutating tools: confirmed by rule, mode, or user.
    Write,
    /// Arbitrary execution: the strongest gate.
    Exec,
}

/// What one tool invocation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text content returned to the model.
    pub content: String,
    /// Whether this is an error result.
    pub is_error: bool,
    /// Spill pointer if full output was parked on disk.
    pub spill: Option<String>,
    /// Images produced by the tool (read hand on an image file).
    pub images: Vec<ka_protocol::ImagePart>,
}

impl ToolOutput {
    /// A successful output.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            spill: None,
            images: Vec::new(),
        }
    }

    /// An error output (fed back to the model to self-correct).
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            spill: None,
            images: Vec::new(),
        }
    }
}

/// Static definition of a hand (model-facing contract).
#[derive(Debug, Clone)]
pub struct HandDef {
    /// Tool name as the model sees it.
    pub name: String,
    /// One-paragraph description for the model.
    pub description: String,
    /// JSON schema for the arguments object.
    pub parameters: Value,
    /// Clearance tier.
    pub clearance: Clearance,
    /// Whether this hand only reads (never mutates state).
    pub read_only: bool,
}

/// A tool. Async via boxed futures, dyn-safe like Speaker.
pub trait Hand: Send + Sync {
    /// The definition (built per hand; registries are constructed once).
    fn def(&self) -> HandDef;

    /// Clearance tier for THIS call. Defaults to the hand's static tier;
    /// override for argument-dependent tiers (jobs: kill = exec).
    fn clearance_for(&self, _args: &Value) -> Clearance {
        self.def().clearance
    }

    /// Execute with parsed arguments.
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;
}

/// Per-invocation context shared with hands. The ledger and spill store are
/// shared by reference so every hand sees the same read-tracking state.
#[derive(Clone)]
pub struct HandContext {
    /// Working directory for relative paths.
    pub cwd: PathBuf,
    /// Shared read ledger.
    pub ledger: std::sync::Arc<parking_lot::Mutex<Ledger>>,
    /// Shared spill store.
    pub spill: std::sync::Arc<Spill>,
    /// Shared pre-mutation snapshot journal (inert for readonly voices).
    pub snapshots: std::sync::Arc<parking_lot::Mutex<snapshots::Snapshots>>,
    /// Shared table of auto-backgrounded bash jobs (bash hand registers,
    /// the jobs hand lists/kills, the engine kills the rest at shutdown).
    pub jobs: std::sync::Arc<jobs::JobTable>,
    /// Bash auto-background threshold in ms (0 = never background).
    pub bash_background_ms: u64,
    /// Read-hand image size cap in MB (0 = unlimited).
    pub max_image_mb: u32,
    /// Web fetches may target private hosts ([tools.web]).
    pub web_allow_private: bool,
    /// Sandbox policy for bash children ([sandbox]).
    pub sandbox: ka_sandbox::Policy,
}

/// The read ledger: files the model has read, with their stamps. Edits
/// refuse files that are absent (read first) or changed since read.
#[derive(Debug, Default)]
pub struct Ledger {
    stamps: HashMap<PathBuf, FileStamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime: SystemTime,
    size: u64,
}

impl Ledger {
    /// Record a fresh read of `path`.
    pub fn mint(&mut self, path: &Path, meta: &std::fs::Metadata) {
        self.stamps.insert(
            path.to_path_buf(),
            FileStamp {
                mtime: meta.modified().unwrap_or(UNIX_EPOCH),
                size: meta.len(),
            },
        );
    }

    /// `Ok(())` when `path` was read and is unchanged since.
    pub fn verify(&self, path: &Path) -> Result<(), String> {
        let display = path.display();
        let Some(stamp) = self.stamps.get(path) else {
            return Err(format!(
                "{display} has not been read yet; read it before editing"
            ));
        };
        match std::fs::metadata(path) {
            Ok(meta) => {
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                if mtime != stamp.mtime || meta.len() != stamp.size {
                    Err(format!(
                        "{display} changed since it was read; re-read it first"
                    ))
                } else {
                    Ok(())
                }
            }
            Err(e) => Err(format!("{display}: {e}")),
        }
    }

    /// Drop all stamps (after arbitrary shell execution, any file may have
    /// changed — conservative and cheap).
    pub fn invalidate_all(&mut self) {
        self.stamps.clear();
    }

    /// Number of tracked files.
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    /// Whether nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }
}

/// Spill store: oversized tool outputs parked on disk, referenced as
/// `spill://<id>` from the capped excerpt returned to the model.
pub struct Spill {
    dir: PathBuf,
}

impl Default for Spill {
    fn default() -> Self {
        Self::new()
    }
}

impl Spill {
    /// Spill directory under the state root (created lazily).
    pub fn new() -> Self {
        let dir = std::env::var("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .map(|base| base.join("ka/spills"))
            .unwrap_or_else(|_| std::env::temp_dir().join("ka-spills"));
        Self { dir }
    }

    /// Create an empty spill slot for streamed (backgrounded) output;
    /// returns the file path and its `spill://<id>` pointer. The file is
    /// created eagerly so tail reads never race creation.
    pub fn slot(&self) -> std::io::Result<(PathBuf, String)> {
        std::fs::create_dir_all(&self.dir)?;
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!(
            "job-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            n
        );
        let path = self.dir.join(&id);
        std::fs::write(&path, b"")?;
        Ok((path, format!("spill://{id}")))
    }

    /// Park `content`, returning its `spill://<id>` pointer.
    pub fn park(&self, content: &str) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let id = format!(
            "{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            content.len()
        );
        std::fs::write(self.dir.join(&id), content)?;
        Ok(format!("spill://{id}"))
    }
}

/// The full registry wired for Phase 2: an internally-owned todo slot.
pub fn registry() -> Vec<std::sync::Arc<dyn Hand>> {
    registry_with_pathfinder(
        std::sync::Arc::new(parking_lot::RwLock::new(
            pathfinder::PathfinderSource::default(),
        )),
        todo::slot(),
        std::sync::Arc::new(jobs::JobTable::new()),
    )
}

/// Registry with externally-owned pathfinder bootstrap slot (engine) and
/// todo slot (voice — it forwards the list as `Event::Todos`); `jobs` is
/// the shared auto-backgrounded-job table the bash hand promotes into and
/// the jobs hand serves.
pub fn registry_with_pathfinder(
    slot: std::sync::Arc<parking_lot::RwLock<pathfinder::PathfinderSource>>,
    todos: todo::TodoSlot,
    jobs: std::sync::Arc<jobs::JobTable>,
) -> Vec<std::sync::Arc<dyn Hand>> {
    vec![
        std::sync::Arc::new(read::ReadHand),
        std::sync::Arc::new(edit::EditHand),
        std::sync::Arc::new(write::WriteHand),
        std::sync::Arc::new(bash::BashHand),
        std::sync::Arc::new(glob::GlobHand),
        std::sync::Arc::new(grep::GrepHand),
        std::sync::Arc::new(pathfinder::PathfinderHand::from_slot(slot)),
        std::sync::Arc::new(todo::TodoHand::new(todos)),
        std::sync::Arc::new(jobs::JobsHand::new(jobs)),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn ledger_roundtrip_and_staleness() {
        let dir = std::env::temp_dir().join(format!("ka-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let mut ledger = Ledger::default();
        assert!(ledger.verify(&file).is_err(), "untracked file must fail");

        let meta = std::fs::metadata(&file).unwrap();
        ledger.mint(&file, &meta);
        assert!(ledger.verify(&file).is_ok());

        std::fs::write(&file, "two — longer").unwrap();
        assert!(ledger.verify(&file).is_err(), "size change must be caught");

        let meta = std::fs::metadata(&file).unwrap();
        ledger.mint(&file, &meta);
        // same length, newer mtime
        std::fs::write(&file, "TWO — longer!").unwrap();
        assert!(ledger.verify(&file).is_err(), "mtime change must be caught");

        ledger.invalidate_all();
        assert!(ledger.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registries_carry_todo_and_grow_by_one() {
        let base = [
            "read",
            "edit",
            "write",
            "bash",
            "glob",
            "grep",
            "pathfinder",
        ];
        let with = registry_with_pathfinder(
            std::sync::Arc::new(parking_lot::RwLock::new(
                pathfinder::PathfinderSource::default(),
            )),
            todo::slot(),
            std::sync::Arc::new(jobs::JobTable::new()),
        );
        let names: Vec<String> = with.iter().map(|h| h.def().name).collect();
        assert_eq!(names.len(), base.len() + 2, "todo + jobs grow the registry");
        assert!(names.iter().any(|n| n == "todo"), "names: {names:?}");
        assert_eq!(registry().len(), with.len(), "both registries match");
        let todo = with.iter().find(|h| h.def().name == "todo").unwrap();
        let def = todo.def();
        assert_eq!(def.clearance, Clearance::Read, "todo must auto-allow");
        assert!(def.read_only);
    }

    #[test]
    fn spill_parks_and_points() {
        let spill = Spill::new();
        let ptr = spill.park("huge output").unwrap();
        assert!(ptr.starts_with("spill://"), "{ptr}");
        let id = ptr.trim_start_matches("spill://");
        let path = spill.dir.join(id);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "huge output");
        let _ = std::fs::remove_file(&path);
    }
}

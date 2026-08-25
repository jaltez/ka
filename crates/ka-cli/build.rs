use std::path::{Path, PathBuf};

fn main() {
    // stamp version + git hash into --version (release tarballs without
    // .git build fine; the hash is omitted)
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let root = workspace_root();
    let full = match git_hash(&root) {
        Some(hash) => format!("{pkg} ({hash})"),
        None => pkg,
    };
    println!("cargo:rustc-env=KA_VERSION={full}");
    println!(
        "cargo:rerun-if-changed={}",
        root.join(".git/HEAD").display()
    );
}

/// The workspace root: two levels above this crate's manifest dir.
fn workspace_root() -> PathBuf {
    Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../..")
}

fn git_hash(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git/HEAD")).ok()?;
    let referenced = head
        .strip_prefix("ref: ")
        .and_then(|r| std::fs::read_to_string(root.join(".git").join(r.trim())).ok())?;
    Some(referenced.trim().chars().take(12).collect())
}

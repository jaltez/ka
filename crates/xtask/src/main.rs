//! Repo automation for ka, following the cargo-xtask convention.
//! Usage: `cargo xtask <task>` — see `help` for the task list.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Binary-size contract from the design docs (MB).
const SIZE_BUDGET_MB: f64 = 10.0;

fn main() {
    let task = env::args().nth(1).unwrap_or_else(|| "help".to_string());
    let rest: Vec<String> = env::args().skip(2).collect();
    let code = match task.as_str() {
        "install" => install(),
        "link" => link(),
        "unlink" => unlink(),
        "dev" => dev(&rest),
        "ci" => ci(),
        "size" => size(),
        "help" | "--help" | "-h" => {
            print_help();
            0
        }
        other => {
            eprintln!("xtask: unknown task {other:?}\n");
            print_help();
            2
        }
    };
    std::process::exit(code);
}

fn print_help() {
    println!(
        "ka repo automation (cargo xtask <task>)

  install   install STABLE ka globally (cargo install --path, --locked)
  link      build release + symlink kad -> ./target/release/ka in ~/.cargo/bin (DEV binary)
  unlink    remove the kad symlink
  dev [...] rebuild release, then run the dev binary with any args
            (hint: KA_DATA_DIR=/tmp/ka-dev cargo xtask dev -- models)
  ci        fmt --check + clippy -D warnings + tests (what CI runs)
  size      release binary size vs the {SIZE_BUDGET_MB} MB contract
  help      this message"
    );
}

/// Repo root (parent of crates/xtask).
fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/xtask → two levels up
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn cargo_bin_dir() -> PathBuf {
    let home = env::var("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_else(|_| PathBuf::from(".cargo"));
    home.join("bin")
}

fn run(cmd: &mut Command) -> i32 {
    println!("$ {:?}", cmd);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("xtask: failed to run {:?}: {e}", cmd.get_program());
            1
        }
    }
}

/// `cargo install --path crates/ka-cli --locked` — the stable channel.
fn install() -> i32 {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(repo_root())
        .args(["install", "--path", "crates/ka-cli", "--locked"]);
    let code = run(&mut cmd);
    if code == 0 {
        println!("\nstable `ka` installed to {}", cargo_bin_dir().display());
    }
    code
}

/// Build release and symlink `kad` → repo's target/release/ka (dev channel).
fn link() -> i32 {
    let build = run(Command::new("cargo")
        .args(["build", "--release", "-p", "ka-cli"])
        .current_dir(repo_root()));
    if build != 0 {
        return build;
    }
    let target = repo_root().join("target/release/ka");
    if !target.exists() {
        eprintln!("xtask: {} missing after build", target.display());
        return 1;
    }
    let link = cargo_bin_dir().join("kad");
    if link.exists() || link.symlink_metadata().is_ok() {
        if let Err(e) = fs::remove_file(&link) {
            eprintln!("xtask: cannot remove old kad link: {e}");
            return 1;
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap_or_else(|e| {
        panic!(
            "xtask: symlink {} -> {}: {e}",
            link.display(),
            target.display()
        )
    });
    println!("dev `kad` -> {}", target.display());
    println!("tip: KA_DATA_DIR=/tmp/ka-dev kad ... isolates dev sessions");
    0
}

/// Remove the kad symlink.
fn unlink() -> i32 {
    let link = cargo_bin_dir().join("kad");
    if link.symlink_metadata().is_ok() {
        if let Err(e) = fs::remove_file(&link) {
            eprintln!("xtask: {e}");
            return 1;
        }
        println!("removed {}", link.display());
    } else {
        println!("nothing to remove at {}", link.display());
    }
    0
}

/// Rebuild and execute the dev binary with passthrough args.
fn dev(rest: &[String]) -> i32 {
    let build = run(Command::new("cargo")
        .args(["build", "--release", "-p", "ka-cli"])
        .current_dir(repo_root()));
    if build != 0 {
        return build;
    }
    let bin = repo_root().join("target/release/ka");
    let mut cmd = Command::new(&bin);
    cmd.args(rest);
    run(&mut cmd)
}

/// The full CI gate locally.
fn ci() -> i32 {
    let fmt = run(Command::new("cargo")
        .args(["fmt", "--all", "--check"])
        .current_dir(repo_root()));
    if fmt != 0 {
        return fmt;
    }
    let clippy = run(Command::new("cargo")
        .args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ])
        .current_dir(repo_root()));
    if clippy != 0 {
        return clippy;
    }
    run(Command::new("cargo")
        .args(["test", "--workspace"])
        .current_dir(repo_root()))
}

/// Release binary size vs the footprint contract.
fn size() -> i32 {
    let bin = repo_root().join("target/release/ka");
    if !bin.exists() {
        let build = run(Command::new("cargo")
            .args(["build", "--release", "-p", "ka-cli"])
            .current_dir(repo_root()));
        if build != 0 {
            return build;
        }
    }
    let bytes = fs::metadata(&bin).map(|m| m.len()).unwrap_or(0);
    let mb = bytes as f64 / 1024.0 / 1024.0;
    let verdict = if mb <= SIZE_BUDGET_MB {
        "OK"
    } else {
        "OVER BUDGET"
    };
    println!(
        "{:.2} MB / {:.0} MB contract — {verdict}",
        mb, SIZE_BUDGET_MB
    );
    if mb <= SIZE_BUDGET_MB { 0 } else { 1 }
}

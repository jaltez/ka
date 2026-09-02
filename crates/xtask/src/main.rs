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
        "models-sync" => models_sync(),
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
  models-sync  regenerate crates/ka-dialect/models-dev.toml from models.dev
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

/// Regenerate `crates/ka-dialect/models-dev.toml` from https://models.dev.
/// Keeps curated `dialects.toml` selectors untouched (their flags and effort
/// budgets win); adds every tool-capable model on a static OpenAI-compatible
/// or Anthropic-compatible endpoint, with real pricing when published.
fn models_sync() -> i32 {
    let url = "https://models.dev/api.json";
    eprintln!("models-sync: fetching {url}");
    let out = Command::new("curl")
        .args(["-sSL", "--max-time", "60", url])
        .output();
    let bytes = match out {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            eprintln!("models-sync: curl failed: {}", o.status);
            return 1;
        }
        Err(e) => {
            eprintln!("models-sync: curl not available: {e}");
            return 1;
        }
    };
    let root: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("models-sync: bad json: {e}");
            return 1;
        }
    };

    // curated selectors stay authoritative
    let dialects_path = repo_root().join("crates/ka-dialect/dialects.toml");
    let curated = fs::read_to_string(&dialects_path).unwrap_or_default();
    let mut curated_ids: Vec<String> = Vec::new();
    for line in curated.lines() {
        if let Some(rest) = line.strip_prefix("[dialects.\"") {
            if let Some(id) = rest.strip_suffix("\"]") {
                curated_ids.push(id.to_string());
            }
        }
    }

    let mut toml = String::new();
    toml.push_str("# ka dialect catalog — generated from https://models.dev (api.json).\n");
    toml.push_str("# Regenerate with: cargo xtask models-sync\n");
    toml.push_str("# Curated rows in dialects.toml win for the same selector; this file only\n");
    toml.push_str("# adds providers and models (real pricing where published, subscription\n");
    toml.push_str("# plans carry priced = false so costs are never fabricated).\n\n");

    let empty = serde_json::Map::new();
    let mut providers_written = 0usize;
    let mut rows_written = 0usize;
    let Some(providers) = root.as_object() else {
        eprintln!("models-sync: unexpected root shape");
        return 1;
    };
    let mut pids: Vec<&String> = providers.keys().collect();
    pids.sort();
    for pid in pids {
        let p = providers[pid].as_object().unwrap_or(&empty);
        let api = p.get("api").and_then(|v| v.as_str()).unwrap_or("");
        let env = p
            .get("env")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if api.is_empty() || env.is_empty() {
            continue; // needs dynamic config (vertex etc.) or keyless
        }
        let wire = if api.contains("/anthropic") {
            "anthropic_messages"
        } else {
            "openai_chat"
        };
        let Some(models) = p.get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        // marketplaces with hundreds of models drown the picker; keep
        // first-party vendors, subscription plans, and small specialists.
        // Giant aggregators stay reachable through custom selectors.
        const FIRST_PARTY: &[&str] = &[
            "openai",
            "anthropic",
            "deepseek",
            "groq",
            "mistral",
            "xai",
            "moonshot",
            "zhipuai",
            "zai",
            "deepinfra",
            "together",
            "fireworks",
            "cerebras",
        ];
        let is_plan = pid.contains("plan");
        let is_first_party = FIRST_PARTY.contains(&pid.as_str());
        let tool_models = models
            .values()
            .filter(|m| {
                m.get("tool_call")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .count();
        if !is_plan && !is_first_party && tool_models > 25 {
            continue;
        }
        let mut mids: Vec<&String> = models.keys().collect();
        mids.sort();
        let mut wrote_for_provider = false;
        for mid in mids {
            let m = models[mid].as_object().unwrap_or(&empty);
            if !m
                .get("tool_call")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            if !is_plan
                && !is_first_party
                && !m
                    .get("reasoning")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                continue;
            }
            let selector = format!("{pid}/{mid}");
            if curated_ids.contains(&selector) {
                continue;
            }
            let limit = m.get("limit").and_then(|v| v.as_object()).unwrap_or(&empty);
            let context = limit
                .get("context")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
            let max_output = limit
                .get("output")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
            let cost = m.get("cost").and_then(|v| v.as_object()).unwrap_or(&empty);
            let pin = cost.get("input").and_then(|v| v.as_f64());
            let pout = cost.get("output").and_then(|v| v.as_f64());
            // subscription plans publish 0/0 token costs — that is not
            // per-token pricing, keep them unpriced and badge them as plans
            let priced = pin.is_some()
                && pout.is_some()
                && (pin.unwrap_or(0.0) > 0.0 || pout.unwrap_or(0.0) > 0.0);
            toml.push_str(&format!("[dialects.\"{selector}\"]\n"));
            toml.push_str(&format!("wire = \"{wire}\"\n"));
            toml.push_str(&format!("base_url = \"{api}\"\n"));
            toml.push_str(&format!("api_key_env = \"{env}\"\n"));
            toml.push_str(&format!("context = {context}\n"));
            if max_output > 0 {
                toml.push_str(&format!("max_output = {max_output}\n"));
            }
            toml.push_str(&format!("priced = {priced}\n"));
            if priced {
                toml.push_str(&format!(
                    "[dialects.\"{selector}\".price]\ninput_per_mtok = {}\noutput_per_mtok = {}\n",
                    pin.unwrap_or_default(),
                    pout.unwrap_or_default()
                ));
            }
            toml.push('\n');
            rows_written += 1;
            wrote_for_provider = true;
        }
        if wrote_for_provider {
            providers_written += 1;
        }
    }

    let out_path = repo_root().join("crates/ka-dialect/models-dev.toml");
    if let Err(e) = fs::write(&out_path, &toml) {
        eprintln!("models-sync: write failed: {e}");
        return 1;
    }
    eprintln!(
        "models-sync: {} providers, {} models, {} bytes -> {}",
        providers_written,
        rows_written,
        toml.len(),
        out_path.display()
    );
    0
}

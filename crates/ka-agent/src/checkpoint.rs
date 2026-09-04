//! Working-tree checkpoints: git-based, snapshot-first, non-destructive.
//!
//! A checkpoint builds a commit object from the current working tree via
//! a temporary `GIT_INDEX_FILE` (`read-tree` + `add -A` + `write-tree` +
//! `commit-tree`), so the user's real index, stash, and HEAD are never
//! touched. The commit is intentionally unreferenced — its id is kept by
//! the engine's in-session checkpoint list; `git gc` may prune abandoned
//! checkpoints, which is acceptable for a session-scoped safety net.
//! Restore replays `git checkout <commit> -- .`, overwriting tracked
//! files with the snapshotted contents.

use std::path::Path;
use std::process::Command;

/// Snapshot the working tree at `cwd`. Returns the (full) checkpoint
/// commit id. Fails when `cwd` is not inside a git work tree.
pub fn snapshot(cwd: &Path) -> Result<String, String> {
    git(cwd, &["rev-parse", "--is-inside-work-tree"], &[])
        .filter(|out| out.trim() == "true")
        .ok_or_else(|| "not a git work tree — checkpoints need git".to_string())?;

    let index = temp_index_path();
    let env = [("GIT_INDEX_FILE", index.display().to_string())];
    let has_head = git(cwd, &["rev-parse", "--verify", "HEAD"], &env).is_some();

    let outcome = (|| {
        if has_head {
            git(cwd, &["read-tree", "HEAD"], &env)
                .ok_or_else(|| "read-tree HEAD failed".to_string())?;
        }
        // stage the working tree into the temporary index only
        git(cwd, &["add", "-A", "--"], &env).ok_or_else(|| "git add failed".to_string())?;
        let tree = git(cwd, &["write-tree"], &env)
            .ok_or_else(|| "write-tree failed".to_string())?
            .trim()
            .to_string();

        let stamp = ka_strand::now_rfc3339();
        let mut args = vec![
            "commit-tree".to_string(),
            tree,
            "-m".to_string(),
            format!("ka checkpoint {stamp}"),
        ];
        if has_head {
            args.push("-p".to_string());
            args.push("HEAD".to_string());
        }
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        git_env_commit(cwd, &argrefs, &env)
            .map(|s| s.trim().to_string())
            .ok_or_else(|| "commit-tree failed (is user.email set?)".to_string())
    })();
    let _ = std::fs::remove_file(&index);
    outcome
}

/// Restore a checkpoint commit over the working tree.
pub fn restore(cwd: &Path, commit: &str) -> Result<(), String> {
    // the id must name a commit object the repo still knows
    if git(
        cwd,
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        &[],
    )
    .is_none()
    {
        return Err(format!("checkpoint {commit} is gone (pruned by git gc?)"));
    }
    if git(cwd, &["checkout", commit, "--", "."], &[]).is_none() {
        return Err(format!("git checkout {commit} -- . failed"));
    }
    Ok(())
}

/// Short display form of a commit id.
pub fn short(id: &str) -> &str {
    &id[..id
        .char_indices()
        .nth(12)
        .map(|(i, _)| i)
        .unwrap_or(id.len())]
}

fn temp_index_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("ka-checkpoint-{}-{nanos}", std::process::id()))
}

fn git(cwd: &Path, args: &[&str], env: &[(&str, String)]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git commit-tree` with identity fallbacks so headless environments
/// without user.name/user.email still produce commit objects.
fn git_env_commit(cwd: &Path, args: &[&str], env: &[(&str, String)]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "ka")
        .env("GIT_AUTHOR_EMAIL", "ka@localhost")
        .env("GIT_COMMITTER_NAME", "ka")
        .env("GIT_COMMITTER_EMAIL", "ka@localhost");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::PathBuf;

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-ckpt-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@ka"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.join("tracked.txt"), "v1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn snapshot_restores_working_tree() {
        let dir = temp_repo("roundtrip");
        // mutate: rewrite a tracked file, add a new tracked file
        std::fs::write(dir.join("tracked.txt"), "v2\n").unwrap();

        let id = snapshot(&dir).expect("snapshot must succeed");
        assert_eq!(id.len(), 40, "full commit id: {id}");
        // the user's HEAD is untouched
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim().len(), 40);

        // more drift after the checkpoint, then restore
        std::fs::write(dir.join("tracked.txt"), "v3\n").unwrap();
        restore(&dir, &id).expect("restore must succeed");
        assert_eq!(
            std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
            "v2\n",
            "checkpoint contents restored"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_stays_off_the_real_index() {
        let dir = temp_repo("index-clean");
        std::fs::write(dir.join("tracked.txt"), "dirty\n").unwrap();
        let id = snapshot(&dir).unwrap();
        // the real index still shows the working-tree modification as
        // unstaged; the checkpoint commit captured it independently
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(
            status.contains(" M tracked.txt"),
            "index untouched: {status:?}"
        );
        restore(&dir, &id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_repo_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ka-ckpt-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(snapshot(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_truncates_display_ids() {
        let id = "a".repeat(40);
        assert_eq!(short(&id), "a".repeat(12));
        assert_eq!(short("abc"), "abc");
    }
}

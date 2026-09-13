//! `ka update`: self-update against GitHub releases. Artifacts are
//! ed25519-signed (see `cargo xtask sign`); this module refuses to
//! touch an unsigned build and verifies every download before the
//! atomic binary swap.
//!
//! Layout: the CI release pipeline tags `vX.Y.Z` (assets
//! `ka-<triple>.tar.gz` + `.sig` + `.sha256`); the legacy
//! `ka-<channel>-*` tag shape still matches for hand-made releases.
//! `ka-<target-triple>.tar.gz` (+ `.sig`). Everything is parameterized
//! so tests can drive a local fixture server and swap a temp file.

use std::path::{Path, PathBuf};
use std::process::Command;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Placeholder repo (config `[update] repo` overrides; anything else
/// fails with the configured name in the error — no guessing).
pub const DEFAULT_REPO: &str = "kampka/ka";
const DEFAULT_API: &str = "https://api.github.com";

/// The update result summary, ready to print.
#[derive(Debug)]
pub struct UpdateOutcome {
    /// Version before the update.
    pub old_version: String,
    /// New version after the swap (None = nothing installed).
    pub new_version: Option<String>,
    /// The release tag that was found/installed.
    pub latest_tag: String,
    /// True when the newest release is not newer than the running
    /// version (nothing to do — no download, no reinstall).
    pub up_to_date: bool,
}

/// CLI entry: resolve defaults from env/config, then run.
pub async fn run(channel: &str, check_only: bool, repo: &str) -> Result<String, String> {
    let api_base = std::env::var("KA_UPDATE_API_BASE").unwrap_or_else(|_| DEFAULT_API.to_string());
    let state_dir = state_dir();
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let outcome = check_and_install(
        &api_base,
        repo,
        channel,
        crate::PUBLIC_KEY,
        &state_dir,
        &exe,
        check_only,
    )
    .await?;
    if outcome.up_to_date {
        return Ok(format!(
            "up to date: {} (latest release: {})",
            outcome.old_version, outcome.latest_tag
        ));
    }
    match outcome.new_version {
        Some(new) => Ok(format!(
            "updated: {} → {new} ({})",
            outcome.old_version, outcome.latest_tag
        )),
        None => Ok(format!(
            "current {} — latest release: {}",
            outcome.old_version, outcome.latest_tag
        )),
    }
}

/// Parse a `(major, minor, patch)` triple out of a release tag
/// (`v0.2.0`, `v0.2.0-edge`, `ka-stable-1.2.3`) or the running version
/// string (`0.1.0 (hash)`).
fn version_triple(s: &str) -> Option<(u64, u64, u64)> {
    let token = s.split([' ', '-']).find(|t| {
        let digits = t.strip_prefix('v').unwrap_or(t);
        digits.chars().next().is_some_and(|c| c.is_ascii_digit())
    })?;
    let digits = token.strip_prefix('v').unwrap_or(token);
    let mut it = digits.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// XDG state dir for downloads (`~/.local/state/ka/update` fallback).
fn state_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("ka/update")
}

/// The release triple this binary updates for.
pub fn artifact_triple() -> String {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-musl".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-musl".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        (arch, os) => format!("{arch}-unknown-{os}"),
    }
}

/// The running binary's version string.
fn current_version() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let out = Command::new(exe).arg("--version").output().ok()?;
            String::from_utf8(out.stdout).ok()
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Fetch, verify, and (unless `check_only`) atomically install.
pub async fn check_and_install(
    api_base: &str,
    repo: &str,
    channel: &str,
    pubkey: Option<&str>,
    state_dir: &Path,
    exe: &Path,
    check_only: bool,
) -> Result<UpdateOutcome, String> {
    let old_version = current_version();
    let Some(pubkey) = pubkey else {
        return Err("unsigned build: refusing to update (rebuild with KA_PUBKEY set)".to_string());
    };
    let verifying = verifying_key(pubkey)?;

    let client = reqwest::Client::new();
    let url = format!("{api_base}/repos/{repo}/releases");
    let releases: serde_json::Value = client
        .get(&url)
        .header("User-Agent", "ka-update")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GET {url}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("releases JSON: {e}"))?;

    let legacy_prefix = format!("ka-{channel}");
    let release = releases
        .as_array()
        .and_then(|list| {
            list.iter().find(|r| {
                r["tag_name"].as_str().is_some_and(|t| {
                    // CI tags `vX.Y.Z` (version-shaped — a stray `v`
                    // prefix alone must not match); the legacy
                    // `ka-<channel>-*` shape still matches too.
                    (t.starts_with('v') && version_triple(t).is_some())
                        || t.starts_with(&legacy_prefix)
                })
            })
        })
        .ok_or_else(|| format!("no release tagged v* (or {legacy_prefix:?}) in {repo}"))?;
    let tag = release["tag_name"].as_str().unwrap_or_default().to_string();

    // never reinstall the running version (or downgrade to a
    // re-published older tag): compare triples before any download
    if let (Some(new), Some(cur)) = (version_triple(&tag), version_triple(&old_version)) {
        if new <= cur {
            return Ok(UpdateOutcome {
                old_version,
                new_version: None,
                latest_tag: tag,
                up_to_date: true,
            });
        }
    }

    let asset_name = format!("ka-{}.tar.gz", artifact_triple());
    let asset_url = |name: &str| -> Result<String, String> {
        release["assets"]
            .as_array()
            .and_then(|assets| {
                assets.iter().find_map(|a| {
                    (a["name"].as_str() == Some(name))
                        .then(|| a["browser_download_url"].as_str().map(str::to_string))
                        .flatten()
                })
            })
            .ok_or_else(|| format!("release {tag}: asset {name:?} missing"))
    };
    let tar_url = asset_url(&asset_name)?;
    let sig_url = asset_url(&format!("{asset_name}.sig"))?;

    let download = |url: String| {
        let client = &client;
        async move {
            client
                .get(&url)
                .header("User-Agent", "ka-update")
                .send()
                .await
                .map_err(|e| format!("GET {url}: {e}"))?
                .error_for_status()
                .map_err(|e| format!("GET {url}: {e}"))?
                .bytes()
                .await
                .map(Vec::from)
                .map_err(|e| format!("GET {url}: {e}"))
        }
    };
    let tarball = download(tar_url).await?;
    let sig_bytes = download(sig_url).await?;
    let sig_b64 = String::from_utf8_lossy(&sig_bytes).into_owned();

    verify(&tarball, sig_b64.trim(), &verifying)?;

    if check_only {
        return Ok(UpdateOutcome {
            old_version,
            new_version: None,
            latest_tag: tag,
            up_to_date: false,
        });
    }

    // extract into the state dir, then atomically swap
    let extract_dir = state_dir.join("extract");
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("mkdir {}: {e}", extract_dir.display()))?;
    let tarball_path = state_dir.join(&asset_name);
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("mkdir {}: {e}", state_dir.display()))?;
    std::fs::write(&tarball_path, &tarball)
        .map_err(|e| format!("write {}: {e}", tarball_path.display()))?;
    let status = Command::new("tar")
        .args([
            "xzf",
            tarball_path.to_string_lossy().as_ref(),
            "-C",
            extract_dir.to_string_lossy().as_ref(),
        ])
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    if !status.success() {
        return Err("tar extraction failed".to_string());
    }
    let new_bin = extract_dir.join("ka");
    if !new_bin.exists() {
        return Err("tarball: no 'ka' entry".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod: {e}"))?;
    }

    // atomic rename over the running binary; a busy executable (ETXTBSY)
    // lands the new binary next to the old one with instructions
    match std::fs::rename(&new_bin, exe) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(26) => {
            let beside = exe.with_file_name("ka.new");
            std::fs::rename(&new_bin, &beside)
                .map_err(|e| format!("stage {}: {e}", beside.display()))?;
            return Ok(UpdateOutcome {
                old_version,
                new_version: Some(format!(
                    "STAGED at {} — busy executable; swap manually: mv {} {}",
                    beside.display(),
                    beside.display(),
                    exe.display()
                )),
                latest_tag: tag,
                up_to_date: false,
            });
        }
        Err(e) => return Err(format!("install: {e}")),
    }

    let new_version = Command::new(exe)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| tag.clone());
    Ok(UpdateOutcome {
        old_version,
        new_version: Some(new_version),
        latest_tag: tag,
        up_to_date: false,
    })
}

/// Verify tarball bytes against a base64 ed25519 signature.
fn verify(payload: &[u8], sig_b64: &str, verifying: &VerifyingKey) -> Result<(), String> {
    let sig_bytes: [u8; 64] = unb64(sig_b64)
        .ok_or_else(|| "signature: not base64".to_string())?
        .try_into()
        .map_err(|_| "signature: wrong length".to_string())?;
    verifying
        .verify(payload, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| "signature verification FAILED — artifact tampered or wrong key")?;
    Ok(())
}

fn verifying_key(pubkey_b64: &str) -> Result<VerifyingKey, String> {
    let bytes: [u8; 32] = unb64(pubkey_b64)
        .ok_or_else(|| "KA_PUBKEY: not base64".to_string())?
        .try_into()
        .map_err(|_| "KA_PUBKEY: wrong length".to_string())?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| format!("KA_PUBKEY: {e}"))
}

/// Base64 decode (whitespace and '=' tolerated).
fn unb64(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    for chunk in cleaned.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ed25519_dalek::SigningKey;

    const SEED: [u8; 32] = [7u8; 32];
    const TARGET: &str = "x86_64-unknown-linux-musl";

    fn b64(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = ((chunk[0] as u32) << 16)
                | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
                | (*chunk.get(2).unwrap_or(&0) as u32);
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

    /// Serve a releases list plus tarball/signature assets.
    async fn fixture_server(
        listener: tokio::net::TcpListener,
        server_addr: std::net::SocketAddr,
        tarball: Vec<u8>,
        sig: String,
        wrong_sig: bool,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let req = String::from_utf8_lossy(&buf).into_owned();
            let body: Vec<u8> = if req.starts_with("GET /repos/owner/ka/releases") {
                let releases = serde_json::json!([
                    {"tag_name": "v-unrelated", "assets": []},
                    {"tag_name": "ka-stable-9.9.9", "assets": [
                        {"name": format!("ka-{TARGET}.tar.gz"),
                         "browser_download_url": format!("http://{server_addr}/artifact")},
                        {"name": format!("ka-{TARGET}.tar.gz.sig"),
                         "browser_download_url": format!("http://{server_addr}/signature")}
                    ]}
                ]);
                releases.to_string().into_bytes()
            } else if req.starts_with("GET /artifact") {
                tarball.clone()
            } else if req.starts_with("GET /signature") {
                if wrong_sig {
                    format!("{}==", "A".repeat(86)).into_bytes()
                } else {
                    format!("{sig}\n").into_bytes()
                }
            } else {
                b"not found".to_vec()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.ok();
            sock.write_all(&body).await.ok();
            sock.shutdown().await.ok();
        }
    }

    fn make_tarball(dir: &Path, marker: &str) -> Vec<u8> {
        let payload = dir.join("ka");
        std::fs::write(&payload, marker).unwrap();
        let tar_path = dir.join("ka.tar.gz");
        let status = Command::new("tar")
            .args([
                "czf",
                tar_path.to_string_lossy().as_ref(),
                "-C",
                dir.to_string_lossy().as_ref(),
                "ka",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(&tar_path).unwrap()
    }

    #[tokio::test]
    async fn valid_artifact_swaps_and_tampered_refuses() {
        let work = std::env::temp_dir().join(format!("ka-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();

        let signing = SigningKey::from_bytes(&SEED);
        let pubkey = b64(signing.verifying_key().as_bytes());

        let marker = "KA-BINARY-V999\n";
        let tarball = make_tarball(&work, marker);
        use ed25519_dalek::Signer;
        let sig = b64(&signing.sign(&tarball).to_bytes());

        // tampered case: right key, wrong payload bytes
        std::fs::create_dir_all(work.join("tampered")).unwrap();
        let tampered_tarball = make_tarball(&work.join("tampered"), "TAMPERED");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tampered_tarball,
            sig.clone(),
            false,
        ));

        let state = work.join("state");
        let exe = work.join("installed-ka");
        std::fs::write(&exe, "OLD").unwrap();

        let err = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &state,
            &exe,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.contains("tampered"), "{err}");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"OLD",
            "exe untouched on refusal"
        );

        // tampered-signature case: garbage sig
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tarball.clone(),
            sig.clone(),
            true,
        ));
        let err = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &state,
            &exe,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.contains("FAILED"), "{err}");

        // valid case: full update swaps the temp "binary"
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(
            listener,
            addr,
            tarball.clone(),
            sig.clone(),
            false,
        ));
        let outcome = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &state,
            &exe,
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.latest_tag, "ka-stable-9.9.9");
        assert_eq!(std::fs::read(&exe).unwrap(), marker.as_bytes());
        // the fixture "binary" is a text file: --version fails and the
        // tag name is reported as the new version instead
        assert_eq!(outcome.new_version.as_deref(), Some("ka-stable-9.9.9"));

        // check-only: reports without touching the exe
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fixture_server(listener, addr, tarball, sig, false));
        let outcome = check_and_install(
            &format!("http://{addr}"),
            "owner/ka",
            "stable",
            Some(&pubkey),
            &state,
            &exe,
            true,
        )
        .await
        .unwrap();
        assert!(outcome.new_version.is_none());
        assert_eq!(outcome.latest_tag, "ka-stable-9.9.9");

        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn unsigned_build_refuses_before_network() {
        let work = std::env::temp_dir().join(format!("ka-update-unsigned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let err = check_and_install(
            "http://127.0.0.1:1",
            "owner/ka",
            "stable",
            None,
            &work,
            &work.join("ka"),
            false,
        )
        .await
        .unwrap_err();
        assert!(err.contains("unsigned build"), "{err}");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn version_triples_parse_all_release_shapes() {
        assert_eq!(version_triple("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(version_triple("v0.2.0-edge"), Some((0, 2, 0)));
        assert_eq!(version_triple("ka-stable-1.2.3"), Some((1, 2, 3)));
        assert_eq!(version_triple("0.1.0 (2c1f5fb)"), Some((0, 1, 0)));
        assert_eq!(version_triple("10.0.2"), Some((10, 0, 2)));
        assert_eq!(version_triple("no-version-here"), None);
    }
}

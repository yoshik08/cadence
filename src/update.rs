//! Auto-updater: GitHub Releases of yoshik08/cadence (public, no token).
//!
//! - One check per launch, off the UI thread, 5 s timeout; any error is logged and ignored.
//! - Only the release's own `cadence.exe` asset (https), downloaded into `<data>\update\`,
//!   checked against the API size and SHA256 (asset `digest`, or a `cadence.exe.sha256` asset).
//! - Applying swaps the program file only: the running exe is renamed to `cadence.exe.old`
//!   (Windows allows renaming a running exe), the new one is copied into its place, started,
//!   and this process quits. Nothing outside `update\` in the data folder is touched, so the
//!   Client ID and both sign-ins survive.
//! - Loop protection: `update\applying` is written before a swap. A start that finds it next
//!   to a staged exe knows the last attempt didn't finish and discards the staged files.
//! - `cadence.exe.old` stays through the first launch of the new version (manual rollback)
//!   and is deleted on the launch after that.

use serde_json::Value;
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::logln;

const GITHUB_LATEST: &str = "https://api.github.com/repos/yoshik08/cadence/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/yoshik08/cadence/releases/latest";
const ASSET: &str = "cadence.exe";
const USER_AGENT: &str = "Cadence-updater";
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

const STAGED: &str = "cadence.exe";
const STAGED_VERSION: &str = "version.txt";
const PARTIAL: &str = "cadence.exe.download";
const MARKER: &str = "applying";
const ERROR: &str = "error.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// `v1.5.0`, `1.5`, `1.5.0` -> Version. Prereleases (`1.5.0-beta`) and build metadata give
/// None, so they are never offered.
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    if s.is_empty() || s.contains(['-', '+']) {
        return None;
    }
    let mut parts = s.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version(major, minor, patch))
}

pub fn current() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).unwrap_or(Version(0, 0, 0))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    pub page: String,
    pub url: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: Option<String>,
    /// A `cadence.exe.sha256` asset, used when the API gives no digest.
    pub checksum_url: Option<String>,
}

fn allowed_url(url: &str) -> bool {
    url.starts_with("https://") || (cfg!(debug_assertions) && url.starts_with("http://127.0.0.1:"))
}

/// The release in a `releases/latest` response, if it is a usable stable release with our asset.
pub fn parse_release(v: &Value) -> Option<Release> {
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let version = parse_version(v["tag_name"].as_str()?)?;
    let assets = v["assets"].as_array()?;
    let asset = assets.iter().find(|a| a["name"].as_str() == Some(ASSET))?;
    let url = asset["browser_download_url"].as_str()?.to_string();
    if !allowed_url(&url) {
        return None;
    }
    let size = asset["size"].as_u64().filter(|s| *s > 0)?;
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase);
    let checksum_url = assets
        .iter()
        .find(|a| a["name"].as_str() == Some("cadence.exe.sha256"))
        .and_then(|a| a["browser_download_url"].as_str())
        .filter(|u| allowed_url(u))
        .map(str::to_string);
    let page = v["html_url"].as_str().filter(|u| u.starts_with("https://")).unwrap_or(RELEASES_PAGE).to_string();
    Some(Release { version, page, url, size, sha256, checksum_url })
}

/// GitHub's API, or (debug builds) `CADENCE_UPDATE_URL` for tests against a local server.
pub fn api_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(u) = std::env::var("CADENCE_UPDATE_URL") {
        return u;
    }
    GITHUB_LATEST.to_string()
}

/// A release newer than `current`, or None (no update, or any error: silent).
pub async fn check(http: &reqwest::Client, url: &str, current: Version) -> Option<Release> {
    let resp = http
        .get(url)
        .header("user-agent", USER_AGENT)
        .header("accept", "application/vnd.github+json")
        .timeout(CHECK_TIMEOUT)
        .send()
        .await;
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            logln!("update: check got HTTP {}", r.status().as_u16());
            return None;
        }
        Err(e) => {
            logln!("update: check failed: {e}");
            return None;
        }
    };
    let v: Value = resp.json().await.ok()?;
    let rel = parse_release(&v)?;
    if rel.version > current {
        logln!("update: {} available (running {current})", rel.version);
        Some(rel)
    } else {
        None
    }
}

pub fn update_dir(data: &Path) -> PathBuf {
    data.join("update")
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads and verifies `rel` into `dir`, staging it as `dir\cadence.exe`. `progress` gets
/// 0.0..=1.0. On any mismatch the partial file is removed and nothing is staged.
pub async fn download(http: &reqwest::Client, rel: &Release, dir: &Path, mut progress: impl FnMut(f32)) -> Result<PathBuf, String> {
    use std::io::Write;
    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create the update folder ({e})."))?;
    let expected_sha = match (&rel.sha256, &rel.checksum_url) {
        (Some(h), _) => Some(h.clone()),
        (None, Some(u)) => {
            let text = http.get(u).header("user-agent", USER_AGENT).timeout(CHECK_TIMEOUT).send().await
                .map_err(|e| format!("Couldn't get the checksum ({e})."))?
                .text().await.map_err(|e| format!("Couldn't get the checksum ({e})."))?;
            let h = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("The release's checksum file is malformed.".into());
            }
            Some(h)
        }
        (None, None) => None,
    };
    let partial = dir.join(PARTIAL);
    let result: Result<(), String> = async {
        let mut resp = http
            .get(&rel.url)
            .header("user-agent", USER_AGENT)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("Download failed ({e})."))?;
        if !resp.status().is_success() {
            return Err(format!("Download failed (HTTP {}).", resp.status().as_u16()));
        }
        let mut file = std::fs::File::create(&partial).map_err(|e| format!("Couldn't write the download ({e})."))?;
        let mut got = 0u64;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("Download failed ({e})."))? {
            got += chunk.len() as u64;
            if got > rel.size {
                return Err("The download is larger than the release says.".into());
            }
            file.write_all(&chunk).map_err(|e| format!("Couldn't write the download ({e})."))?;
            progress(got as f32 / rel.size as f32);
        }
        file.flush().map_err(|e| e.to_string())?;
        drop(file);
        if got != rel.size {
            return Err(format!("Size mismatch: got {got} bytes, expected {}.", rel.size));
        }
        if let Some(want) = &expected_sha {
            let have = sha256_hex(&partial).map_err(|e| e.to_string())?;
            if &have != want {
                return Err("Checksum mismatch: the download was refused.".into());
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = std::fs::remove_file(&partial);
        logln!("update: download refused: {e}");
        return Err(e);
    }
    let staged = dir.join(STAGED);
    std::fs::rename(&partial, &staged).map_err(|e| format!("Couldn't stage the update ({e})."))?;
    let _ = std::fs::write(dir.join(STAGED_VERSION), rel.version.to_string());
    logln!("update: {} downloaded and verified (sha256 {})", rel.version, if expected_sha.is_some() { "checked" } else { "not provided" });
    Ok(staged)
}

/// The version staged for the next start, if any.
pub fn staged_version(dir: &Path) -> Option<Version> {
    if !dir.join(STAGED).exists() {
        return None;
    }
    parse_version(&std::fs::read_to_string(dir.join(STAGED_VERSION)).ok()?)
}

/// The error the last apply left (shown in Settings > Updates once).
pub fn take_error(dir: &Path) -> Option<String> {
    let e = std::fs::read_to_string(dir.join(ERROR)).ok()?;
    let _ = std::fs::remove_file(dir.join(ERROR));
    Some(e)
}

/// Can the program file be replaced (the folder is writable)?
pub fn can_install(exe: &Path) -> bool {
    let Some(folder) = exe.parent() else { return false };
    let probe = folder.join(".cadence-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

pub fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// Replaces `exe` with `new`: `exe` -> `exe.old`, copy `new` -> `exe`. On failure the old exe is
/// put back, so the current version keeps working.
pub fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    let old = old_path(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("Couldn't move the current version aside ({e})."))?;
    if let Err(e) = std::fs::copy(new, exe) {
        let _ = std::fs::remove_file(exe);
        let _ = std::fs::rename(&old, exe);
        return Err(format!("Couldn't put the new version in place ({e})."));
    }
    Ok(())
}

fn discard_staged(dir: &Path) {
    for f in [STAGED, STAGED_VERSION, PARTIAL] {
        let _ = std::fs::remove_file(dir.join(f));
    }
}

/// Swaps in the staged exe now (marker first). Ok: the caller starts `exe` and quits.
pub fn apply(exe: &Path, dir: &Path) -> Result<(), String> {
    let staged = dir.join(STAGED);
    if !staged.exists() {
        return Err("No downloaded update to install.".into());
    }
    std::fs::write(dir.join(MARKER), b"").map_err(|e| format!("Couldn't start the update ({e})."))?;
    match swap(exe, &staged) {
        Ok(()) => {
            discard_staged(dir);
            logln!("update: swapped in the new version");
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(dir.join(MARKER));
            discard_staged(dir);
            let _ = std::fs::write(dir.join(ERROR), &e);
            logln!("update: swap failed: {e}");
            Err(e)
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Start {
    /// Nothing to do; keep starting.
    Continue,
    /// The staged update was swapped in: start the exe again and quit.
    Restart,
}

/// At launch, before the window: finishes or applies updates.
pub fn on_start(exe: &Path, dir: &Path) -> Start {
    let marker = dir.join(MARKER);
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        if dir.join(STAGED).exists() {
            // The marker was written but the swap never completed: don't try again.
            discard_staged(dir);
            let _ = std::fs::write(dir.join(ERROR), "The last update didn't finish, so it was skipped.");
            logln!("update: previous apply didn't finish; staged update discarded");
        } else {
            logln!("update: now running {} (previous version kept as cadence.exe.old)", current());
        }
        return Start::Continue;
    }
    let _ = std::fs::remove_file(old_path(exe));
    match staged_version(dir) {
        Some(v) if v > current() => match apply(exe, dir) {
            Ok(()) => Start::Restart,
            Err(_) => Start::Continue,
        },
        Some(_) => {
            discard_staged(dir);
            Start::Continue
        }
        None => Start::Continue,
    }
}

/// Starts the (new) exe detached.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    std::process::Command::new(exe).spawn().map(|_| ()).map_err(|e| format!("Couldn't start the new version ({e})."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cadence-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn hash_tree(dir: &Path, skip: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p == skip {
                continue;
            }
            if p.is_dir() {
                out.extend(hash_tree(&p, skip));
            } else {
                out.push((p.clone(), sha256_hex(&p).unwrap()));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn versions() {
        assert!(parse_version("1.4.0") < parse_version("1.5.0"));
        assert_eq!(parse_version("v1.5.0"), parse_version("1.5.0"));
        assert!(parse_version("1.10.0") > parse_version("1.9.0"));
        assert_eq!(parse_version("v1.4"), Some(Version(1, 4, 0)));
        assert_eq!(parse_version("1.5.0-beta.1"), None);
        assert_eq!(parse_version("1.5.0+build"), None);
        assert_eq!(parse_version("1.x"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(current(), parse_version(env!("CARGO_PKG_VERSION")).unwrap());
    }

    fn release_json(tag: &str, url: &str, size: u64, digest: Option<&str>) -> Value {
        json!({
            "tag_name": tag, "draft": false, "prerelease": false,
            "html_url": "https://github.com/yoshik08/cadence/releases/tag/v9",
            "assets": [{ "name": "cadence.exe", "browser_download_url": url, "size": size, "digest": digest }]
        })
    }

    #[test]
    fn release_parsing() {
        let h = "a".repeat(64);
        let r = parse_release(&release_json("v1.6.0", "https://x/cadence.exe", 10, Some(&format!("sha256:{h}")))).unwrap();
        assert_eq!(r.version, Version(1, 6, 0));
        assert_eq!(r.sha256.as_deref(), Some(h.as_str()));
        let mut pre = release_json("v1.6.0", "https://x/cadence.exe", 10, None);
        pre["prerelease"] = json!(true);
        assert!(parse_release(&pre).is_none());
        let mut draft = release_json("v1.6.0", "https://x/cadence.exe", 10, None);
        draft["draft"] = json!(true);
        assert!(parse_release(&draft).is_none());
        assert!(parse_release(&release_json("v1.6.0-rc1", "https://x/cadence.exe", 10, None)).is_none());
        assert!(parse_release(&release_json("v1.6.0", "ftp://x/cadence.exe", 10, None)).is_none());
        assert!(parse_release(&release_json("v1.6.0", "https://x/cadence.exe", 0, None)).is_none());
    }

    /// A tiny HTTP server: `/latest` -> `json`, `/cadence.exe` -> `body`. Returns its base URL.
    async fn serve(json: Value, body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let json = json.to_string().replace("{BASE}", &base);
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let (json, body) = (json.clone(), body.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (ct, payload) = if req.starts_with("GET /latest") {
                        ("application/json", json.into_bytes())
                    } else if req.starts_with("GET /cadence.exe") {
                        ("application/octet-stream", body)
                    } else {
                        ("text/plain", b"nope".to_vec())
                    };
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", payload.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&payload).await;
                });
            }
        });
        base
    }

    fn fake_release(tag: &str, body: &[u8], digest_of: &[u8]) -> Value {
        let mut h = sha2::Sha256::new();
        h.update(digest_of);
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        release_json(tag, "{BASE}/cadence.exe", body.len() as u64, Some(&format!("sha256:{hex}")))
    }

    #[tokio::test]
    async fn check_against_fake_endpoint() {
        let http = reqwest::Client::new();
        let body = b"new exe".to_vec();
        let base = serve(fake_release("v1.6.0", &body, &body), body.clone()).await;
        let r = check(&http, &format!("{base}/latest"), Version(1, 5, 0)).await;
        assert_eq!(r.map(|r| r.version), Some(Version(1, 6, 0)));
        assert!(check(&http, &format!("{base}/latest"), Version(1, 6, 0)).await.is_none());
        assert!(check(&http, &format!("{base}/latest"), Version(1, 7, 0)).await.is_none());
        // Nothing listens on port 9 here: silent None.
        assert!(check(&http, "http://127.0.0.1:9/latest", Version(1, 5, 0)).await.is_none());
        // Not JSON.
        assert!(check(&http, &format!("{base}/other"), Version(1, 5, 0)).await.is_none());
    }

    #[tokio::test]
    async fn checksum_and_size_mismatch_refused() {
        let http = reqwest::Client::new();
        let body = b"new exe".to_vec();
        let base = serve(fake_release("v1.6.0", &body, b"something else"), body.clone()).await;
        let rel = check(&http, &format!("{base}/latest"), Version(1, 5, 0)).await.unwrap();
        let dir = temp("mismatch");
        let e = download(&http, &rel, &dir, |_| {}).await.unwrap_err();
        assert!(e.contains("Checksum"), "{e}");
        assert!(!dir.join(STAGED).exists() && !dir.join(PARTIAL).exists());
        let mut wrong_size = rel.clone();
        wrong_size.size += 1;
        wrong_size.sha256 = None;
        assert!(download(&http, &wrong_size, &dir, |_| {}).await.unwrap_err().contains("Size"));
        assert!(!dir.join(STAGED).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Full path: download, stage, apply on next start; the data folder (Client ID, tokens,
    /// settings) is byte-identical afterwards, and the marker stops a half-done apply repeating.
    #[tokio::test]
    async fn staged_update_applies_and_data_is_untouched() {
        let http = reqwest::Client::new();
        let body = b"cadence 99".to_vec();
        let base = serve(fake_release("v99.0.0", &body, &body), body.clone()).await;

        let root = temp("apply");
        let data = root.join("data");
        let install = root.join("install");
        std::fs::create_dir_all(data.join("librespot")).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(data.join("client_id.txt"), "0123456789abcdef0123456789abcdef\n").unwrap();
        std::fs::write(data.join("web_token.json"), r#"{"v":2,"access_token":"a"}"#).unwrap();
        std::fs::write(data.join("librespot").join("credentials.json"), r#"{"auth_data":"x"}"#).unwrap();
        std::fs::write(data.join("settings.json"), r#"{"autoplay":true}"#).unwrap();
        let exe = install.join("cadence.exe");
        std::fs::write(&exe, b"cadence old").unwrap();
        let dir = update_dir(&data);
        let before = hash_tree(&data, &dir);

        let rel = check(&http, &format!("{base}/latest"), current()).await.unwrap();
        let seen = std::sync::Mutex::new(0.0f32);
        download(&http, &rel, &dir, |p| *seen.lock().unwrap() = p).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), 1.0);
        assert_eq!(staged_version(&dir), Some(Version(99, 0, 0)));

        // Next start applies it.
        assert_eq!(on_start(&exe, &dir), Start::Restart);
        assert_eq!(std::fs::read(&exe).unwrap(), body);
        assert_eq!(std::fs::read(old_path(&exe)).unwrap(), b"cadence old");
        assert!(dir.join(MARKER).exists());
        // The new version's first start clears the marker and keeps the rollback copy.
        assert_eq!(on_start(&exe, &dir), Start::Continue);
        assert!(!dir.join(MARKER).exists());
        assert!(old_path(&exe).exists());
        // The launch after that drops the rollback copy.
        assert_eq!(on_start(&exe, &dir), Start::Continue);
        assert!(!old_path(&exe).exists());

        assert_eq!(hash_tree(&data, &dir), before, "data folder changed");

        // Loop protection: a marker next to a staged exe means the last apply died midway.
        download(&http, &rel, &dir, |_| {}).await.unwrap();
        std::fs::write(dir.join(MARKER), b"").unwrap();
        assert_eq!(on_start(&exe, &dir), Start::Continue);
        assert!(!dir.join(STAGED).exists() && !dir.join(MARKER).exists());
        assert!(take_error(&dir).is_some());
        assert_eq!(hash_tree(&data, &dir), before, "data folder changed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_swap_keeps_old_version() {
        let root = temp("swapfail");
        let exe = root.join("cadence.exe");
        std::fs::write(&exe, b"old").unwrap();
        let e = swap(&exe, &root.join("missing.exe")).unwrap_err();
        assert!(e.contains("new version"), "{e}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&root);
    }
}

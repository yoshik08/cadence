//! Disk cache under `%LOCALAPPDATA%\cadence\cache`: Web API responses, album art, lyrics.
//! (Audio is cached by librespot itself in `cache\audio`.) Every subfolder is size-capped and
//! pruned oldest-first at startup, so nothing grows without bound and nothing lives in RAM.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// `%LOCALAPPDATA%\cadence\cache\<sub>`, created on first use.
pub fn dir(sub: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let d = base.join("cadence").join("cache").join(sub);
    let _ = std::fs::create_dir_all(&d);
    d
}

/// 64-bit FNV-1a of `s` as 16 hex chars: a file name for any key.
pub fn key(s: &str) -> String {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The cached bytes for `name`, unless older than `max_age`.
pub fn read(sub: &str, name: &str, max_age: Option<Duration>) -> Option<Vec<u8>> {
    let path = dir(sub).join(name);
    if let Some(max) = max_age {
        let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
        if SystemTime::now().duration_since(modified).unwrap_or_default() > max {
            return None;
        }
    }
    std::fs::read(path).ok()
}

/// Writes atomically (temp file + rename), so a crash never leaves a half file behind.
pub fn write(sub: &str, name: &str, data: &[u8]) {
    let d = dir(sub);
    let tmp = d.join(format!("{name}.tmp"));
    if std::fs::write(&tmp, data).is_ok() && std::fs::rename(&tmp, d.join(name)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Deletes the oldest files of `sub` until it is under `max_bytes`.
pub fn prune(sub: &str, max_bytes: u64) {
    let Ok(rd) = std::fs::read_dir(dir(sub)) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max_bytes {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// Startup housekeeping (run off the UI thread).
pub fn prune_all() {
    prune("api", 64 << 20);
    prune("art", 192 << 20);
    prune("lyrics", 32 << 20);
}

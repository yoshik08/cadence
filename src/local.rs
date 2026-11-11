//! Local music: folders chosen in Settings are scanned recursively (MP3, FLAC, WAV, AAC, M4A,
//! Ogg), indexed in SQLite, watched for changes, and handed to the (patched) librespot player
//! as `spotify:local:` uris, so local tracks play, queue and shuffle like Spotify ones.
//! Files that disappear are kept and marked missing.

use crate::db::{self, LocalTrack};
use librespot_core::SpotifyUri;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const EXTS: [&str; 8] = ["mp3", "flac", "wav", "aac", "m4a", "mp4", "ogg", "m4p"];
static SCANNING: AtomicBool = AtomicBool::new(false);
/// Bumped when the folder list changes so the watcher restarts.
static WATCH_GEN: AtomicU64 = AtomicU64::new(0);

pub fn folders() -> Vec<String> {
    crate::prefs::get("local_folders").as_array().map_or(Vec::new(), |a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}

pub fn set_folders(list: &[String]) {
    crate::prefs::set("local_folders", serde_json::json!(list));
    WATCH_GEN.fetch_add(1, Ordering::Relaxed);
    rescan();
    watch();
}

fn decode_part(s: &str) -> String {
    // application/x-www-form-urlencoded: '+' is a space, %XX a byte.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let h = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).trim_end_matches(char::from(0)).trim().to_string()
}

fn read_file(path: &Path, mtime: i64, size: i64) -> LocalTrack {
    let p = path.display().to_string();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    match librespot_playback::local_uri_for(path) {
        Ok(uri) => {
            let (artist, album, title, dur) = match &uri {
                SpotifyUri::Local { artist, album_title, track_title, duration } => (decode_part(artist), decode_part(album_title), decode_part(track_title), duration.as_millis() as i64),
                _ => Default::default(),
            };
            LocalTrack {
                path: p,
                uri: uri.to_uri().unwrap_or_default(),
                title: if title.is_empty() { stem } else { title },
                artist: if artist.is_empty() { "Unknown artist".into() } else { artist },
                album,
                duration_ms: dur,
                mtime,
                size,
                missing: false,
                error: String::new(),
            }
        }
        Err(e) => LocalTrack { path: p, title: stem, mtime, size, error: e.to_string(), ..Default::default() },
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().and_then(|x| x.to_str()).is_some_and(|x| EXTS.contains(&x.to_ascii_lowercase().as_str())) {
            out.push(path);
        }
    }
}

fn scan_now() -> (usize, usize) {
    let roots = folders();
    db::local_retain_roots(&roots);
    let mut files = Vec::new();
    for r in &roots {
        walk(Path::new(r), &mut files);
    }
    let mut seen = std::collections::HashSet::new();
    let mut new = 0;
    for f in &files {
        let Ok(meta) = std::fs::metadata(f) else { continue };
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
        let size = meta.len() as i64;
        let key = f.display().to_string();
        seen.insert(key.clone());
        match db::local_get(&key) {
            Some(t) if t.mtime == mtime && t.size == size => {
                if t.missing {
                    db::local_mark_missing(&key, false);
                }
            }
            _ => {
                db::local_upsert(&read_file(f, mtime, size));
                new += 1;
            }
        }
    }
    let mut missing = 0;
    for t in db::local_all() {
        if !seen.contains(&t.path) && !t.missing {
            db::local_mark_missing(&t.path, true);
            missing += 1;
        }
    }
    register();
    (new, missing)
}

/// Gives the player the uri -> file map of every playable local file.
pub fn register() {
    let mut map = HashMap::new();
    for t in db::local_all() {
        if t.missing || !t.error.is_empty() {
            continue;
        }
        if let Ok(uri) = SpotifyUri::from_uri(&t.uri) {
            map.insert(uri, PathBuf::from(&t.path));
        }
    }
    librespot_playback::register_local_files(map);
}

/// Rescans in the background; refreshes the Local files page when it is open.
pub fn rescan() {
    if SCANNING.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        let (new, missing) = scan_now();
        SCANNING.store(false, Ordering::Relaxed);
        crate::logln!("local: scan done ({new} new or changed, {missing} missing)");
        let _ = slint::invoke_from_event_loop(|| {
            crate::browse::local_changed();
        });
    });
}

/// Watches the folders (subtrees) and rescans 2 s after changes settle.
pub fn watch() {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::Storage::FileSystem::{FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification};
    use windows::Win32::System::Threading::WaitForMultipleObjects;
    let roots = folders();
    if roots.is_empty() {
        return;
    }
    let generation = WATCH_GEN.load(Ordering::Relaxed);
    std::thread::spawn(move || unsafe {
        let handles: Vec<HANDLE> = roots
            .iter()
            .filter_map(|r| {
                FindFirstChangeNotificationW(&windows::core::HSTRING::from(r.as_str()), true, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE).ok()
            })
            .collect();
        if handles.is_empty() {
            return;
        }
        let mut dirty = false;
        while WATCH_GEN.load(Ordering::Relaxed) == generation {
            let r = WaitForMultipleObjects(&handles, false, if dirty { 2000 } else { 5000 });
            if r == WAIT_TIMEOUT {
                if dirty {
                    dirty = false;
                    rescan();
                }
                continue;
            }
            let i = (r.0 - WAIT_OBJECT_0.0) as usize;
            if i < handles.len() {
                dirty = true;
                let _ = FindNextChangeNotification(handles[i]);
            } else {
                std::thread::sleep(Duration::from_secs(5));
            }
        }
        for h in handles {
            let _ = FindCloseChangeNotification(h);
        }
    });
}

/// Startup: register what the index already knows, then rescan and watch.
pub fn init() {
    if folders().is_empty() {
        return;
    }
    register();
    rescan();
    watch();
}

/// Embedded cover art of a local file (JPEG / PNG bytes).
pub fn artwork(uri: &str) -> Option<Vec<u8>> {
    use symphonia::core::{formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint};
    let t = db::local_by_uri(uri)?;
    let file = std::fs::File::open(&t.path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = Path::new(&t.path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()).ok()?;
    if let Some(rev) = probed.metadata.get().as_ref().and_then(|m| m.current().cloned())
        && let Some(v) = rev.visuals().first()
    {
        return Some(v.data.to_vec());
    }
    let rev = probed.format.metadata().current().cloned()?;
    rev.visuals().first().map(|v| v.data.to_vec())
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_uri_parts() {
        assert_eq!(super::decode_part("Test+Artist%21"), "Test Artist!");
        assert_eq!(super::decode_part("caf%C3%A9"), "café");
    }
}

//! `%LOCALAPPDATA%\cadence\cadence.log`: a GUI launch has no stderr, so auth and other outcomes
//! worth diagnosing later are appended here too. Never log token values.
//! The file is rotated to `cadence.old.log` at 1 MiB, so it never grows without bound.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn path() -> PathBuf {
    crate::cache::dir("..").join("cadence.log")
}

/// One line to stderr and the log file, prefixed with the Unix time.
pub fn line(msg: &str) {
    eprintln!("{msg}");
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = path();
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(&p, p.with_file_name("cadence.old.log"));
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{secs} {msg}");
    }
}

/// `logln!("auth: refresh ok")`: format and write one log line.
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}

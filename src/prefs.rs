//! Small preferences in `%APPDATA%\cadence\settings.json` (one JSON object). Reads go through an
//! in-memory copy; every write merges one key and rewrites the file atomically, so modules that
//! own different keys never clobber each other.

use serde_json::{Map, Value};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn path() -> std::path::PathBuf {
    crate::auth::data_dir().join("settings.json")
}

fn store() -> MutexGuard<'static, Map<String, Value>> {
    static S: OnceLock<Mutex<Map<String, Value>>> = OnceLock::new();
    S.get_or_init(|| {
        let map = std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        Mutex::new(map)
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

pub fn get(key: &str) -> Value {
    store().get(key).cloned().unwrap_or(Value::Null)
}

pub fn set(key: &str, value: Value) {
    let mut s = store();
    if s.get(key) == Some(&value) {
        return;
    }
    s.insert(key.to_string(), value);
    let text = Value::Object(s.clone()).to_string();
    drop(s);
    let p = path();
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        crate::logln!("prefs: cannot write settings.json");
    }
}

pub fn bool_or(key: &str, default: bool) -> bool {
    get(key).as_bool().unwrap_or(default)
}

pub fn u64_or(key: &str, default: u64) -> u64 {
    get(key).as_u64().unwrap_or(default)
}

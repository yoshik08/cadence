//! Two sign-ins, both PKCE with a loopback redirect:
//! 1. Web API (`web.rs`) with the user's own client ID, redirect `127.0.0.1:8888/callback`.
//! 2. Streaming (librespot) with librespot's own desktop client ID, redirect `127.0.0.1:8898/login`,
//!    done once; afterwards librespot's `Cache` holds reusable credentials.

#![allow(dead_code)]

use librespot_core::{authentication::Credentials, cache::Cache};
use librespot_oauth::OAuthClientBuilder;
use std::{
    path::PathBuf,
    process::Command,
    time::Duration,
};

// librespot-core 0.8.0 config.rs `KEYMASTER_CLIENT_ID` (pub(crate), default `SessionConfig` client ID on Windows).
const STREAM_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const STREAM_REDIRECT: &str = "http://127.0.0.1:8898/login";
// Verbatim `OAUTH_SCOPES` from librespot v0.8.0 src/main.rs.
const STREAM_SCOPES: &[&str] = &[
    "app-remote-control",
    "playlist-modify",
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read",
    "playlist-read-collaborative",
    "playlist-read-private",
    "streaming",
    "ugc-image-upload",
    "user-follow-modify",
    "user-follow-read",
    "user-library-modify",
    "user-library-read",
    "user-modify",
    "user-modify-playback-state",
    "user-modify-private",
    "user-personalized",
    "user-read-birthdate",
    "user-read-currently-playing",
    "user-read-email",
    "user-read-play-history",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// `%APPDATA%\cadence`, created on first call. Debug builds: `CADENCE_DATA_DIR` replaces it, so
/// tests never touch the real folder.
pub fn data_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    let over = std::env::var_os("CADENCE_DATA_DIR").map(PathBuf::from);
    #[cfg(not(debug_assertions))]
    let over: Option<PathBuf> = None;
    let dir = over.unwrap_or_else(|| {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("cadence")
    });
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("auth: cannot create {}: {e}", dir.display());
    }
    dir
}

const CLIENT_ID_FILE: &str = "client_id.txt";

/// A Spotify Client ID: exactly 32 hex characters once whitespace (and a BOM) is trimmed.
pub fn valid_client_id(s: &str) -> Option<String> {
    let s = s.trim_start_matches('\u{feff}').trim();
    (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// The message the setup card and Settings show for an ID that isn't valid.
pub const CLIENT_ID_HINT: &str = "A Client ID is 32 letters and numbers (0-9, a-f). Copy it from your app's Settings in the Spotify Dashboard.";

/// The valid ID on the first line of `dir/client_id.txt`.
pub fn read_client_id(dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(CLIENT_ID_FILE)).ok()?;
    valid_client_id(text.lines().next()?)
}

/// Writes `id` to `dir/client_id.txt` atomically (temp file in the same folder, then rename)
/// and reads it back. The ID is never logged.
pub fn save_client_id(dir: &std::path::Path, id: &str) -> Result<String, String> {
    let id = valid_client_id(id).ok_or_else(|| CLIENT_ID_HINT.to_string())?;
    let path = dir.join(CLIENT_ID_FILE);
    let tmp = dir.join("client_id.txt.tmp");
    std::fs::write(&tmp, format!("{id}\n"))
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("Couldn't save the Client ID ({e}).")
        })?;
    match read_client_id(dir) {
        Some(back) if back == id => Ok(id),
        _ => Err("The Client ID didn't save correctly. Try again.".into()),
    }
}

/// The Client ID from `dir`, importing a valid `env` value into the file when the file has none.
pub fn client_id_in(dir: &std::path::Path, env: Option<&str>) -> Option<String> {
    if let Some(id) = read_client_id(dir) {
        return Some(id);
    }
    let id = valid_client_id(env?)?;
    match save_client_id(dir, &id) {
        Ok(_) => crate::logln!("auth: imported the Client ID from SPOTIFY_CLIENT_ID into client_id.txt"),
        Err(e) => crate::logln!("auth: Client ID import failed: {e}"),
    }
    Some(id)
}

/// `client_id.txt`, else `SPOTIFY_CLIENT_ID` (written to the file once, so the env var is never
/// needed again).
pub fn client_id() -> Option<String> {
    let env = std::env::var("SPOTIFY_CLIENT_ID").ok();
    client_id_in(&data_dir(), env.as_deref())
}

/// `1a2b…9f0e` for display.
pub fn mask_client_id(id: &str) -> String {
    if id.len() < 8 {
        return "••••".into();
    }
    format!("{}••••••••{}", &id[..4], &id[id.len() - 4..])
}

/// The two token files Sign out removes. client_id.txt is never among them.
pub fn token_files(dir: &std::path::Path) -> [PathBuf; 2] {
    [dir.join("web_token.json"), dir.join("librespot").join("credentials.json")]
}

/// Sign out: deletes only the token files.
pub fn sign_out(dir: &std::path::Path) {
    for f in token_files(dir) {
        let _ = std::fs::remove_file(f);
    }
    crate::logln!("auth: signed out (token files removed, Client ID kept)");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cadence-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn validation() {
        assert_eq!(valid_client_id(&format!("  {ID}\r\n")).as_deref(), Some(ID));
        assert_eq!(valid_client_id(&format!("\u{feff}{}", ID.to_uppercase())).as_deref(), Some(ID));
        assert!(valid_client_id("0123456789abcdef").is_none());
        assert!(valid_client_id(&format!("{ID}0")).is_none());
        assert!(valid_client_id("0123456789abcdef0123456789abcdeg").is_none());
        assert!(valid_client_id("").is_none());
        assert!(valid_client_id("0123456789abcdef 0123456789abcde").is_none());
    }

    #[test]
    fn atomic_save_and_read_back() {
        let d = temp("save");
        assert!(save_client_id(&d, "nope").is_err());
        assert!(!d.join(CLIENT_ID_FILE).exists());
        assert_eq!(save_client_id(&d, ID).as_deref(), Ok(ID));
        assert_eq!(read_client_id(&d).as_deref(), Some(ID));
        assert!(!d.join("client_id.txt.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn env_import_only_when_file_missing() {
        let d = temp("env");
        assert_eq!(client_id_in(&d, None), None);
        assert_eq!(client_id_in(&d, Some("bad")), None);
        assert!(!d.join(CLIENT_ID_FILE).exists());
        assert_eq!(client_id_in(&d, Some(ID)).as_deref(), Some(ID));
        assert_eq!(read_client_id(&d).as_deref(), Some(ID));
        // The file wins over a different env value.
        let other = "ffffffffffffffffffffffffffffffff";
        assert_eq!(client_id_in(&d, Some(other)).as_deref(), Some(ID));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sign_out_keeps_client_id() {
        let d = temp("signout");
        save_client_id(&d, ID).unwrap();
        std::fs::create_dir_all(d.join("librespot")).unwrap();
        for f in token_files(&d) {
            std::fs::write(f, "{}").unwrap();
        }
        sign_out(&d);
        assert!(token_files(&d).iter().all(|f| !f.exists()));
        assert_eq!(read_client_id(&d).as_deref(), Some(ID));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mask() {
        assert_eq!(mask_client_id(ID), "0123••••••••cdef");
    }
}

/// librespot cache: credentials and volume under `data_dir()/librespot`; audio under
/// `%LOCALAPPDATA%\cadence\cache\audio`, capped at 1 GiB (librespot prunes the oldest files).
/// A track played once is never downloaded again while it stays in the cache.
pub fn librespot_cache() -> Cache {
    let dir = data_dir().join("librespot");
    let audio = crate::cache::dir("audio");
    // Settings > Storage: 0 = 0.5 GB, else whole GB.
    let gb = crate::prefs::u64_or("cache_gb", 1);
    let limit = if gb == 0 { 512 << 20 } else { gb << 30 };
    Cache::new(Some(dir.clone()), Some(dir), Some(audio), Some(limit)).unwrap_or_else(|e| {
        eprintln!("auth: librespot cache disabled: {e}");
        Cache::new(None::<PathBuf>, None, None, None).expect("cache without paths cannot fail")
    })
}

/// Cached librespot credentials, else a one-time browser approval for streaming (port 8898).
///
/// The returned credentials are only an access token. Reusable ones get written into `cache`
/// by `Session::connect(creds, true)`, which `Spirc::new` calls itself, provided the session was
/// created with `Session::new(SessionConfig::default(), Some(cache.clone()))`.
pub async fn streaming_credentials(cache: &Cache) -> Result<Credentials, String> {
    if let Some(creds) = cache.credentials() {
        return Ok(creds);
    }
    // librespot-oauth's listener blocks on a std socket and cannot be cancelled, so run it on a
    // detached thread: a wedged login never stalls the runtime or its shutdown.
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let token = OAuthClientBuilder::new(STREAM_CLIENT_ID, STREAM_REDIRECT, STREAM_SCOPES.to_vec())
            .open_in_browser()
            .with_custom_message(&page("Cadence can now stream. You can close this tab."))
            .build()
            .and_then(|client| client.get_access_token())
            .map(|t| t.access_token)
            .map_err(|e| e.to_string());
        let _ = tx.send(token);
    });
    let token = tokio::time::timeout(LOGIN_TIMEOUT, rx)
        .await
        .map_err(|_| "streaming sign-in timed out after 5 minutes".to_owned())?
        .map_err(|_| "streaming sign-in thread ended unexpectedly".to_owned())??;
    Ok(Credentials::with_access_token(token))
}

pub fn open_browser(url: &str) -> Result<(), String> {
    // `cmd /c start` would cut the URL at `&`; rundll32 gets it as one argument.
    Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open the browser ({e}); open this URL manually: {url}"))
}

pub fn page(msg: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>Cadence</title>\
         <style>body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#000;\
         color:#f5f5f7;font:17px system-ui,'Segoe UI',sans-serif}}</style><p>{msg}</p>"
    )
}

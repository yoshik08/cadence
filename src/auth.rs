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

/// `%APPDATA%\cadence`, created on first call.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("cadence");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("auth: cannot create {}: {e}", dir.display());
    }
    dir
}

/// `SPOTIFY_CLIENT_ID`, else the first line of `data_dir()/client_id.txt`.
pub fn client_id() -> Option<String> {
    let clean = |s: &str| {
        let s = s.trim_start_matches('\u{feff}').trim();
        (!s.is_empty()).then(|| s.to_owned())
    };
    std::env::var("SPOTIFY_CLIENT_ID")
        .ok()
        .and_then(|v| clean(&v))
        .or_else(|| {
            let text = std::fs::read_to_string(data_dir().join("client_id.txt")).ok()?;
            clean(text.lines().next()?)
        })
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

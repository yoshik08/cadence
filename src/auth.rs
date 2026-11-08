//! Two sign-ins, both PKCE with a loopback redirect:
//! 1. Web API (rspotify) with the user's own client ID, redirect `127.0.0.1:8888/callback`.
//! 2. Streaming (librespot) with librespot's own desktop client ID, redirect `127.0.0.1:8898/login`,
//!    done once; afterwards librespot's `Cache` holds reusable credentials.

#![allow(dead_code)]

use librespot_core::{authentication::Credentials, cache::Cache};
use librespot_oauth::OAuthClientBuilder;
use rspotify::{AuthCodePkceSpotify, Config, Credentials as WebCreds, OAuth, prelude::*, scopes};
use std::{
    path::PathBuf,
    process::Command,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

const WEB_ADDR: &str = "127.0.0.1:8888";
const WEB_REDIRECT: &str = "http://127.0.0.1:8888/callback";
const WEB_SCOPES: &str = "playlist-read-private playlist-read-collaborative user-library-read \
                          user-read-recently-played user-read-private";

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

/// Web API client: cached token (refreshed when expired), else browser sign-in on port 8888.
/// The returned client keeps refreshing its token (and rewriting the cache) on every request.
pub async fn web_api(client_id: &str) -> Result<AuthCodePkceSpotify, String> {
    let oauth = OAuth {
        redirect_uri: WEB_REDIRECT.to_owned(),
        scopes: scopes!(WEB_SCOPES),
        ..Default::default()
    };
    let config = Config {
        token_cached: true,
        token_refreshing: true,
        cache_path: data_dir().join("web_token.json"),
        ..Default::default()
    };
    let mut spotify = AuthCodePkceSpotify::with_config(WebCreds::new_pkce(client_id), oauth, config);

    // Missing/corrupt cache or a cache missing a scope all come back as None/Err: just sign in again.
    if let Ok(Some(token)) = spotify.read_token_cache(true).await {
        let expired = token.is_expired();
        *spotify.token.lock().await.unwrap() = Some(token);
        if !expired {
            return Ok(spotify);
        }
        match spotify.refresh_token().await {
            // `refresh_token` also rewrites the cache file.
            Ok(()) if spotify.token.lock().await.unwrap().is_some() => return Ok(spotify),
            Ok(()) => eprintln!("auth: cached token has no refresh token, signing in again"),
            Err(e) => eprintln!("auth: token refresh failed ({e}), signing in again"),
        }
    }

    let url = spotify.get_authorize_url(None).map_err(|e| e.to_string())?;
    // Bind before opening the browser so the redirect cannot arrive early.
    let listener = TcpListener::bind(WEB_ADDR)
        .await
        .map_err(|e| format!("cannot listen on {WEB_ADDR}: {e}"))?;
    open_browser(&url)?;
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &spotify))
        .await
        .map_err(|_| "Spotify sign-in timed out after 5 minutes".to_owned())??;
    // Exchanges the code, stores the token and writes `web_token.json`.
    spotify.request_token(&code).await.map_err(|e| format!("token exchange: {e}"))?;
    Ok(spotify)
}

/// librespot cache: credentials and volume under `data_dir()/librespot`; no audio cache.
pub fn librespot_cache() -> Cache {
    let dir = data_dir().join("librespot");
    Cache::new(Some(dir.clone()), Some(dir), None, None).unwrap_or_else(|e| {
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

fn open_browser(url: &str) -> Result<(), String> {
    // `cmd /c start` would cut the URL at `&`; rundll32 gets it as one argument.
    Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open the browser ({e}); open this URL manually: {url}"))
}

/// Accepts connections until one is the OAuth redirect; returns its (state-checked) `code`.
async fn wait_for_code(listener: &TcpListener, spotify: &AuthCodePkceSpotify) -> Result<String, String> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let Some(target) = request_target(&mut stream).await else { continue };
        if !target.starts_with("/callback") {
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        }
        let result = if target.contains("error=") {
            Err("Spotify sign-in was declined".to_owned())
        } else {
            spotify
                .parse_response_code(&format!("http://{WEB_ADDR}{target}"))
                .ok_or_else(|| "sign-in redirect had no code or a wrong state".to_owned())
        };
        let msg = if result.is_ok() {
            "Signed in to Cadence. You can close this tab."
        } else {
            "Sign-in failed. Close this tab and try again from Cadence."
        };
        respond(&mut stream, "200 OK", &page(msg)).await;
        return result;
    }
}

/// Path+query from the request line (`GET <target> HTTP/1.1`); `None` on an idle or broken connection.
async fn request_target(stream: &mut TcpStream) -> Option<String> {
    // Browsers sometimes open speculative connections that never send anything.
    let mut line = String::new();
    let mut reader = BufReader::new(stream.take(8192));
    match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await {
        Ok(Ok(n)) if n > 0 => line.split_whitespace().nth(1).map(str::to_owned),
        _ => None,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(msg.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn page(msg: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>Cadence</title>\
         <style>body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#000;\
         color:#f5f5f7;font:17px system-ui,'Segoe UI',sans-serif}}</style><p>{msg}</p>"
    )
}

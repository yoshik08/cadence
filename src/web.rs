//! Spotify Web API client with its own token handling (replaces rspotify).
//!
//! Why our own: rspotify 0.16.1's PKCE refresh drops the stored refresh token whenever Spotify's
//! refresh response omits one, which Spotify's docs allow ("When a refresh token is not returned,
//! continue using the existing token"). The token file then held `"refresh_token": null` and
//! Cadence had to open the browser every hour.
//!
//! Rules implemented here:
//! - A refresh response without `refresh_token` keeps the previous one; a new one replaces it.
//! - Refreshes are serialized by one async mutex, so two callers never race.
//! - The token file is written atomically (temp file + rename).
//! - `authorized_at` records the original consent; refresh tokens die 6 months after it and
//!   refreshing does not extend that, so `days_left` lets the UI warn ahead of time.
//! - `400 invalid_grant`: the stored token is discarded (no retry) and the user signs in again.
//! - Outcomes go to the log file (status and error kind only, never token values).

use base64::Engine;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sha2::Digest;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::logln;

const API: &str = "https://api.spotify.com/v1/";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const ADDR: &str = "127.0.0.1:8888";
pub const REDIRECT: &str = "http://127.0.0.1:8888/callback";
/// Every scope the finished app needs, requested in one consent (verified against Spotify's
/// scopes page Nov 2026). Changing this list forces a new sign-in.
pub const SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-public",
    "playlist-modify-private",
    "ugc-image-upload",
    "user-library-read",
    "user-library-modify",
    "user-follow-read",
    "user-follow-modify",
    "user-read-recently-played",
    "user-top-read",
    "user-read-private",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-playback-position",
];
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh this long before the access token expires.
const EARLY: u64 = 120;
/// Spotify refresh tokens live 6 months from the original authorization.
const REFRESH_LIFETIME_DAYS: i64 = 182;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub access: String,
    pub refresh: Option<String>,
    /// Unix seconds.
    pub expires_at: u64,
    pub scope: String,
    /// Unix seconds of the browser consent this refresh token came from.
    pub authorized_at: u64,
}

impl Token {
    fn to_json(&self) -> String {
        json!({
            "v": 2,
            "access_token": self.access,
            "refresh_token": self.refresh,
            "expires_at": self.expires_at,
            "scope": self.scope,
            "authorized_at": self.authorized_at,
        })
        .to_string()
    }

    /// Only the v2 format: the rspotify-era file has the old scope set (and usually no refresh
    /// token), so it needs the new consent anyway.
    fn from_json(b: &[u8]) -> Option<Token> {
        let v: Value = serde_json::from_slice(b).ok()?;
        if v["v"].as_u64() != Some(2) {
            return None;
        }
        Some(Token {
            access: v["access_token"].as_str()?.to_string(),
            refresh: v["refresh_token"].as_str().map(str::to_string),
            expires_at: v["expires_at"].as_u64()?,
            scope: v["scope"].as_str().unwrap_or("").to_string(),
            authorized_at: v["authorized_at"].as_u64().unwrap_or(0),
        })
    }

    fn has_scopes(&self) -> bool {
        let have: Vec<&str> = self.scope.split_whitespace().collect();
        SCOPES.iter().all(|s| have.contains(s))
    }

    /// Applies a token-endpoint response. A missing `refresh_token` keeps the current one.
    fn merge(&self, resp: &Value, at: u64) -> Option<Token> {
        let access = resp["access_token"].as_str()?.to_string();
        let refresh = resp["refresh_token"].as_str().map(str::to_string).or_else(|| self.refresh.clone());
        let scope = resp["scope"].as_str().map_or_else(|| self.scope.clone(), str::to_string);
        Some(Token {
            access,
            refresh,
            expires_at: at + resp["expires_in"].as_u64().unwrap_or(3600),
            scope,
            authorized_at: self.authorized_at,
        })
    }
}

#[derive(Debug, Clone)]
pub enum ApiError {
    /// No usable sign-in (declined, timed out, or the refresh token died and sign-in failed).
    Auth(String),
    /// Non-2xx from the Web API.
    Status { code: u16, reason: String },
    /// Couldn't reach Spotify.
    Network(String),
    /// 2xx with a body that isn't JSON.
    Parse(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Auth(e) => write!(f, "auth: {e}"),
            ApiError::Status { code, reason } => write!(f, "HTTP {code} {reason}"),
            ApiError::Network(e) => write!(f, "request: {e}"),
            ApiError::Parse(e) => write!(f, "Unexpected response from Spotify: {e}"),
        }
    }
}

impl ApiError {
    pub fn code(&self) -> Option<u16> {
        match self {
            ApiError::Status { code, .. } => Some(*code),
            _ => None,
        }
    }
}

/// Shown when Spotify answers `invalid_client`: the saved Client ID is wrong or its app was
/// deleted. Every file is kept; the user changes the ID in Settings.
pub const INVALID_CLIENT: &str = "Spotify doesn't recognize this Client ID. Settings > Change Client ID.";

pub fn is_invalid_client(e: &ApiError) -> bool {
    matches!(e, ApiError::Auth(m) if m == INVALID_CLIENT)
}

/// The stored token lacks a scope this version asks for, so the next start asks once more.
pub fn stored_needs_consent(dir: &std::path::Path) -> bool {
    std::fs::read(dir.join("web_token.json"))
        .ok()
        .and_then(|b| Token::from_json(&b))
        .is_some_and(|t| !t.has_scopes())
}

enum RefreshError {
    /// The refresh token is dead: discard it.
    InvalidGrant,
    /// The Client ID itself is rejected: keep the token and the ID, tell the user.
    InvalidClient,
    /// Network or server trouble: keep the token and try again later.
    Transient(String),
}

pub struct Web {
    http: reqwest::Client,
    client_id: String,
    path: PathBuf,
    /// The one token; holding the lock is what serializes refreshes and sign-ins.
    token: tokio::sync::Mutex<Option<Token>>,
    /// Shown while the browser sign-in is waiting (the UI sets it).
    on_sign_in: std::sync::Mutex<Option<Box<dyn Fn(bool) + Send + Sync>>>,
}

impl Web {
    /// Loads the stored token (refreshing it if stale) or runs the browser sign-in.
    fn new(client_id: &str, path: PathBuf) -> Web {
        Web {
            http: crate::http::client().clone(),
            client_id: client_id.to_string(),
            path,
            token: tokio::sync::Mutex::new(None),
            on_sign_in: std::sync::Mutex::new(None),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Change Client ID: a browser sign-in with `client_id` whose token goes to `path` (a side
    /// file), so the current login stays until the caller swaps the files in.
    pub async fn sign_in_to(client_id: &str, path: PathBuf) -> Result<(), ApiError> {
        let web = Web::new(client_id, path);
        let t = web.sign_in().await?;
        web.save(&t);
        Ok(())
    }

    pub async fn connect(client_id: &str) -> Result<Arc<Web>, ApiError> {
        let web = Arc::new(Web::new(client_id, crate::auth::data_dir().join("web_token.json")));
        let stored = std::fs::read(&web.path).ok().and_then(|b| Token::from_json(&b));
        match &stored {
            Some(t) if !t.has_scopes() => logln!("auth: stored token lacks scopes, signing in again"),
            Some(_) => logln!("auth: stored token loaded"),
            None => logln!("auth: no stored token (or old format), signing in"),
        }
        *web.token.lock().await = stored.filter(Token::has_scopes);
        web.access().await?;
        Ok(web)
    }

    pub fn set_sign_in_notifier(&self, f: impl Fn(bool) + Send + Sync + 'static) {
        *self.on_sign_in.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
    }

    /// Days until the refresh token reaches its 6-month end (re-consent needed after).
    pub async fn days_left(&self) -> Option<i64> {
        let t = self.token.lock().await;
        let at = t.as_ref()?.authorized_at as i64;
        (at > 0).then(|| REFRESH_LIFETIME_DAYS - (now() as i64 - at) / 86_400)
    }

    #[allow(dead_code)] // Settings > Re-authorize (CHECKLIST 3.12).
    /// Forgets the token and signs in again in the browser (Settings > Re-authorize).
    /// The current token stays (in memory and on disk) until the new sign-in succeeds; the
    /// Client ID is never touched.
    pub async fn reauthorize(&self) -> Result<(), ApiError> {
        let mut t = self.token.lock().await;
        let new = self.sign_in().await?;
        self.save(&new);
        *t = Some(new);
        Ok(())
    }

    /// A valid access token: refreshes when close to expiry, signs in when there is no token or
    /// the refresh token is dead.
    async fn access(&self) -> Result<String, ApiError> {
        let mut t = self.token.lock().await;
        if let Some(tok) = t.as_ref()
            && tok.expires_at > now() + EARLY
        {
            return Ok(tok.access.clone());
        }
        if let Some(tok) = t.clone() {
            match self.refresh(&tok).await {
                Ok(new) => {
                    logln!(
                        "auth: refresh ok (refresh token {})",
                        if new.refresh != tok.refresh { "rotated" } else { "kept" }
                    );
                    self.save(&new);
                    let access = new.access.clone();
                    *t = Some(new);
                    return Ok(access);
                }
                Err(RefreshError::InvalidClient) => {
                    logln!("auth: refresh failed: invalid_client (token and Client ID kept)");
                    return Err(ApiError::Auth(INVALID_CLIENT.into()));
                }
                Err(RefreshError::InvalidGrant) => {
                    logln!("auth: refresh failed: invalid_grant, discarding token and signing in again");
                    *t = None;
                    let _ = std::fs::remove_file(&self.path);
                }
                Err(RefreshError::Transient(e)) => {
                    logln!("auth: refresh failed (transient): {e}");
                    // Still valid for a bit: use it. Otherwise report offline, keep the token.
                    if tok.expires_at > now() + 5 {
                        return Ok(tok.access.clone());
                    }
                    return Err(ApiError::Network(e));
                }
            }
        }
        let new = self.sign_in().await?;
        self.save(&new);
        let access = new.access.clone();
        *t = Some(new);
        Ok(access)
    }

    async fn refresh(&self, tok: &Token) -> Result<Token, RefreshError> {
        let Some(refresh) = tok.refresh.as_deref() else {
            return Err(RefreshError::InvalidGrant);
        };
        let at = now();
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", &self.client_id)])
            .send()
            .await
            .map_err(|e| RefreshError::Transient(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if status == StatusCode::BAD_REQUEST && body["error"].as_str() == Some("invalid_grant") {
            return Err(RefreshError::InvalidGrant);
        }
        if body["error"].as_str() == Some("invalid_client") {
            return Err(RefreshError::InvalidClient);
        }
        if !status.is_success() {
            return Err(RefreshError::Transient(format!(
                "HTTP {} {}",
                status.as_u16(),
                body["error"].as_str().unwrap_or("")
            )));
        }
        tok.merge(&body, at).ok_or_else(|| RefreshError::Transient("token response without access_token".into()))
    }

    fn save(&self, t: &Token) {
        let tmp = self.path.with_extension("json.tmp");
        let r = std::fs::write(&tmp, t.to_json()).and_then(|_| std::fs::rename(&tmp, &self.path));
        if let Err(e) = r {
            logln!("auth: cannot write token file: {e}");
        }
    }

    /// Browser PKCE sign-in on the loopback redirect.
    async fn sign_in(&self) -> Result<Token, ApiError> {
        let notify = |on: bool| {
            if let Some(f) = self.on_sign_in.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                f(on);
            }
        };
        let verifier = random_string(64);
        let state = random_string(16);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
        let url = reqwest::Url::parse_with_params(
            AUTHORIZE_URL,
            &[
                ("client_id", self.client_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", REDIRECT),
                ("code_challenge_method", "S256"),
                ("code_challenge", &challenge),
                ("state", &state),
                ("scope", &SCOPES.join(" ")),
            ],
        )
        .map_err(|e| ApiError::Auth(e.to_string()))?;
        // Bind before opening the browser so the redirect cannot arrive early.
        let listener = TcpListener::bind(ADDR).await.map_err(|e| ApiError::Auth(format!("cannot listen on {ADDR}: {e}")))?;
        crate::auth::open_browser(url.as_str()).map_err(ApiError::Auth)?;
        logln!("auth: browser sign-in started");
        notify(true);
        let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state)).await;
        notify(false);
        let code = code.map_err(|_| ApiError::Auth("Spotify sign-in timed out after 5 minutes".into()))??;
        let at = now();
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", &self.client_id),
                ("code_verifier", &verifier),
            ])
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            logln!("auth: code exchange failed: HTTP {} {}", status.as_u16(), body["error"].as_str().unwrap_or(""));
            if body["error"].as_str() == Some("invalid_client") {
                return Err(ApiError::Auth(INVALID_CLIENT.into()));
            }
            return Err(ApiError::Auth(format!("token exchange failed ({})", status.as_u16())));
        }
        let blank = Token { access: String::new(), refresh: None, expires_at: 0, scope: String::new(), authorized_at: at };
        let t = blank.merge(&body, at).ok_or_else(|| ApiError::Auth("token response without access_token".into()))?;
        logln!("auth: signed in (refresh token {})", if t.refresh.is_some() { "present" } else { "MISSING" });
        Ok(t)
    }

    /// One Web API call. An empty 2xx body comes back as `Value::Null`.
    /// - 429: waits the `Retry-After` seconds, then retries.
    /// - Network errors and 5xx: capped exponential backoff, for calls that are safe to repeat
    ///   (GET, PUT, DELETE). A POST is never repeated: it could create something twice.
    /// - 401: forces one token refresh.
    pub async fn call(&self, method: Method, path: &str, query: &[(&str, &str)], body: Option<&Value>) -> Result<Value, ApiError> {
        crate::art::debug_assert_off_ui_thread();
        let repeatable = method != Method::POST;
        let mut retries = 0;
        let mut attempt = 0;
        let mut reauthed = false;
        loop {
            let access = self.access().await?;
            let mut req = self.http.request(method.clone(), format!("{API}{path}")).bearer_auth(&access).query(query);
            req = match body {
                Some(b) => req.json(b),
                // Spotify wants a Content-Length on bodiless PUT/POST/DELETE.
                None if method != Method::GET => req.header("content-length", "0"),
                None => req,
            };
            let resp = match req.send().await {
                Ok(r) => r,
                Err(_) if repeatable && attempt < crate::http::RETRIES => {
                    tokio::time::sleep(crate::http::backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(ApiError::Network(e.to_string())),
            };
            let status = resp.status();
            if status.is_server_error() && repeatable && attempt < crate::http::RETRIES {
                tokio::time::sleep(crate::http::backoff(attempt)).await;
                attempt += 1;
                continue;
            }
            if status.is_success() {
                let bytes = resp.bytes().await.map_err(|e| ApiError::Network(e.to_string()))?;
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    return Ok(Value::Null);
                }
                return serde_json::from_slice(&bytes).map_err(|e| ApiError::Parse(e.to_string()));
            }
            if status == StatusCode::TOO_MANY_REQUESTS && retries < 3 {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(2u64);
                if wait <= crate::http::MAX_RETRY_AFTER {
                    retries += 1;
                    tokio::time::sleep(Duration::from_secs(wait.max(1))).await;
                    continue;
                }
            }
            if status == StatusCode::UNAUTHORIZED && !reauthed {
                reauthed = true;
                // Expire it locally so access() refreshes.
                if let Some(t) = self.token.lock().await.as_mut()
                    && t.access == access
                {
                    t.expires_at = 0;
                }
                continue;
            }
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            let reason = v["error"]["message"].as_str().or(v["error"]["reason"].as_str()).unwrap_or("").to_string();
            return Err(ApiError::Status { code: status.as_u16(), reason });
        }
    }

    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, ApiError> {
        self.call(Method::GET, path, query, None).await
    }

    #[allow(dead_code)] // Playlist cover upload (CHECKLIST 3.4).
    /// Raw-bytes PUT (playlist cover upload: base64 JPEG as the body).
    pub async fn put_bytes(&self, path: &str, content_type: &str, data: Vec<u8>) -> Result<(), ApiError> {
        let access = self.access().await?;
        let resp = self
            .http
            .put(format!("{API}{path}"))
            .bearer_auth(&access)
            .header("content-type", content_type)
            .body(data)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(ApiError::Status { code: resp.status().as_u16(), reason: String::new() })
        }
    }
}

fn random_string(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut buf = vec![0u8; len];
    let _ = getrandom::fill(&mut buf);
    buf.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

/// Accepts connections until one is the OAuth redirect; returns its state-checked `code`.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, ApiError> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| ApiError::Auth(e.to_string()))?;
        let Some(target) = request_target(&mut stream).await else { continue };
        if !target.starts_with("/callback") {
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        }
        let url = reqwest::Url::parse(&format!("http://{ADDR}{target}")).ok();
        let param = |k: &str| url.as_ref()?.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let result = if param("error").is_some() {
            Err(ApiError::Auth("Spotify sign-in was declined".into()))
        } else if param("state").as_deref() != Some(state) {
            Err(ApiError::Auth("sign-in redirect had a wrong state".into()))
        } else {
            param("code").ok_or_else(|| ApiError::Auth("sign-in redirect had no code".into()))
        };
        let msg = if result.is_ok() {
            "Signed in to Cadence. You can close this tab."
        } else {
            "Sign-in failed. Close this tab and try again from Cadence."
        };
        respond(&mut stream, "200 OK", &crate::auth::page(msg)).await;
        return result;
    }
}

async fn request_target(stream: &mut TcpStream) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(refresh: Option<&str>) -> Token {
        Token { access: "a".into(), refresh: refresh.map(Into::into), expires_at: 0, scope: SCOPES.join(" "), authorized_at: 100 }
    }

    #[test]
    fn refresh_without_new_refresh_token_keeps_old() {
        let t = tok(Some("old")).merge(&json!({"access_token": "b", "expires_in": 3600}), 1000).unwrap();
        assert_eq!(t.refresh.as_deref(), Some("old"));
        assert_eq!(t.access, "b");
        assert_eq!(t.expires_at, 4600);
        assert_eq!(t.authorized_at, 100, "refresh must not move the 6-month clock");
    }

    #[test]
    fn rotated_refresh_token_replaces_old() {
        let t = tok(Some("old")).merge(&json!({"access_token": "b", "refresh_token": "new"}), 0).unwrap();
        assert_eq!(t.refresh.as_deref(), Some("new"));
    }

    #[test]
    fn token_file_round_trip_and_old_format_rejected() {
        let t = tok(Some("r"));
        assert_eq!(Token::from_json(t.to_json().as_bytes()), Some(t));
        let old = br#"{"access_token":"x","expires_in":3600,"expires_at":"2026-11-10T17:26:27Z","refresh_token":null,"scope":"a"}"#;
        assert_eq!(Token::from_json(old), None);
    }

    #[test]
    fn scope_check() {
        let mut t = tok(None);
        assert!(t.has_scopes());
        t.scope = "user-read-private".into();
        assert!(!t.has_scopes());
    }
}

//! Web API reads with a disk cache, and tolerant JSON-to-data parsing.
//!
//! Spotify's Feb 2026 dev-mode changes removed fields (popularity, followers, label,
//! available_markets...) and bulk endpoints; nothing here reads those, and every field is
//! optional: a missing key reads as empty.

use crate::art::SpImage;
use crate::cache;
use crate::web::ApiError;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

pub fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

pub fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

pub fn arr(v: &Value) -> &[Value] {
    v.as_array().map_or(&[][..], |a| a.as_slice())
}

/// `a` unless it is null (new `item` field vs. old `track`).
pub fn present<'a>(a: &'a Value, b: &'a Value) -> &'a Value {
    if a.is_null() { b } else { a }
}

pub fn not_signed_in() -> ApiError {
    ApiError::Auth("not signed in to Spotify".into())
}

/// Bumped after every library or playlist edit; part of the cache key of `me/*` and
/// `playlists/*` responses, so an edited list is fetched fresh instead of served stale.
static LIBRARY_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn library_changed() {
    LIBRARY_EPOCH.fetch_add(1, Relaxed);
}

fn max_age(path: &str) -> Duration {
    const H: u64 = 3600;
    Duration::from_secs(if path.starts_with("albums/") || path == "me" {
        30 * 24 * H
    } else if path.starts_with("artists/") || path.starts_with("shows/") {
        24 * H
    } else if path.starts_with("me/player/") {
        300
    } else if path == "search" {
        6 * H
    } else {
        12 * H
    })
}

/// One GET through the disk cache (reused across launches until `max_age`).
pub async fn get(path: &str, query: &[(&str, &str)]) -> Result<Value, ApiError> {
    let web = crate::web().ok_or_else(not_signed_in)?;
    let mut parts: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
    parts.sort();
    let epoch = if path.starts_with("me/") || path.starts_with("playlists/") { LIBRARY_EPOCH.load(Relaxed) } else { 0 };
    let name = format!("{}.json", cache::key(&format!("{path}?{}#{epoch}", parts.join("&"))));
    let age = max_age(path);
    let n2 = name.clone();
    if let Some(b) = tokio::task::spawn_blocking(move || cache::read("api", &n2, Some(age))).await.ok().flatten()
        && let Ok(v) = serde_json::from_slice(&b)
    {
        return Ok(v);
    }
    let v = web.get(path, query).await?;
    let data = v.to_string();
    tokio::task::spawn_blocking(move || cache::write("api", &name, data.as_bytes()));
    Ok(v)
}

/// GET without the cache (state that must be current: liked / followed checks).
pub async fn get_fresh(path: &str, query: &[(&str, &str)]) -> Result<Value, ApiError> {
    crate::web().ok_or_else(not_signed_in)?.get(path, query).await
}

pub async fn call(method: reqwest::Method, path: &str, query: &[(&str, &str)], body: Option<&Value>) -> Result<Value, ApiError> {
    let r = crate::web().ok_or_else(not_signed_in)?.call(method, path, query, body).await;
    library_changed();
    r
}

/// Library membership for up to 40 uris per request (`/me/library/contains`).
pub async fn contains(uris: &[String]) -> Result<Vec<bool>, ApiError> {
    let mut out = Vec::with_capacity(uris.len());
    for chunk in uris.chunks(40) {
        let joined = chunk.join(",");
        let v = get_fresh("me/library/contains", &[("uris", &joined)]).await?;
        let flags: Vec<bool> = arr(&v).iter().map(|b| b.as_bool().unwrap_or(false)).collect();
        out.extend((0..chunk.len()).map(|i| flags.get(i).copied().unwrap_or(false)));
    }
    Ok(out)
}

/// Save (true) or remove (false) library items: tracks, albums, artists, shows, playlists.
pub async fn set_saved(uris: &[String], saved: bool) -> Result<(), ApiError> {
    let method = if saved { reqwest::Method::PUT } else { reqwest::Method::DELETE };
    for chunk in uris.chunks(40) {
        let joined = chunk.join(",");
        call(method.clone(), "me/library", &[("uris", &joined)], None).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Plain data.

pub fn images(j: &Value) -> Vec<SpImage> {
    arr(j)
        .iter()
        .map(|i| SpImage {
            url: s(&i["url"]).to_string(),
            width: i["width"].as_u64().map(|x| x as u32),
            height: i["height"].as_u64().map(|x| x as u32),
        })
        .filter(|i| !i.url.is_empty())
        .collect()
}

pub fn first_image(j: &Value) -> String {
    images(j).first().map(|i| i.url.clone()).unwrap_or_default()
}

pub fn mmss(ms: u64) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)
}

/// "1 hr 12 min" / "14 min" / "45 sec".
pub fn long_duration(ms: u64) -> String {
    let min = (ms + 30_000) / 60_000;
    if ms < 60_000 {
        format!("{} sec", ms / 1000)
    } else if min >= 60 {
        format!("{} hr {} min", min / 60, min % 60)
    } else {
        format!("{min} min")
    }
}

pub fn join(parts: &[&str]) -> String {
    parts.iter().copied().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
}

pub fn names(artists: &Value) -> String {
    arr(artists).iter().map(|a| s(&a["name"])).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(", ")
}

pub fn year(a: &Value) -> &str {
    s(&a["release_date"]).get(..4).unwrap_or("")
}

/// Plain text from Spotify's HTML-ish descriptions.
pub fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&quot;", "\"").replace("&#x27;", "'").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&#x2F;", "/")
}

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().collect::<String>() + c.as_str())
}

#[derive(Clone, Default, Debug)]
pub struct CardData {
    pub title: String,
    pub subtitle: String,
    pub uri: String,
    pub images: Vec<SpImage>,
    /// Art URL when there is no image list (librespot metadata, local stats).
    pub art_url: String,
    pub round: bool,
    pub liked: bool,
    pub kind: String,
    /// Albums: "album" / "single" / "compilation"; release date for sorting.
    pub album_type: String,
    pub date: String,
}

#[derive(Clone, Default, Debug)]
#[allow(dead_code)] // artist / album links for future row links
pub struct TrackData {
    pub title: String,
    pub artist: String,
    pub artist_uri: String,
    pub album: String,
    pub album_uri: String,
    pub added: String,
    pub duration_ms: u64,
    pub uri: String,
    pub explicit: bool,
    pub unavailable: bool,
    pub local: bool,
    /// Episodes: description line; tracks: unused.
    pub note: String,
    pub image: String,
}

pub fn track(t: &Value) -> TrackData {
    if s(&t["name"]).is_empty() {
        return TrackData { title: "Unavailable".into(), unavailable: true, ..Default::default() };
    }
    let uri = s(&t["uri"]).to_string();
    let mut artist = names(&t["artists"]);
    if artist.is_empty() {
        artist = s(&t["show"]["name"]).to_string();
    }
    TrackData {
        title: s(&t["name"]).to_string(),
        artist,
        artist_uri: s(&t["artists"][0]["uri"]).to_string(),
        album: s(&t["album"]["name"]).to_string(),
        album_uri: s(&t["album"]["uri"]).to_string(),
        added: String::new(),
        duration_ms: n(&t["duration_ms"]),
        local: t["is_local"].as_bool().unwrap_or(false) || uri.starts_with("spotify:local:"),
        explicit: t["explicit"].as_bool().unwrap_or(false),
        unavailable: t["is_playable"].as_bool() == Some(false),
        image: first_image(&t["album"]["images"]),
        note: String::new(),
        uri,
    }
}

pub fn episode(e: &Value) -> TrackData {
    let mut t = track(e);
    t.album = s(&e["release_date"]).to_string();
    t.artist = s(&e["show"]["name"]).to_string();
    t.note = plain(s(&e["description"])).chars().take(160).collect();
    t.image = first_image(&e["images"]);
    t
}

pub fn album_card(a: &Value) -> Option<CardData> {
    let uri = s(&a["uri"]);
    if uri.is_empty() {
        return None;
    }
    let kind = s(&a["album_type"]).to_string();
    Some(CardData {
        title: s(&a["name"]).to_string(),
        subtitle: join(&[year(a), &names(&a["artists"])]),
        uri: uri.to_string(),
        images: images(&a["images"]),
        kind: "album".into(),
        album_type: kind,
        date: s(&a["release_date"]).to_string(),
        ..Default::default()
    })
}

pub fn artist_card(a: &Value) -> Option<CardData> {
    let uri = s(&a["uri"]);
    if uri.is_empty() {
        return None;
    }
    Some(CardData {
        title: s(&a["name"]).to_string(),
        subtitle: "Artist".into(),
        uri: uri.to_string(),
        images: images(&a["images"]),
        round: true,
        kind: "artist".into(),
        ..Default::default()
    })
}

pub fn owner_name(p: &Value) -> &str {
    let o = &p["owner"];
    if s(&o["display_name"]).is_empty() { s(&o["id"]) } else { s(&o["display_name"]) }
}

pub fn playlist_card(p: &Value) -> Option<CardData> {
    let uri = s(&p["uri"]);
    if uri.is_empty() {
        return None;
    }
    Some(CardData {
        title: s(&p["name"]).to_string(),
        subtitle: format!("Playlist · {}", owner_name(p)),
        uri: uri.to_string(),
        images: images(&p["images"]),
        kind: "playlist".into(),
        ..Default::default()
    })
}

pub fn show_card(sh: &Value) -> Option<CardData> {
    let uri = s(&sh["uri"]);
    if uri.is_empty() {
        return None;
    }
    Some(CardData {
        title: s(&sh["name"]).to_string(),
        subtitle: join(&["Podcast", s(&sh["publisher"])]),
        uri: uri.to_string(),
        images: images(&sh["images"]),
        kind: "show".into(),
        ..Default::default()
    })
}

pub fn episode_card(e: &Value) -> Option<CardData> {
    let uri = s(&e["uri"]);
    if uri.is_empty() {
        return None;
    }
    Some(CardData {
        title: s(&e["name"]).to_string(),
        subtitle: join(&[s(&e["release_date"]), &long_duration(n(&e["duration_ms"]))]),
        uri: uri.to_string(),
        images: images(&e["images"]),
        kind: "episode".into(),
        ..Default::default()
    })
}

/// The kind of a Spotify uri or open.spotify.com link, normalized to a uri.
pub fn parse_link(text: &str) -> Option<String> {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("spotify:") {
        let mut p = rest.split(':');
        let kind = p.next()?;
        let id = p.next()?;
        return matches!(kind, "track" | "album" | "artist" | "playlist" | "show" | "episode")
            .then(|| format!("spotify:{kind}:{id}"))
            .filter(|_| !id.is_empty());
    }
    let rest = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://"))?;
    let rest = rest.strip_prefix("open.spotify.com/")?;
    let mut p = rest.split('/').filter(|x| !x.is_empty() && !x.starts_with("intl-"));
    let kind = p.next()?;
    let id = p.next()?.split(['?', '#']).next()?;
    (matches!(kind, "track" | "album" | "artist" | "playlist" | "show" | "episode") && !id.is_empty())
        .then(|| format!("spotify:{kind}:{id}"))
}

pub fn id_of(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or("")
}

pub fn kind_of(uri: &str) -> &str {
    if uri.ends_with(":collection") {
        return "collection";
    }
    uri.split(':').nth(1).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert_eq!(parse_link("https://open.spotify.com/track/abc?si=x").as_deref(), Some("spotify:track:abc"));
        assert_eq!(parse_link("https://open.spotify.com/intl-de/album/xyz").as_deref(), Some("spotify:album:xyz"));
        assert_eq!(parse_link("spotify:playlist:p1").as_deref(), Some("spotify:playlist:p1"));
        assert_eq!(parse_link("hello"), None);
        assert_eq!(parse_link("spotify:user:x"), None);
    }

    #[test]
    fn text() {
        assert_eq!(plain("<a href=x>Hi</a> &amp; bye"), "Hi & bye");
        assert_eq!(long_duration(72 * 60_000), "1 hr 12 min");
        assert_eq!(long_duration(5_000), "5 sec");
        assert_eq!(capitalize("single"), "Single");
    }
}

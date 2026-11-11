//! Library index: Liked Songs, saved albums, followed artists, playlists and followed podcasts,
//! mirrored from the Web API into SQLite (`db::library`). Pages and the sidebar query it on
//! demand, so a large library never sits in RAM. Synced at launch when older than 30 minutes,
//! after edits made in Cadence, and on demand.

use crate::api::{self, arr, n, s};
use crate::db::{self, LibItem};
use crate::web::ApiError;
use std::sync::atomic::{AtomicBool, Ordering};

const STALE_SECS: i64 = 30 * 60;
pub const KINDS: [&str; 5] = ["playlist", "track", "album", "artist", "show"];
static SYNCING: AtomicBool = AtomicBool::new(false);

fn track_item(x: &serde_json::Value) -> Option<LibItem> {
    let t = &x["track"];
    let uri = s(&t["uri"]);
    if uri.is_empty() {
        return None;
    }
    Some(LibItem {
        uri: uri.into(),
        kind: "track".into(),
        title: s(&t["name"]).into(),
        subtitle: api::names(&t["artists"]),
        artist: api::names(&t["artists"]),
        artist_uri: s(&t["artists"][0]["uri"]).into(),
        album: s(&t["album"]["name"]).into(),
        album_uri: s(&t["album"]["uri"]).into(),
        added_at: s(&x["added_at"]).into(),
        duration_ms: n(&t["duration_ms"]) as i64,
        explicit: t["explicit"].as_bool().unwrap_or(false),
        image: pick_small(&t["album"]["images"]),
        ..Default::default()
    })
}

/// The smallest image URL >= 64 px (sidebar / rows), else the first.
fn pick_small(imgs: &serde_json::Value) -> String {
    let list = api::images(imgs);
    crate::art::pick_url(&list, 64).unwrap_or_default()
}

async fn pages(path: &str, f: impl Fn(&serde_json::Value) -> Option<LibItem>) -> Result<Vec<LibItem>, ApiError> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let off = offset.to_string();
        let j = api::get_fresh(path, &[("limit", "50"), ("offset", &off)]).await?;
        let list = arr(&j["items"]);
        out.extend(list.iter().filter_map(&f));
        offset += list.len();
        if list.is_empty() || !j["next"].is_string() || offset >= 20_000 {
            break;
        }
    }
    Ok(out)
}

async fn fetch(kind: &str) -> Result<Vec<LibItem>, ApiError> {
    match kind {
        "track" => pages("me/tracks", track_item).await,
        "album" => {
            pages("me/albums", |x| {
                let a = &x["album"];
                let uri = s(&a["uri"]);
                (!uri.is_empty()).then(|| LibItem {
                    uri: uri.into(),
                    kind: "album".into(),
                    title: s(&a["name"]).into(),
                    subtitle: api::names(&a["artists"]),
                    artist: api::names(&a["artists"]),
                    artist_uri: s(&a["artists"][0]["uri"]).into(),
                    added_at: s(&x["added_at"]).into(),
                    image: pick_small(&a["images"]),
                    total: n(&a["total_tracks"]) as i64,
                    album: s(&a["release_date"]).into(),
                    ..Default::default()
                })
            })
            .await
        }
        "playlist" => {
            pages("me/playlists", |p| {
                let uri = s(&p["uri"]);
                (!uri.is_empty()).then(|| LibItem {
                    uri: uri.into(),
                    kind: "playlist".into(),
                    title: s(&p["name"]).into(),
                    subtitle: api::owner_name(p).into(),
                    owner_id: s(&p["owner"]["id"]).into(),
                    collaborative: p["collaborative"].as_bool().unwrap_or(false),
                    public: p["public"].as_bool().unwrap_or(false),
                    total: n(&api::present(&p["items"], &p["tracks"])["total"]) as i64,
                    image: pick_small(&p["images"]),
                    ..Default::default()
                })
            })
            .await
        }
        "show" => {
            pages("me/shows", |x| {
                let sh = &x["show"];
                let uri = s(&sh["uri"]);
                (!uri.is_empty()).then(|| LibItem {
                    uri: uri.into(),
                    kind: "show".into(),
                    title: s(&sh["name"]).into(),
                    subtitle: s(&sh["publisher"]).into(),
                    added_at: s(&x["added_at"]).into(),
                    image: pick_small(&sh["images"]),
                    total: n(&sh["total_episodes"]) as i64,
                    ..Default::default()
                })
            })
            .await
        }
        "artist" => {
            // Cursor-paged.
            let mut out = Vec::new();
            let mut after = String::new();
            loop {
                let mut q = vec![("type", "artist"), ("limit", "50")];
                if !after.is_empty() {
                    q.push(("after", after.as_str()));
                }
                let j = api::get_fresh("me/following", &q).await?;
                let list = arr(&j["artists"]["items"]);
                for a in list {
                    let uri = s(&a["uri"]);
                    if !uri.is_empty() {
                        out.push(LibItem {
                            uri: uri.into(),
                            kind: "artist".into(),
                            title: s(&a["name"]).into(),
                            subtitle: "Artist".into(),
                            image: pick_small(&a["images"]),
                            ..Default::default()
                        });
                    }
                }
                after = s(&j["artists"]["cursors"]["after"]).to_string();
                if list.is_empty() || after.is_empty() || out.len() >= 5000 {
                    break;
                }
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

/// Syncs every kind that is stale (or all with `force`). Returns the kinds that changed.
pub async fn sync(force: bool) -> Vec<&'static str> {
    if SYNCING.swap(true, Ordering::Relaxed) {
        return Vec::new();
    }
    let mut changed = Vec::new();
    for kind in KINDS {
        let fresh = db::synced_at(kind).is_some_and(|t| db::now() - t < STALE_SECS);
        if fresh && !force {
            continue;
        }
        match fetch(kind).await {
            Ok(items) => {
                let k = kind.to_string();
                let _ = tokio::task::spawn_blocking(move || db::library_replace(&k, &items)).await;
                changed.push(kind);
            }
            Err(e) => {
                crate::logln!("library: sync {kind} failed: {e}");
                if matches!(e, ApiError::Auth(_)) {
                    break;
                }
            }
        }
    }
    SYNCING.store(false, Ordering::Relaxed);
    changed
}

/// Syncs one kind now (after an edit made elsewhere or a manual refresh).
pub async fn sync_kind(kind: &'static str) {
    if let Ok(items) = fetch(kind).await {
        let _ = tokio::task::spawn_blocking(move || db::library_replace(kind, &items)).await;
    }
}

/// Records a like / save / follow done in Cadence without a full resync.
pub fn note_saved(uri: &str, saved: bool, item: Option<LibItem>) {
    let kind = api::kind_of(uri).to_string();
    let uri = uri.to_string();
    std::thread::spawn(move || {
        let it = item.map(|mut i| {
            i.kind = kind.clone();
            i.uri = uri.clone();
            i
        });
        let fallback = LibItem { uri: uri.clone(), ..Default::default() };
        db::library_set(&kind, if saved { it.as_ref().or(Some(&fallback)) } else { None }, &uri);
    });
}

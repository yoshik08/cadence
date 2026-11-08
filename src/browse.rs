//! Browse backend: Web API reads and the Slint models behind `Browse`.
//!
//! Responses are read as raw JSON with a tolerant parser instead of rspotify's typed models. After
//! Spotify's Feb 2026 changes those models still require fields that may be gone (for example
//! `external_ids` on tracks) and `SimplifiedPlaylist` / `FullPlaylist` panic when `items` is
//! missing, which aborts a release build. Requests still go through rspotify (`api_get`), so token
//! refresh stays its job.

use crate::{art, Browse, Card, MainWindow, TrackRow};
use rspotify::model::Image as SpImage;
use rspotify::{http::HttpError, prelude::*, AuthCodePkceSpotify, ClientError};
use serde_json::Value;
use slint::{ComponentHandle, Image, Model, ModelNotify, ModelTracker, VecModel, Weak};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;
use tokio::runtime::Handle;

const UNAVAILABLE: &str =
    "Spotify doesn't share the track list of playlists you don't own. Play still works.";
const PAGE_SIZE: &str = "50";

// ---------------------------------------------------------------------------------------------
// Shared state. Async results carry the PAGE value they started under and are dropped when it moved.

/// Bumped by every navigation.
static PAGE: AtomicU64 = AtomicU64::new(0);
/// Bumped by every search keystroke and navigation (debounce + stale results).
static SEARCH: AtomicU64 = AtomicU64::new(0);
/// What the cards on screen know about themselves, so opening one can show its header at once.
static META: LazyLock<Mutex<HashMap<String, Meta>>> = LazyLock::new(Default::default);
/// (user id, display name), fetched once.
static ME: Mutex<Option<(String, String)>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Playlists,
    Playlist,
    Album,
    Artist,
    Liked,
}

#[derive(Clone)]
struct Env {
    rt: Handle,
    sp: AuthCodePkceSpotify,
    play: Arc<dyn Fn(String, usize) + Send + Sync>,
    ui: Weak<MainWindow>,
}

#[derive(Clone, Default)]
struct CardData {
    title: String,
    subtitle: String,
    /// Subtitle without a track count, for the list header.
    meta: String,
    uri: String,
    images: Vec<SpImage>,
    liked: bool,
}

#[derive(Clone)]
struct Meta {
    title: String,
    meta: String,
    images: Vec<SpImage>,
}

#[derive(Default)]
struct TrackData {
    title: String,
    artist: String,
    album: String,
    duration: String,
    uri: String,
}

enum Items {
    Cards(Vec<CardData>),
    Tracks(Vec<TrackData>),
}

struct Chunk {
    items: Items,
    total: u32,
    /// Raw entries in the response (including null ones), for the next offset.
    count: u32,
    more: bool,
}

// ---------------------------------------------------------------------------------------------
// JSON access. Indexing a serde_json::Value with a missing key (or on a non-object) gives Null,
// so these read anything unexpected as empty.

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map_or(&[][..], |a| a.as_slice())
}

/// One GET. A 429 is retried up to 3 times after the `Retry-After` seconds Spotify sends (2 s if
/// the header is missing); a wait over 30 s is not worth blocking a page for, so it fails at once.
async fn get(sp: &AuthCodePkceSpotify, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
    let q: HashMap<&str, &str> = query.iter().copied().collect();
    let mut retries = 0;
    loop {
        let err = match sp.api_get(path, &q).await {
            Ok(body) => return serde_json::from_str(&body).map_err(|e| format!("Unexpected response from Spotify: {e}")),
            Err(e) => e,
        };
        if let ClientError::Http(h) = &err
            && let HttpError::StatusCode(r) = &**h
            && r.status().as_u16() == 429
            && retries < 3
        {
            let wait = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(2u64);
            if wait <= 30 {
                retries += 1;
                tokio::time::sleep(Duration::from_secs(wait.max(1))).await;
                continue;
            }
        }
        return Err(err.to_string());
    }
}

// ---------------------------------------------------------------------------------------------
// JSON to plain data.

fn join(parts: &[&str]) -> String {
    parts.iter().copied().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
}

fn names(artists: &Value) -> String {
    arr(artists).iter().map(|a| s(&a["name"])).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(", ")
}

fn images(j: &Value) -> Vec<SpImage> {
    arr(j)
        .iter()
        .map(|i| SpImage {
            url: s(&i["url"]).to_string(),
            width: i["width"].as_u64().map(|x| x as u32),
            height: i["height"].as_u64().map(|x| x as u32),
        })
        .collect()
}

fn duration(ms: u64) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)
}

fn present<'a>(a: &'a Value, b: &'a Value) -> &'a Value {
    if a.is_null() { b } else { a }
}

fn count_text(kind: Kind, total: u32) -> String {
    let (one, many) = if kind == Kind::Artist { ("release", "releases") } else { ("track", "tracks") };
    match total {
        0 => String::new(),
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

fn remember(c: CardData) -> CardData {
    lock(&META).insert(
        c.uri.clone(),
        Meta { title: c.title.clone(), meta: c.meta.clone(), images: c.images.clone() },
    );
    c
}

fn track(t: &Value) -> TrackData {
    if s(&t["name"]).is_empty() {
        return TrackData { title: "Unavailable".into(), ..Default::default() };
    }
    let mut artist = names(&t["artists"]);
    if artist.is_empty() {
        artist = s(&t["show"]["name"]).to_string();
    }
    TrackData {
        title: s(&t["name"]).to_string(),
        artist,
        album: s(&t["album"]["name"]).to_string(),
        duration: duration(n(&t["duration_ms"])),
        uri: s(&t["uri"]).to_string(),
    }
}

fn year(a: &Value) -> &str {
    s(&a["release_date"]).get(..4).unwrap_or("")
}

fn album_card(a: &Value) -> Option<CardData> {
    let uri = s(&a["uri"]);
    if uri.is_empty() {
        return None;
    }
    let meta = join(&[&names(&a["artists"]), year(a)]);
    Some(remember(CardData {
        title: s(&a["name"]).to_string(),
        subtitle: meta.clone(),
        meta,
        uri: uri.to_string(),
        images: images(&a["images"]),
        liked: false,
    }))
}

fn artist_card(a: &Value) -> Option<CardData> {
    let uri = s(&a["uri"]);
    if uri.is_empty() {
        return None;
    }
    Some(remember(CardData {
        title: s(&a["name"]).to_string(),
        subtitle: "Artist".into(),
        meta: String::new(),
        uri: uri.to_string(),
        images: images(&a["images"]),
        liked: false,
    }))
}

fn playlist_card(p: &Value, with_count: bool) -> Option<CardData> {
    let uri = s(&p["uri"]);
    if uri.is_empty() {
        return None;
    }
    let owner = &p["owner"];
    let owner = if s(&owner["display_name"]).is_empty() { s(&owner["id"]) } else { s(&owner["display_name"]) };
    let total = n(&present(&p["items"], &p["tracks"])["total"]) as u32;
    let count = if with_count { count_text(Kind::Playlist, total) } else { String::new() };
    Some(remember(CardData {
        title: s(&p["name"]).to_string(),
        subtitle: join(&[owner, &count]),
        meta: owner.to_string(),
        uri: uri.to_string(),
        images: images(&p["images"]),
        liked: false,
    }))
}

/// One release on an artist page, shown as a row that opens the album.
fn album_row(a: &Value) -> Option<TrackData> {
    let c = album_card(a)?;
    let mut label = s(&a["album_type"]).to_string();
    if let Some(first) = label.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    Some(TrackData { title: c.title, artist: label, album: String::new(), duration: year(a).to_string(), uri: c.uri })
}

// ---------------------------------------------------------------------------------------------
// Requests.

async fn ensure_me(sp: &AuthCodePkceSpotify) -> Result<(String, String), String> {
    let cached = lock(&ME).clone();
    if let Some(me) = cached {
        return Ok(me);
    }
    let j = get(sp, "me", &[]).await?;
    if s(&j["id"]).is_empty() {
        return Err("Couldn't read your Spotify profile".into());
    }
    let me = (s(&j["id"]).to_string(), s(&j["display_name"]).to_string());
    *lock(&ME) = Some(me.clone());
    Ok(me)
}

/// One page (50) of a paged list: my playlists, a playlist's items, an album's tracks, an
/// artist's albums or Liked Songs. `/playlists/{id}/items` is the post-Feb 2026 path; its
/// entries carry `item` (older responses `track`).
async fn fetch_page(sp: &AuthCodePkceSpotify, kind: Kind, id: &str, offset: u32) -> Result<Chunk, String> {
    let off = offset.to_string();
    let mut q = vec![("limit", PAGE_SIZE), ("offset", off.as_str())];
    let path = match kind {
        Kind::Playlists => "me/playlists".to_string(),
        Kind::Playlist => format!("playlists/{id}/items"),
        Kind::Album => format!("albums/{id}/tracks"),
        Kind::Artist => {
            q.push(("include_groups", "album,single"));
            format!("artists/{id}/albums")
        }
        Kind::Liked => "me/tracks".to_string(),
    };
    let j = get(sp, &path, &q).await?;
    let list = arr(&j["items"]);
    let items = match kind {
        Kind::Playlists => Items::Cards(list.iter().filter_map(|p| playlist_card(p, true)).collect()),
        Kind::Playlist => Items::Tracks(list.iter().map(|x| track(present(&x["item"], &x["track"]))).collect()),
        Kind::Album => Items::Tracks(list.iter().map(track).collect()),
        Kind::Artist => Items::Tracks(list.iter().filter_map(album_row).collect()),
        Kind::Liked => Items::Tracks(list.iter().map(|x| track(&x["track"])).collect()),
    };
    Ok(Chunk {
        items,
        total: n(&j["total"]) as u32,
        count: list.len() as u32,
        more: j["next"].is_string(),
    })
}

/// Recently played (one request, the API's maximum of 50 plays), collapsed to distinct albums.
async fn fetch_recent(sp: &AuthCodePkceSpotify) -> Result<Vec<CardData>, String> {
    let j = get(sp, "me/player/recently-played", &[("limit", PAGE_SIZE)]).await?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for it in arr(&j["items"]) {
        let album = &it["track"]["album"];
        if seen.insert(s(&album["uri"]).to_string()) {
            out.extend(album_card(album));
        }
    }
    Ok(out)
}

struct Found {
    tracks: Vec<TrackData>,
    artists: Vec<CardData>,
    albums: Vec<CardData>,
    playlists: Vec<CardData>,
}

async fn fetch_search(sp: &AuthCodePkceSpotify, q: &str) -> Result<Found, String> {
    let j = get(sp, "search", &[("q", q), ("type", "track,artist,album,playlist"), ("limit", "10")]).await?;
    let list = |k: &str| arr(&j[k]["items"]);
    Ok(Found {
        tracks: list("tracks").iter().map(track).filter(|t| !t.uri.is_empty()).collect(),
        artists: list("artists").iter().filter_map(artist_card).collect(),
        albums: list("albums").iter().filter_map(album_card).collect(),
        playlists: list("playlists").iter().filter_map(|p| playlist_card(p, false)).collect(),
    })
}

// ---------------------------------------------------------------------------------------------
// Slint models (UI thread only).

const RECENT: usize = 0;
const ARTISTS: usize = 1;
const ALBUMS: usize = 2;
const FOUND_PLAYLISTS: usize = 3;
/// Card art size in logical px: Home is compact, Search shelves are larger.
const SHELF_ART: [f32; 4] = [120.0, 160.0, 160.0, 160.0];

struct LazyRow {
    card: Card,
    images: Vec<SpImage>,
    asked: Cell<bool>,
}

/// The Home playlist list. Art is requested the first time a row is asked for, which happens
/// only for rows the ListView instantiates; the result is written back with `row_changed`.
struct Lazy {
    rows: RefCell<Vec<LazyRow>>,
    notify: ModelNotify,
    rt: Handle,
    ui: Weak<MainWindow>,
}

impl Lazy {
    fn extend(&self, new: Vec<(Card, Vec<SpImage>)>) {
        let (start, n) = (self.rows.borrow().len(), new.len());
        self.rows
            .borrow_mut()
            .extend(new.into_iter().map(|(card, images)| LazyRow { card, images, asked: Cell::new(false) }));
        self.notify.row_added(start, n);
    }
    fn clear(&self) {
        self.rows.borrow_mut().clear();
        self.notify.reset();
    }
    fn set_art(&self, i: usize, uri: &str, img: Image) {
        let hit = match self.rows.borrow_mut().get_mut(i) {
            Some(r) if r.card.uri.as_str() == uri => {
                r.card.art = img;
                true
            }
            _ => false,
        };
        if hit {
            self.notify.row_changed(i);
        }
    }
}

impl Model for Lazy {
    type Data = Card;
    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }
    fn row_data(&self, i: usize) -> Option<Card> {
        let rows = self.rows.borrow();
        let r = rows.get(i)?;
        if !r.asked.replace(true) && !r.images.is_empty() {
            let px = self.ui.upgrade().map_or(56.0, |u| 56.0 * u.window().scale_factor()).round() as u32;
            if let Some(url) = art::pick_url(&r.images, px) {
                let (uri, ui) = (r.card.uri.to_string(), self.ui.clone());
                self.rt.spawn(async move {
                    let Some(a) = art::load(&url, px).await else { return };
                    let _ = ui.upgrade_in_event_loop(move |_| {
                        with_models(|m| m.playlists.set_art(i, &uri, Image::from_rgb8(a.buf)));
                    });
                });
            }
        }
        Some(r.card.clone())
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Models {
    shelves: [Rc<VecModel<Card>>; 4],
    tracks: Rc<VecModel<TrackRow>>,
    found: Rc<VecModel<TrackRow>>,
    playlists: Rc<Lazy>,
}

thread_local! {
    static MODELS: RefCell<Option<Models>> = const { RefCell::new(None) };
}

fn with_models<R>(f: impl FnOnce(&Models) -> R) -> R {
    MODELS.with(|m| f(m.borrow().as_ref().expect("browse::init not called")))
}

fn card(c: &CardData) -> Card {
    Card {
        title: c.title.as_str().into(),
        subtitle: c.subtitle.as_str().into(),
        uri: c.uri.as_str().into(),
        art: Image::default(),
        liked: c.liked,
    }
}

fn row(t: &TrackData) -> TrackRow {
    TrackRow {
        title: t.title.as_str().into(),
        artist: t.artist.as_str().into(),
        album: t.album.as_str().into(),
        duration: t.duration.as_str().into(),
        uri: t.uri.as_str().into(),
    }
}

/// Decodes art for `images` at `logical` px (scaled to the window's DPI) and hands the finished
/// `Image` to `apply` on the UI thread, unless the page has changed meanwhile.
fn art_task(
    env: &Env,
    ui: &MainWindow,
    images: &[SpImage],
    logical: f32,
    g: u64,
    apply: impl FnOnce(&MainWindow, Image) + Send + 'static,
) {
    let px = (logical * ui.window().scale_factor()).round() as u32;
    let Some(url) = art::pick_url(images, px) else { return };
    let weak = env.ui.clone();
    env.rt.spawn(async move {
        let Some(a) = art::load(&url, px).await else { return };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) == g {
                apply(&ui, Image::from_rgb8(a.buf));
            }
        });
    });
}

fn fill_shelf(env: &Env, ui: &MainWindow, g: u64, shelf: usize, cards: &[CardData]) {
    with_models(|m| m.shelves[shelf].set_vec(cards.iter().map(card).collect::<Vec<_>>()));
    for c in cards {
        let uri = c.uri.clone();
        art_task(env, ui, &c.images, SHELF_ART[shelf], g, move |_, img| {
            with_models(|m| {
                let model = &m.shelves[shelf];
                if let Some(i) = (0..model.row_count()).find(|&i| model.row_data(i).is_some_and(|r| r.uri.as_str() == uri)) {
                    let mut r = model.row_data(i).unwrap();
                    r.art = img;
                    model.set_row_data(i, r);
                }
            });
        });
    }
}

fn append(items: &Items) {
    with_models(|m| match items {
        Items::Cards(cards) => m.playlists.extend(cards.iter().map(|c| (card(c), c.images.clone())).collect()),
        Items::Tracks(rows) => m.tracks.extend(rows.iter().map(row)),
    });
}

// ---------------------------------------------------------------------------------------------
// Navigation.

/// A short sentence for the error line; the raw error goes to stderr.
fn friendly(raw: &str) -> &'static str {
    if raw.contains("429") {
        "Spotify is limiting requests. Try again in a moment."
    } else if raw.contains("401") || raw.contains("403") {
        "Spotify didn't allow this request."
    } else if raw.contains("request:") || raw.contains("error sending") || raw.contains("timed out") {
        "Can't reach Spotify. Check your connection."
    } else {
        "Something went wrong loading this page."
    }
}

fn show_error(b: &Browse, raw: &str) {
    eprintln!("browse: {raw}");
    b.set_error(friendly(raw).into());
}

/// Ends the loading state of a page: no dimming, no status line.
fn finish(b: &Browse) {
    b.set_loading(false);
    b.set_status("".into());
}

/// Starts a new page: invalidates pending work, empties every model and resets the header state.
/// `status` is the line shown until the first data arrives.
fn new_page(ui: &MainWindow, page: &str, status: &str) -> u64 {
    let g = PAGE.fetch_add(1, Relaxed) + 1;
    SEARCH.fetch_add(1, Relaxed);
    with_models(|m| {
        m.shelves.iter().for_each(|s| s.set_vec(Vec::new()));
        m.tracks.set_vec(Vec::new());
        m.found.set_vec(Vec::new());
        m.playlists.clear();
    });
    let b = ui.global::<Browse>();
    b.set_page(page.into());
    b.set_flip(!b.get_flip());
    b.set_loading(true);
    b.set_status(status.into());
    b.set_error("".into());
    b.set_search_note("".into());
    b.set_list_kind("".into());
    b.set_list_uri("".into());
    b.set_list_title("".into());
    b.set_list_meta("".into());
    b.set_list_note("".into());
    b.set_list_art(Image::default());
    g
}

/// Fetches the remaining pages of a list one after another (`offset` entries are already shown)
/// and appends each as it arrives, so the list fills progressively. Stops when `next` runs out,
/// on an error, or as soon as the user navigates away. List pages show "Loaded N of M" meanwhile.
fn stream_rest(env: &Env, g: u64, kind: Kind, id: String, offset: u32, total: u32) {
    let (e, weak) = (env.clone(), env.ui.clone());
    let (mut offset, mut total) = (offset, total);
    env.rt.spawn(async move {
        loop {
            if PAGE.load(Relaxed) != g {
                return;
            }
            let res = fetch_page(&e.sp, kind, &id, offset).await;
            let more = res.as_ref().is_ok_and(|c| c.more && c.count > 0);
            if let Ok(c) = &res {
                offset += c.count;
                total = total.max(c.total).max(offset);
            }
            let (loaded, of) = (offset, total);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if PAGE.load(Relaxed) != g {
                    return;
                }
                let b = ui.global::<Browse>();
                match res {
                    Ok(c) => append(&c.items),
                    Err(x) => show_error(&b, &x),
                }
                let status = if more && kind != Kind::Playlists { format!("Loaded {loaded} of {of}") } else { String::new() };
                b.set_status(status.into());
            });
            if !more {
                return;
            }
        }
    });
}

fn go_home(env: &Env, ui: &MainWindow) {
    let g = new_page(ui, "home", "Loading your library…");
    lock(&META).clear();
    let (e, weak) = (env.clone(), env.ui.clone());
    env.rt.spawn(async move {
        let (me, recent, playlists) =
            tokio::join!(ensure_me(&e.sp), fetch_recent(&e.sp), fetch_page(&e.sp, Kind::Playlists, "", 0));
        let _ = weak.upgrade_in_event_loop(move |ui| apply_home(&e, &ui, g, me, recent, playlists));
    });
}

fn apply_home(
    e: &Env,
    ui: &MainWindow,
    g: u64,
    me: Result<(String, String), String>,
    recent: Result<Vec<CardData>, String>,
    playlists: Result<Chunk, String>,
) {
    if PAGE.load(Relaxed) != g {
        return;
    }
    let b = ui.global::<Browse>();
    finish(&b);
    let mut error = None;
    match recent {
        Ok(cards) => fill_shelf(e, ui, g, RECENT, &cards),
        Err(x) => error = Some(x),
    }
    let mut rows = Vec::new();
    match me {
        Ok((id, _)) => rows.push(remember(CardData {
            title: "Liked Songs".into(),
            subtitle: "Saved tracks".into(),
            uri: format!("spotify:user:{id}:collection"),
            liked: true,
            ..Default::default()
        })),
        Err(x) => error = error.or(Some(x)),
    }
    match playlists {
        Ok(chunk) => {
            if chunk.more && chunk.count > 0 {
                stream_rest(e, g, Kind::Playlists, String::new(), chunk.count, chunk.total);
            }
            if let Items::Cards(cards) = chunk.items {
                rows.extend(cards);
            }
        }
        Err(x) => error = error.or(Some(x)),
    }
    append(&Items::Cards(rows));
    if let Some(x) = error {
        show_error(&b, &x);
    }
}

fn go_search(ui: &MainWindow) {
    new_page(ui, "search", "");
    lock(&META).clear();
    finish(&ui.global::<Browse>());
}

fn search(env: &Env, ui: &MainWindow, text: &str) {
    let s = SEARCH.fetch_add(1, Relaxed) + 1;
    let g = PAGE.load(Relaxed);
    let q = text.trim().to_string();
    let b = ui.global::<Browse>();
    if q.is_empty() {
        with_models(|m| {
            m.shelves[ARTISTS..].iter().for_each(|s| s.set_vec(Vec::new()));
            m.found.set_vec(Vec::new());
        });
        b.set_search_note("".into());
        b.set_error("".into());
        finish(&b);
        return;
    }
    b.set_loading(true);
    b.set_status("Searching…".into());
    let (e, weak) = (env.clone(), env.ui.clone());
    env.rt.spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if SEARCH.load(Relaxed) != s {
            return;
        }
        let res = fetch_search(&e.sp, &q).await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if SEARCH.load(Relaxed) != s || PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            finish(&b);
            match res {
                Ok(f) => {
                    b.set_error("".into());
                    with_models(|m| m.found.set_vec(f.tracks.iter().map(row).collect::<Vec<_>>()));
                    fill_shelf(&e, &ui, g, ARTISTS, &f.artists);
                    fill_shelf(&e, &ui, g, ALBUMS, &f.albums);
                    fill_shelf(&e, &ui, g, FOUND_PLAYLISTS, &f.playlists);
                    let none = f.tracks.is_empty() && f.artists.is_empty() && f.albums.is_empty() && f.playlists.is_empty();
                    b.set_search_note(if none { format!("No results for \u{201c}{q}\u{201d}").into() } else { "".into() });
                }
                Err(x) => show_error(&b, &x),
            }
        });
    });
}

/// `spotify:playlist:ID` / `album` / `artist`, or Liked Songs (`liked` or `spotify:user:ID:collection`).
fn parse_uri(uri: &str) -> Option<(Kind, String)> {
    if uri == "liked" || uri.ends_with(":collection") {
        return Some((Kind::Liked, String::new()));
    }
    let mut p = uri.split(':');
    if p.next()? != "spotify" {
        return None;
    }
    let kind = match p.next()? {
        "playlist" => Kind::Playlist,
        "album" => Kind::Album,
        "artist" => Kind::Artist,
        _ => return None,
    };
    Some((kind, p.next()?.to_string()))
}

fn open(env: &Env, ui: &MainWindow, uri: &str) {
    let Some((kind, id)) = parse_uri(uri) else { return };
    let meta = lock(&META).get(uri).cloned();
    let status = match kind {
        Kind::Playlist => "Loading playlist…",
        Kind::Album => "Loading album…",
        Kind::Artist => "Loading releases…",
        _ => "Loading your liked songs…",
    };
    let g = new_page(ui, "list", status);

    let (kind_name, fallback) = match kind {
        Kind::Playlist => ("playlist", "Playlist"),
        Kind::Album => ("album", "Album"),
        Kind::Artist => ("artist", "Artist"),
        _ => ("liked", "Liked Songs"),
    };
    let b = ui.global::<Browse>();
    b.set_list_kind(kind_name.into());
    if kind != Kind::Liked {
        b.set_list_uri(uri.into());
    }
    b.set_list_title(meta.as_ref().map_or(fallback, |m| m.title.as_str()).into());
    let base = meta.as_ref().map(|m| m.meta.clone()).unwrap_or_default();
    b.set_list_meta(base.as_str().into());
    if let Some(m) = &meta {
        art_task(env, ui, &m.images, 232.0, g, |ui, img| {
            let b = ui.global::<Browse>();
            b.set_list_art(img);
        });
    }

    let (e, weak) = (env.clone(), env.ui.clone());
    env.rt.spawn(async move {
        let (me, res) = tokio::join!(
            async {
                if kind == Kind::Liked { ensure_me(&e.sp).await } else { Ok(Default::default()) }
            },
            fetch_page(&e.sp, kind, &id, 0)
        );
        let _ = weak.upgrade_in_event_loop(move |ui| apply_open(&e, &ui, g, kind, id, base, me, res));
    });
}

fn apply_open(
    e: &Env,
    ui: &MainWindow,
    g: u64,
    kind: Kind,
    id: String,
    base: String,
    me: Result<(String, String), String>,
    res: Result<Chunk, String>,
) {
    if PAGE.load(Relaxed) != g {
        return;
    }
    let b = ui.global::<Browse>();
    finish(&b);
    let mut base = base;
    let mut error = None;
    if kind == Kind::Liked {
        match me {
            Ok((id, name)) => {
                b.set_list_uri(format!("spotify:user:{id}:collection").into());
                base = name;
            }
            Err(x) => error = Some(x),
        }
    }
    match res {
        Ok(chunk) => {
            b.set_list_meta(join(&[&base, &count_text(kind, chunk.total)]).into());
            if kind == Kind::Playlist && chunk.count == 0 {
                b.set_list_note(UNAVAILABLE.into());
            }
            if chunk.more && chunk.count > 0 {
                b.set_status(format!("Loaded {} of {}", chunk.count, chunk.total.max(chunk.count)).into());
                stream_rest(e, g, kind, id, chunk.count, chunk.total);
            }
            append(&chunk.items);
        }
        Err(x) => {
            // Spotify refuses the items of playlists the user does not own.
            if kind == Kind::Playlist && (x.contains("403") || x.contains("404")) {
                b.set_list_note(UNAVAILABLE.into());
            } else {
                error = error.or(Some(x));
            }
        }
    }
    if let Some(x) = error {
        show_error(&b, &x);
    }
}

pub fn init(
    ui: &crate::MainWindow,
    rt: tokio::runtime::Handle,
    spotify: rspotify::AuthCodePkceSpotify,
    play: Arc<dyn Fn(String, usize) + Send + Sync>,
) {
    let env = Env { rt: rt.clone(), sp: spotify, play, ui: ui.as_weak() };
    let models = Models {
        shelves: std::array::from_fn(|_| Rc::new(VecModel::default())),
        tracks: Rc::new(VecModel::default()),
        found: Rc::new(VecModel::default()),
        playlists: Rc::new(Lazy { rows: RefCell::new(Vec::new()), notify: ModelNotify::default(), rt, ui: ui.as_weak() }),
    };
    let b = ui.global::<Browse>();
    b.set_recent(models.shelves[RECENT].clone().into());
    b.set_found_artists(models.shelves[ARTISTS].clone().into());
    b.set_found_albums(models.shelves[ALBUMS].clone().into());
    b.set_found_playlists(models.shelves[FOUND_PLAYLISTS].clone().into());
    b.set_found_tracks(models.found.clone().into());
    b.set_tracks(models.tracks.clone().into());
    b.set_playlists(models.playlists.clone().into());
    MODELS.with(|m| *m.borrow_mut() = Some(models));

    let (e, w) = (env.clone(), ui.as_weak());
    b.on_open(move |uri| {
        if let Some(ui) = w.upgrade() {
            open(&e, &ui, &uri);
        }
    });
    let (e, w) = (env.clone(), ui.as_weak());
    b.on_search(move |text| {
        if let Some(ui) = w.upgrade() {
            search(&e, &ui, &text);
        }
    });
    let e = env.clone();
    b.on_play(move |uri, index| {
        if !uri.is_empty() {
            (e.play)(uri.to_string(), index.max(0) as usize);
        }
    });
    let (e, w) = (env.clone(), ui.as_weak());
    b.on_go_home(move || {
        if let Some(ui) = w.upgrade() {
            go_home(&e, &ui);
        }
    });
    let w = ui.as_weak();
    b.on_go_search(move || {
        if let Some(ui) = w.upgrade() {
            go_search(&ui);
        }
    });

    go_home(&env, ui);
}

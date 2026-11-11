//! Browse: the router (back / forward with scroll restoration), every page, and the sidebar.
//!
//! Pages: home, search, library, list (playlist / album / Liked Songs / podcast / local files /
//! local folder / "show all" / an artist's liked songs), artist, stats, profile, settings.
//! Async results carry the PAGE value they started under and are dropped when it moved, so a
//! slow response never lands on the wrong page. Page data is freed on navigation.

use crate::api::{self, CardData, Source, TrackData, arr, n, s};
use crate::art::{self, Priority, SpImage};
use crate::db;
use crate::{Browse, Card, MainWindow, Playback, ShelfData, SideItem, StatRow, Song};
use reqwest::Method;
use serde_json::{Value, json};
use slint::{ComponentHandle, Image, Model, ModelNotify, ModelRc, ModelTracker, SharedString, VecModel, Weak};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

const UNAVAILABLE: &str = "Spotify doesn't share the track list of playlists you don't own or collaborate on. Play still works.";

/// Bumped by every navigation.
static PAGE: AtomicU64 = AtomicU64::new(0);
/// `f32` bits of the window scale factor (1.0 until the window reports one).
static SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f80_0000);
/// Bumped by every search / find keystroke (debounce + stale results).
static SEARCH: AtomicU64 = AtomicU64::new(0);
static FIND: AtomicU64 = AtomicU64::new(0);
/// Titles seen on cards this session (queue panel "Next from", menus).
static TITLES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
/// Playlists whose items Spotify returned (owned or collaborative).
static READABLE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
/// (user id, display name).
static ME: Mutex<Option<(String, String)>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn known_title(uri: &str) -> Option<String> {
    lock(&TITLES).get(uri).cloned().filter(|t| !t.is_empty())
}

fn remember(c: &CardData) {
    let mut t = lock(&TITLES);
    if t.len() > 2000 {
        t.clear();
    }
    t.insert(c.uri.clone(), c.title.clone());
}

pub fn is_readable(playlist_uri: &str) -> bool {
    lock(&READABLE).contains(playlist_uri)
}

pub async fn ensure_me() -> Result<(String, String), String> {
    me_from(Source::Network).await
}

async fn me_from(src: Source) -> Result<(String, String), String> {
    if let Some(me) = lock(&ME).clone() {
        return Ok(me);
    }
    let j = api::get_from(src, "me", &[]).await.map_err(|e| e.to_string())?;
    if s(&j["id"]).is_empty() {
        return Err("Couldn't read your Spotify profile".into());
    }
    let me = (s(&j["id"]).to_string(), s(&j["display_name"]).to_string());
    *lock(&ME) = Some(me.clone());
    Ok(me)
}

/// No Spotify sign-in and none on its way. While the launch sign-in is still running, pages
/// show what is cached instead of a "sign in" note.
fn signed_out() -> bool {
    crate::web().is_none() && !crate::web_pending()
}

pub fn my_id() -> Option<String> {
    lock(&ME).as_ref().map(|m| m.0.clone())
}

// ---------------------------------------------------------------------------------------------
// Models (UI thread).

/// A row that shows art: cards and songs.
trait ArtRow: Clone + PartialEq + 'static {
    fn key(&self) -> &str;
    fn art(&self) -> &Image;
    fn set_art(&mut self, img: Image, fresh: bool);
}

impl ArtRow for Card {
    fn key(&self) -> &str {
        self.uri.as_str()
    }
    fn art(&self) -> &Image {
        &self.art
    }
    fn set_art(&mut self, img: Image, fresh: bool) {
        self.art = img;
        self.fresh = fresh;
    }
}

impl ArtRow for Song {
    fn key(&self) -> &str {
        self.uri.as_str()
    }
    fn art(&self) -> &Image {
        &self.art
    }
    fn set_art(&mut self, img: Image, fresh: bool) {
        self.art = img;
        self.fresh = fresh;
    }
}

/// One row of a `LazyArt` list: the row, the art URL chosen for its slot, and whether its art
/// was already asked for.
struct Slot<T> {
    row: T,
    url: String,
    asked: Cell<bool>,
}

/// A list for virtualized views (library rows, card grids, song tables). Art is requested when a
/// row is first shown, a couple of screens ahead of the scroll position at lower priority, and
/// dropped again for rows far from the viewport, so memory follows what is on screen, not the
/// list length. Reading rows from Rust (`peek`, `update_all`) never requests art.
struct LazyArt<T: ArtRow> {
    rows: RefCell<Vec<Slot<T>>>,
    notify: ModelNotify,
    /// The slot width in logical pixels.
    px: Cell<f32>,
    me: std::rc::Weak<LazyArt<T>>,
    /// First row of the window last handled by `window` (scroll events repeat a lot).
    seen_first: Cell<usize>,
}

type LazyCards = LazyArt<Card>;
type LazySongs = LazyArt<Song>;

fn slot<T>((row, url): (T, String)) -> Slot<T> {
    Slot { row, url, asked: Cell::new(false) }
}

impl<T: ArtRow> LazyArt<T> {
    fn new(px: f32) -> Rc<LazyArt<T>> {
        Rc::new_cyclic(|me| LazyArt {
            rows: RefCell::new(Vec::new()),
            notify: ModelNotify::default(),
            px: Cell::new(px),
            me: me.clone(),
            seen_first: Cell::new(usize::MAX),
        })
    }

    fn set(&self, rows: Vec<(T, String)>) {
        *self.rows.borrow_mut() = rows.into_iter().map(slot).collect();
        self.seen_first.set(usize::MAX);
        self.notify.reset();
    }

    fn len(&self) -> usize {
        self.rows.borrow().len()
    }

    fn extend(&self, rows: impl IntoIterator<Item = (T, String)>) {
        let start = self.len();
        self.rows.borrow_mut().extend(rows.into_iter().map(slot));
        let added = self.len() - start;
        if added > 0 {
            self.notify.row_added(start, added);
        }
    }

    /// Row `i` without requesting its art.
    fn peek(&self, i: usize) -> Option<T> {
        self.rows.borrow().get(i).map(|s| s.row.clone())
    }

    /// Every row, without requesting art.
    fn rows(&self) -> Vec<T> {
        self.rows.borrow().iter().map(|s| s.row.clone()).collect()
    }

    /// Changes rows in place; views hear about the rows `f` reports as changed.
    fn update_all(&self, mut f: impl FnMut(&mut T) -> bool) {
        let changed: Vec<usize> = self.rows.borrow_mut().iter_mut().enumerate().filter_map(|(i, s)| f(&mut s.row).then_some(i)).collect();
        for i in changed {
            self.notify.row_changed(i);
        }
    }

    /// Replaces the rows, touching as little as possible: a row with the same key and art URL
    /// keeps its art, unchanged rows are not reported, a longer list with the same start is
    /// appended to, and only a real change of shape resets the view. Scroll position survives.
    fn patch(&self, rows: Vec<(T, String)>) {
        let mut new: Vec<Slot<T>> = Vec::with_capacity(rows.len());
        {
            let old = self.rows.borrow();
            let mut by_key: HashMap<(&str, &str), &Slot<T>> = HashMap::with_capacity(old.len());
            for s in old.iter() {
                by_key.entry((s.row.key(), s.url.as_str())).or_insert(s);
            }
            for (mut row, url) in rows {
                let asked = match by_key.get(&(row.key(), url.as_str())) {
                    Some(prev) if prev.asked.get() => {
                        let (img, fresh) = (prev.row.art().clone(), false);
                        row.set_art(img, fresh);
                        true
                    }
                    _ => false,
                };
                new.push(Slot { row, url, asked: Cell::new(asked) });
            }
        }
        let old_len = self.len();
        let same_start = {
            let old = self.rows.borrow();
            old.iter().zip(&new).all(|(a, b)| a.row == b.row)
        };
        if new.len() >= old_len && same_start {
            let tail: Vec<Slot<T>> = new.into_iter().skip(old_len).collect();
            let added = tail.len();
            self.rows.borrow_mut().extend(tail);
            if added > 0 {
                self.notify.row_added(old_len, added);
            }
        } else if new.len() == old_len {
            let changed: Vec<usize> = {
                let mut old = self.rows.borrow_mut();
                let mut changed = Vec::new();
                for (i, (o, n)) in old.iter_mut().zip(new).enumerate() {
                    if o.row != n.row || o.url != n.url {
                        *o = n;
                        changed.push(i);
                    }
                }
                changed
            };
            for i in changed {
                self.notify.row_changed(i);
            }
        } else {
            *self.rows.borrow_mut() = new;
            self.seen_first.set(usize::MAX);
            self.notify.reset();
        }
    }

    fn set_art(&self, i: usize, key: &str, img: Image, fresh: bool) {
        let hit = match self.rows.borrow_mut().get_mut(i) {
            Some(s) if s.row.key() == key => {
                s.row.set_art(img, fresh);
                true
            }
            _ => false,
        };
        if hit {
            self.notify.row_changed(i);
        }
    }

    /// The request for row `i` was dropped: let the row ask again when it is next shown.
    fn forget(&self, i: usize, key: &str) {
        if let Some(s) = self.rows.borrow().get(i)
            && s.row.key() == key
        {
            s.asked.set(false);
        }
    }

    /// Makes sure row `i` has its art or a request for it. Art already in memory is put on the
    /// row right here, so a cached row never shows the placeholder.
    fn want(&self, i: usize, priority: Priority) {
        let px = art::physical(self.px.get(), scale());
        let (key, url) = {
            let rows = self.rows.borrow();
            let Some(s) = rows.get(i) else { return };
            if s.asked.replace(true) || s.url.is_empty() {
                return;
            }
            (s.row.key().to_string(), s.url.clone())
        };
        if let Some(a) = art::cached(&url, px) {
            if let Some(s) = self.rows.borrow_mut().get_mut(i) {
                s.row.set_art(Image::from_rgb8(a.buf), false);
            }
            if priority == Priority::Prefetch {
                self.notify.row_changed(i);
            }
            return;
        }
        let (me, g) = (SendWeak(self.me.clone()), PAGE.load(Relaxed));
        art::request(url, px, priority, g, move |got| {
            let _ = slint::invoke_from_event_loop(move || {
                let me = me;
                let Some(model) = me.0.upgrade() else { return };
                if PAGE.load(Relaxed) != g {
                    return;
                }
                match got {
                    Some((a, fresh)) => model.set_art(i, &key, Image::from_rgb8(a.buf), fresh),
                    None => model.forget(i, &key),
                }
            });
        });
    }

    /// Rows `first..last` are on screen: prefetch the next two screens, and let go of the art of
    /// rows more than a few screens away (it comes back from the memory or disk cache).
    fn window(&self, first: usize, last: usize) {
        if self.seen_first.replace(first) == first {
            return;
        }
        let (len, screen) = (self.len(), last.saturating_sub(first).max(1));
        for i in last..(last + 2 * screen).min(len) {
            self.want(i, Priority::Prefetch);
        }
        let keep = first.saturating_sub(2 * screen)..(last + 3 * screen).min(len);
        let mut rows = self.rows.borrow_mut();
        for (i, s) in rows.iter_mut().enumerate() {
            if !keep.contains(&i) && s.asked.get() {
                s.row.set_art(Image::default(), false);
                s.asked.set(false);
            }
        }
    }
}

impl<T: ArtRow> Model for LazyArt<T> {
    type Data = T;
    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }
    fn row_data(&self, i: usize) -> Option<T> {
        self.want(i, Priority::Visible);
        self.rows.borrow().get(i).map(|s| s.row.clone())
    }
    fn set_row_data(&self, i: usize, data: T) {
        if let Some(s) = self.rows.borrow_mut().get_mut(i) {
            s.row = data;
        } else {
            return;
        }
        self.notify.row_changed(i);
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Models {
    shelves: Rc<VecModel<ShelfData>>,
    shelf_cards: RefCell<Vec<Rc<VecModel<Card>>>>,
    found_tracks: Rc<LazySongs>,
    found_shelves: Rc<VecModel<ShelfData>>,
    found_cards: RefCell<Vec<Rc<VecModel<Card>>>>,
    tracks: Rc<LazySongs>,
    list_cards: Rc<LazyCards>,
    lib: Rc<LazyCards>,
    /// The art URL each home / search shelf card was loaded from (to tell a changed cover).
    card_urls: RefCell<HashMap<String, String>>,
    side: Rc<VecModel<SideItem>>,
    popular: Rc<LazySongs>,
    disc: Rc<VecModel<Card>>,
    find_tracks: Rc<LazySongs>,
    find_cards: Rc<VecModel<Card>>,
    history: Rc<VecModel<SharedString>>,
}

/// Data behind the current page, in its original order (sorting, duplicates, CSV, play lists).
#[derive(Default)]
struct PageData {
    rows: Vec<TrackData>,
    /// Indices into `rows`, in display order (after sort and explicit filter).
    order: Vec<usize>,
    popular: Vec<TrackData>,
    disc_all: Vec<CardData>,
    find: Vec<TrackData>,
    find_cards: Vec<CardData>,
    /// Next page to fetch for "Load more": (path, offset, kind).
    more: Option<(String, usize, &'static str)>,
    /// Playlist owner id / collaborative flag / public flag (for menus).
    owner_id: String,
    collaborative: bool,
    public: bool,
    title: String,
    description: String,
    artist_uri: String,
    total: usize,
    total_ms: u64,
    /// Where the list is being read from, and, while a refresh runs behind rows already on
    /// screen, the refreshed rows: they replace `rows` in one step when the refresh is complete,
    /// so the list never shrinks or jumps while it reloads.
    src: Option<Source>,
    refreshing: bool,
    fresh: Vec<TrackData>,
}

thread_local! {
    static MODELS: RefCell<Option<Models>> = const { RefCell::new(None) };
    static DATA: RefCell<PageData> = RefCell::new(PageData::default());
    static UI: RefCell<Option<Weak<MainWindow>>> = const { RefCell::new(None) };
    static NAV: RefCell<(Vec<(String, f32)>, usize)> = const { RefCell::new((Vec::new(), 0)) };
}

fn with_models<R>(f: impl FnOnce(&Models) -> R) -> R {
    MODELS.with(|m| f(m.borrow().as_ref().expect("browse::init not called")))
}

fn data<R>(f: impl FnOnce(&mut PageData) -> R) -> R {
    DATA.with(|d| f(&mut d.borrow_mut()))
}

fn ui() -> Option<MainWindow> {
    UI.with(|u| u.borrow().as_ref().and_then(|w| w.upgrade()))
}

fn weak() -> Weak<MainWindow> {
    UI.with(|u| u.borrow().clone().expect("browse::init not called"))
}

/// The window scale factor (UI thread). Also remembered for `scale_factor`.
fn scale() -> f32 {
    let f = ui().map_or(1.0, |u| u.window().scale_factor());
    SCALE.store(f.to_bits(), Relaxed);
    f
}

/// The last scale factor seen on the UI thread, for code running elsewhere.
fn scale_factor() -> f32 {
    f32::from_bits(SCALE.load(Relaxed))
}

fn card(c: &CardData) -> Card {
    remember(c);
    Card {
        title: c.title.as_str().into(),
        subtitle: c.subtitle.as_str().into(),
        uri: c.uri.as_str().into(),
        art: Image::default(),
        liked: c.liked,
        round: c.round,
        pinned: false,
        local: c.kind == "local-folder" || c.kind == "local",
        fresh: false,
    }
}

fn row(t: &TrackData, index: usize) -> Song {
    Song {
        title: t.title.as_str().into(),
        artist: t.artist.as_str().into(),
        album: t.album.as_str().into(),
        added: t.added.as_str().into(),
        duration: if t.duration_ms > 0 { api::mmss(t.duration_ms).into() } else { "".into() },
        uri: t.uri.as_str().into(),
        explicit: t.explicit,
        liked: false,
        liked_known: false,
        index: index as i32,
        unavailable: t.unavailable,
        local: t.local,
        marked: false,
        progress: 0.0,
        note: t.note.as_str().into(),
        art: Image::default(),
        fresh: false,
    }
}

/// The search Top result's cover, logical pixels (ui/browse.slint TopResult).
const TOP_ART: f32 = 96.0;

/// Cover sizes of the song rows, in logical pixels (ui/track_row.slint).
const TABLE_ART: f32 = 40.0;
const LIST_ART: f32 = 48.0;

/// The art URL for a song row whose cover is `logical` pixels wide.
fn song_art(t: &TrackData, logical: f32) -> String {
    art::pick_url(&t.images, logical, scale()).unwrap_or_default()
}

/// A song row with its art URL, for a `LazySongs` list.
fn song(t: &TrackData, index: usize, logical: f32) -> (Song, String) {
    (row(t, index), song_art(t, logical))
}

/// The two cover sizes the library index keeps for an item (Part 1).
fn lib_images(l: &db::LibItem) -> Vec<SpImage> {
    let sized = |url: &str, w: u32| (!url.is_empty()).then(|| SpImage { url: url.to_string(), width: Some(w), height: Some(w) });
    sized(&l.image, 64).into_iter().chain(sized(&l.image_lg, 300)).collect()
}

fn image_url(c: &CardData, logical: f32) -> Option<String> {
    art::pick_url(&c.images, logical, scale()).or_else(|| (!c.art_url.is_empty()).then(|| c.art_url.clone()))
}

/// Loads art for the cards of `model` that have none yet (shelves: a few dozen cards at most).
/// Art in memory is set at once; the rest is requested and fades in when it arrives.
fn load_cards_art(model: &Rc<VecModel<Card>>, cards: &[CardData], logical: f32, g: u64) {
    let px = art::physical(logical, scale());
    for (i, c) in cards.iter().enumerate() {
        let Some(url) = image_url(c, logical) else { continue };
        let Some(row) = model.row_data(i).filter(|r| r.uri.as_str() == c.uri) else { continue };
        if row.art.size().width > 0 {
            continue;
        }
        with_models(|m| m.card_urls.borrow_mut().insert(c.uri.clone(), url.clone()));
        if let Some(a) = art::cached(&url, px) {
            model.set_row_data(i, Card { art: Image::from_rgb8(a.buf), fresh: false, ..row });
            continue;
        }
        let uri = c.uri.clone();
        let m = SendWeak(Rc::downgrade(model));
        art::request(url, px, Priority::Visible, g, move |got| {
            let Some((a, fresh)) = got else { return };
            let _ = slint::invoke_from_event_loop(move || {
                let m = m;
                if PAGE.load(Relaxed) != g {
                    return;
                }
                let Some(model) = m.0.upgrade() else { return };
                if let Some(i) = (0..model.row_count()).find(|&i| model.row_data(i).is_some_and(|r| r.uri.as_str() == uri)) {
                    let mut r = model.row_data(i).unwrap();
                    r.art = Image::from_rgb8(a.buf);
                    r.fresh = fresh;
                    model.set_row_data(i, r);
                }
            });
        });
    }
}

/// `invoke_from_event_loop` needs `Send`; the weak model is only touched back on the UI thread.
struct SendWeak<T>(std::rc::Weak<T>);
unsafe impl<T> Send for SendWeak<T> {}

/// Art for one slot (page header, top result). `apply` gets the image, its accent colour and
/// whether it should fade in. In order of preference: the right size from memory (at once);
/// else any smaller copy from memory right away, then the right size with a fade.
fn art_into(images: &[SpImage], url: &str, logical: f32, g: u64, apply: impl Fn(&MainWindow, Image, (u8, u8, u8), bool) + Send + 'static) {
    let px = art::physical(logical, scale());
    let Some(url) = art::pick_url(images, logical, scale()).or_else(|| (!url.is_empty()).then(|| url.to_string())) else { return };
    let Some(ui) = ui() else { return };
    if let Some(a) = art::cached(&url, px) {
        return apply(&ui, Image::from_rgb8(a.buf), a.accent, false);
    }
    if let Some(a) = art::cached_any(images) {
        apply(&ui, Image::from_rgb8(a.buf), a.accent, false);
    }
    let w = weak();
    crate::runtime().spawn(async move {
        let Some((a, _)) = art::load_fresh(&url, px).await else { return };
        let _ = w.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) == g {
                // Not in memory a moment ago, so it replaces a placeholder or a smaller copy.
                apply(&ui, Image::from_rgb8(a.buf), a.accent, true);
            }
        });
    });
}

/// Sets the page header art (list and artist pages).
fn set_list_art(ui: &MainWindow, img: Image, fresh: bool) {
    let b = ui.global::<Browse>();
    b.set_list_art_fresh(fresh);
    b.set_list_art(img);
}

/// Card rows for `cards`, keeping the art `model` already shows for the same items.
fn card_rows(model: &VecModel<Card>, cards: &[CardData]) -> Vec<Card> {
    let old: HashMap<String, Image> = model.iter().filter(|c| c.art.size().width > 0).map(|c| (c.uri.to_string(), c.art)).collect();
    cards
        .iter()
        .map(|c| {
            let mut r = card(c);
            if let Some(img) = old.get(&c.uri) {
                r.art = img.clone();
            }
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Errors and page state.

fn friendly(e: &str) -> &'static str {
    if e.contains("429") {
        "Spotify is limiting requests. Try again in a moment."
    } else if e.contains("auth:") {
        "Sign in to Spotify to see this (Settings > Re-authorize)."
    } else if e.contains("401") || e.contains("403") {
        "Spotify didn't allow this request."
    } else if e.contains("404") {
        "Spotify couldn't find this."
    } else if e.contains("request:") || e.contains("error sending") || e.contains("timed out") {
        "Can't reach Spotify. Check your connection."
    } else {
        "Something went wrong loading this page."
    }
}

fn show_error(b: &Browse, raw: &str) {
    crate::logln!("browse: {raw}");
    b.set_error(friendly(raw).into());
}

fn finish(b: &Browse) {
    b.set_loading(false);
    b.set_status("".into());
}

/// Starts a page: invalidates pending work, frees the previous page's models and data.
fn new_page(ui: &MainWindow, page: &str, status: &str) -> u64 {
    let g = PAGE.fetch_add(1, Relaxed) + 1;
    art::set_page(g);
    SEARCH.fetch_add(1, Relaxed);
    FIND.fetch_add(1, Relaxed);
    with_models(|m| {
        m.tracks.set(Vec::new());
        m.list_cards.set(Vec::new());
        m.popular.set(Vec::new());
        m.disc.set_vec(Vec::new());
        m.find_tracks.set(Vec::new());
        m.find_cards.set_vec(Vec::new());
        m.found_tracks.set(Vec::new());
        m.found_shelves.set_vec(Vec::new());
        m.found_cards.borrow_mut().clear();
        m.lib.set(Vec::new());
        if page != "home" {
            m.shelves.set_vec(Vec::new());
            m.shelf_cards.borrow_mut().clear();
            m.card_urls.borrow_mut().clear();
        }
    });
    data(|d| *d = PageData::default());
    let b = ui.global::<Browse>();
    b.set_page(page.into());
    b.set_flip(!b.get_flip());
    b.set_loading(true);
    b.set_status(status.into());
    b.set_error("".into());
    b.set_search_note("".into());
    b.set_list_kind("".into());
    b.set_list_uri("".into());
    b.set_list_type("".into());
    b.set_list_title("".into());
    b.set_list_desc("".into());
    b.set_list_meta("".into());
    b.set_list_owner("".into());
    b.set_list_owner_uri("".into());
    b.set_list_note("".into());
    b.set_list_art_fresh(false);
    b.set_list_art(Image::default());
    b.set_list_round(false);
    b.set_list_editable(false);
    b.set_list_saved(false);
    b.set_list_can_save(false);
    b.set_list_playable(true);
    b.set_list_more(false);
    b.set_sort_key("".into());
    b.set_sort_desc(false);
    b.set_find_note("".into());
    b.set_find_expanded("".into());
    b.set_find_open(false);
    b.set_popular_note("".into());
    b.set_popular_title("Popular".into());
    b.set_liked_note("".into());
    b.set_following(false);
    b.set_disc_filter("all".into());
    b.set_has_top(false);
    b.set_sel_row(-1);
    // Artist and album pages show the Now Playing panel with lyrics (unless the queue is open).
    if page == "artist" {
        ui.global::<Playback>().set_side_open(true);
    }
    g
}

// ---------------------------------------------------------------------------------------------
// Router.

/// Navigates to a route: "home", "search", "library", "stats", "profile", "settings",
/// "showall:<kind>:<query>", "local", "folder:<id>", "artist-liked:<uri>", or a Spotify uri.
pub fn go(route: &str) {
    let Some(ui) = ui() else { return };
    let scroll = ui.global::<Browse>().get_scroll_y();
    NAV.with(|n| {
        let mut n = n.borrow_mut();
        let pos = n.1;
        if let Some(cur) = n.0.get_mut(pos) {
            cur.1 = scroll;
            if cur.0 == route {
                return;
            }
        }
        let keep = (pos + 1).min(n.0.len());
        n.0.truncate(keep);
        n.0.push((route.to_string(), 0.0));
        if n.0.len() > 50 {
            n.0.remove(0);
        }
        n.1 = n.0.len() - 1;
    });
    show(&ui, route, 0.0);
}

fn step(delta: i32) {
    let Some(ui) = ui() else { return };
    let scroll = ui.global::<Browse>().get_scroll_y();
    let target = NAV.with(|n| {
        let mut n = n.borrow_mut();
        let pos = n.1;
        if let Some(cur) = n.0.get_mut(pos) {
            cur.1 = scroll;
        }
        let next = pos as i32 + delta;
        if next < 0 || next as usize >= n.0.len() {
            return None;
        }
        n.1 = next as usize;
        Some(n.0[n.1].clone())
    });
    if let Some((route, y)) = target {
        show(&ui, &route, y);
    }
}

pub fn back() {
    step(-1);
}

pub fn forward() {
    step(1);
}

fn update_nav(b: &Browse) {
    let (back, fwd) = NAV.with(|n| {
        let n = n.borrow();
        (n.1 > 0, n.1 + 1 < n.0.len())
    });
    b.set_can_back(back);
    b.set_can_forward(fwd);
}

/// Restores the scroll position once the page has content (after the first data lands).
fn restore_scroll(y: f32) {
    if y >= 0.0 {
        return;
    }
    let w = weak();
    slint::Timer::single_shot(Duration::from_millis(60), move || {
        if let Some(ui) = w.upgrade() {
            ui.global::<Browse>().set_scroll_y(y);
        }
    });
}

fn show(ui: &MainWindow, route: &str, scroll: f32) {
    ui.global::<Playback>().set_expanded(false);
    let b = ui.global::<Browse>();
    let r = route.to_string();
    match route {
        "home" => go_home(ui),
        "search" => go_search(ui),
        "library" => go_library(ui),
        "stats" => go_stats(ui),
        "profile" => go_profile(ui),
        "settings" => {
            new_page(ui, "settings", "");
            finish(&b);
            crate::settings::refresh(ui);
        }
        "local" => open_local(ui),
        _ if r.starts_with("showall:") => open_showall(ui, &r),
        _ if r.starts_with("folder:") => open_folder(ui, &r),
        _ if r.starts_with("artist-liked:") => open_artist_liked(ui, &r["artist-liked:".len()..]),
        _ if r.starts_with("spotify:artist:") => open_artist(ui, &r),
        _ if r.starts_with("spotify:track:") || r.starts_with("spotify:episode:") => {
            // A pasted track / episode link plays; its album / show opens.
            crate::queue::forget_restore();
            crate::with_player(|p| p.play(&r, 0));
            go_home(ui);
        }
        _ => open_list(ui, &r),
    }
    b.set_scroll_y(0.0);
    restore_scroll(scroll);
    update_nav(&b);
}

/// Opens a pasted / typed Spotify link or uri. Returns false when it isn't one.
pub fn open_link(text: &str) -> bool {
    match api::parse_link(text) {
        Some(uri) => {
            go(&uri);
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Home.

const HOME_SECTIONS: [(&str, &str); 7] = [
    ("recent", "Recently played"),
    ("playlists", "Your playlists"),
    ("liked", "Liked Songs"),
    ("albums", "Your albums"),
    ("releases", "New from artists you follow"),
    ("shows", "Your podcasts"),
    ("mostplayed", "Most played in Cadence"),
];

/// Section order and hidden flags from settings.
fn home_layout() -> Vec<(String, bool)> {
    let saved = crate::prefs::get("home_sections");
    let mut out: Vec<(String, bool)> = arr(&saved)
        .iter()
        .filter_map(|v| {
            let k = s(&v["key"]);
            HOME_SECTIONS.iter().any(|(key, _)| *key == k).then(|| (k.to_string(), v["hidden"].as_bool().unwrap_or(false)))
        })
        .collect();
    for (k, _) in HOME_SECTIONS {
        if !out.iter().any(|(x, _)| x == k) {
            out.push((k.to_string(), false));
        }
    }
    out
}

fn save_home_layout(l: &[(String, bool)]) {
    crate::prefs::set("home_sections", Value::Array(l.iter().map(|(k, h)| json!({ "key": k, "hidden": h })).collect()));
}

fn greeting(name: &str) -> String {
    let hour = local_hour();
    let part = if hour < 5 { "Good evening" } else if hour < 12 { "Good morning" } else if hour < 18 { "Good afternoon" } else { "Good evening" };
    if name.is_empty() { part.to_string() } else { format!("{part}, {}", name.split_whitespace().next().unwrap_or(name)) }
}

fn local_hour() -> u32 {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    unsafe { GetLocalTime().wHour as u32 }
}

/// Replaces the rows of `model` with `rows`, touching as little as possible: unchanged rows are
/// left alone, a longer list with the same start is appended to, and only a real change resets
/// the model. Views keep their scroll position and nothing flickers.
fn patch_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let old = model.row_count();
    let same_start = (0..old.min(rows.len())).all(|i| model.row_data(i).as_ref() == Some(&rows[i]));
    if rows.len() >= old && same_start {
        model.extend(rows.into_iter().skip(old));
    } else if rows.len() == old {
        for (i, r) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&r) {
                model.set_row_data(i, r);
            }
        }
    } else {
        model.set_vec(rows);
    }
}

/// The cards of one shelf, keeping the art of cards that are still there with the same cover.
fn shelf_rows(old: &HashMap<String, Image>, cards: &[CardData], logical: f32) -> Vec<Card> {
    cards
        .iter()
        .map(|c| {
            let mut r = card(c);
            let same_cover = with_models(|m| m.card_urls.borrow().get(&c.uri).cloned()) == image_url(c, logical);
            if same_cover && let Some(img) = old.get(&c.uri) {
                r.art = img.clone();
            }
            r
        })
        .collect()
}

fn set_shelves(ui: &MainWindow, filled: &HashMap<String, (Vec<CardData>, String, String)>, g: u64) {
    let loading = filled.is_empty() && !signed_out();
    // Once loaded, sections with nothing to show are left out (hidden ones stay listed for Customize).
    let layout: Vec<(String, bool)> = home_layout()
        .into_iter()
        .filter(|(k, hidden)| *hidden || loading || filled.get(k).is_some_and(|f| !f.0.is_empty() || !f.1.is_empty()))
        .collect();
    let size = |key: &str| if key == "recent" { 140.0 } else { 160.0 };
    // Art already on screen, by card: a refresh keeps it instead of loading it again.
    let old: HashMap<String, Image> = with_models(|m| {
        m.shelf_cards.borrow().iter().flat_map(|model| model.iter()).filter(|c| c.art.size().width > 0).map(|c| (c.uri.to_string(), c.art)).collect()
    });
    let mut rows = Vec::new();
    let mut all_cards = Vec::new();
    for (key, hidden) in &layout {
        let title = HOME_SECTIONS.iter().find(|(k, _)| k == key).map_or("", |(_, t)| t);
        let (cards, note, more) = filled.get(key).cloned().unwrap_or_default();
        rows.push((key.clone(), title, *hidden, note, more));
        all_cards.push(cards);
    }
    // Same sections as now: patch each shelf in place. The page keeps its scroll position.
    let same = with_models(|m| {
        m.shelf_cards.borrow().len() == rows.len()
            && m.shelves.row_count() == rows.len()
            && m.shelves.iter().zip(&rows).all(|(cur, new)| {
                cur.key.as_str() == new.0 && cur.hidden == new.2 && cur.note.as_str() == new.3 && cur.more.as_str() == new.4
            })
    });
    if same {
        let models: Vec<Rc<VecModel<Card>>> = with_models(|m| m.shelf_cards.borrow().clone());
        for ((model, cards), row) in models.iter().zip(&all_cards).zip(&rows) {
            patch_model(model, shelf_rows(&old, cards, size(&row.0)));
            load_cards_art(model, cards, size(&row.0), g);
        }
    } else {
        let mut shelves = Vec::new();
        let mut models = Vec::new();
        for (row, cards) in rows.iter().zip(&all_cards) {
            let model = Rc::new(VecModel::from(shelf_rows(&old, cards, size(&row.0))));
            shelves.push(ShelfData {
                key: row.0.as_str().into(),
                title: row.1.into(),
                round: row.0 == "artists",
                cards: model.clone().into(),
                more: row.4.as_str().into(),
                note: row.3.as_str().into(),
                hidden: row.2,
            });
            models.push(model);
        }
        with_models(|m| {
            m.shelves.set_vec(shelves);
            *m.shelf_cards.borrow_mut() = models.clone();
        });
        for ((model, cards), row) in models.iter().zip(&all_cards).zip(&rows) {
            load_cards_art(model, cards, size(&row.0), g);
        }
    }
    let _ = ui;
}

fn go_home(ui: &MainWindow) {
    let g = new_page(ui, "home", "");
    let b = ui.global::<Browse>();
    b.set_skeleton(true);
    b.set_greeting(greeting(lock(&ME).as_ref().map_or("", |m| m.1.as_str())).into());
    // Empty shelves with titles while loading.
    set_shelves(ui, &HashMap::new(), g);
    if signed_out() {
        finish(&b);
        b.set_skeleton(false);
        b.set_status("Add your Spotify client ID and sign in to load your library (see README).".into());
        load_home_local(g);
        return;
    }
    refresh_home(g, true);
}

struct HomeData {
    name: String,
    filled: HashMap<String, (Vec<CardData>, String, String)>,
    error: Option<String>,
}

/// Home, cache first: the shelves are built from what is on disk (API cache + library index)
/// and shown at once, then built again from the network and patched in place.
fn refresh_home(g: u64, cached_first: bool) {
    let w = weak();
    crate::runtime().spawn(async move {
        if cached_first {
            let h = home_data(Source::Cache).await;
            // Nothing on disk yet (first run): the skeleton stays until the network answers.
            if h.filled.values().any(|f| !f.0.is_empty()) {
                apply_home(&w, h, g, false);
            }
        }
        if crate::web().is_some() {
            let h = home_data(Source::Network).await;
            apply_home(&w, h, g, true);
        }
    });
}

fn apply_home(w: &Weak<MainWindow>, h: HomeData, g: u64, done: bool) {
    let _ = w.upgrade_in_event_loop(move |ui| {
        if PAGE.load(Relaxed) != g {
            return;
        }
        let b = ui.global::<Browse>();
        if done {
            finish(&b);
        }
        let cards: usize = h.filled.values().map(|f| f.0.len()).sum();
        crate::logln!("launch: home {} at +{} ms ({cards} cards)", if done { "refreshed" } else { "shown from cache" }, crate::since_start_ms());
        b.set_skeleton(false);
        b.set_greeting(greeting(&h.name).into());
        // Background tint from the first recently played cover.
        if let Some(first) = h.filled.get("recent").and_then(|f| f.0.first()) {
            art_into(&first.images, &first.art_url, 64.0, g, |ui, _, (r, gr, bl), _| {
                ui.global::<Browse>().set_home_tint(slint::Color::from_rgb_u8(r, gr, bl));
            });
        }
        set_shelves(&ui, &h.filled, g);
        if done && let Some(e) = h.error {
            show_error(&b, &e);
        }
    });
}

async fn home_data(src: Source) -> HomeData {
    let me = me_from(src).await;
    let name = me.as_ref().map(|m| m.1.clone()).unwrap_or_default();
    let mut filled: HashMap<String, (Vec<CardData>, String, String)> = HashMap::new();
    let layout = home_layout();
    let want = |k: &str| layout.iter().any(|(x, h)| x == k && !h);
    let mut error = None;
    // Cards from the library index use the larger cover URL (the shelves are 140 / 160 px).
    let lib_px = art::physical(160.0, scale_factor());

    if want("recent") {
        match api::get_from(src, "me/player/recently-played", &[("limit", "50")]).await {
            Ok(j) => {
                let mut seen = HashSet::new();
                let mut cards = Vec::new();
                for it in arr(&j["items"]) {
                    // The context (playlist / album) if any, else the album.
                    let ctx = s(&it["context"]["uri"]);
                    let album = &it["track"]["album"];
                    let key = if ctx.starts_with("spotify:album:") || ctx.is_empty() { s(&album["uri"]).to_string() } else { ctx.to_string() };
                    if !seen.insert(key.clone()) {
                        continue;
                    }
                    if key.starts_with("spotify:album:") {
                        cards.extend(api::album_card(album));
                    } else if let Some(t) = known_title(&key).or_else(|| db::library_get(&key).map(|l| l.title)) {
                        let img = db::library_get(&key).map(|l| l.art_url(lib_px).to_string()).unwrap_or_default();
                        cards.push(CardData { title: t, subtitle: "Playlist".into(), uri: key, art_url: img, kind: "playlist".into(), ..Default::default() });
                    } else {
                        cards.extend(api::album_card(album));
                    }
                    if cards.len() >= 20 {
                        break;
                    }
                }
                filled.insert("recent".into(), (cards, String::new(), String::new()));
            }
            Err(e) if src == Source::Network => error = Some(e.to_string()),
            Err(_) => {}
        }
    }
    // Library-backed sections come from the SQLite index (synced at launch).
    if src == Source::Network {
        let _ = crate::library::sync(false).await;
    }
    let lib = |kind: &str, n: usize| db::library_list(kind, "", "recent", n);
    let img = |l: &db::LibItem| l.art_url(lib_px).to_string();
    if want("playlists") {
        let mut cards: Vec<CardData> = vec![CardData { title: "Liked Songs".into(), subtitle: format!("{} songs", db::library_count("track")), uri: "liked".into(), liked: true, kind: "collection".into(), ..Default::default() }];
        cards.extend(lib("playlist", 20).into_iter().map(|l| CardData { art_url: img(&l), title: l.title, subtitle: format!("Playlist · {}", l.subtitle), uri: l.uri, kind: "playlist".into(), ..Default::default() }));
        filled.insert("playlists".into(), (cards, String::new(), "library".into()));
    }
    if want("liked") {
        let cards: Vec<CardData> = lib("track", 16)
            .into_iter()
            .map(|l| CardData { art_url: img(&l), title: l.title, subtitle: l.artist, uri: l.uri, kind: "track".into(), ..Default::default() })
            .collect();
        let note = if cards.is_empty() { "Songs you like show up here.".to_string() } else { String::new() };
        filled.insert("liked".into(), (cards, note, "liked".into()));
    }
    if want("albums") {
        let cards: Vec<CardData> = lib("album", 20).into_iter().map(|l| CardData { art_url: img(&l), title: l.title, subtitle: l.subtitle, uri: l.uri, kind: "album".into(), ..Default::default() }).collect();
        filled.insert("albums".into(), (cards, String::new(), "library".into()));
    }
    if want("shows") {
        let cards: Vec<CardData> = lib("show", 20).into_iter().map(|l| CardData { art_url: img(&l), title: l.title, subtitle: l.subtitle, uri: l.uri, kind: "show".into(), ..Default::default() }).collect();
        if !cards.is_empty() {
            filled.insert("shows".into(), (cards, String::new(), String::new()));
        }
    }
    if want("releases") {
        filled.insert("releases".into(), (new_releases(src).await, String::new(), String::new()));
    }
    if want("mostplayed") {
        filled.insert("mostplayed".into(), most_played_cards().await);
    }
    HomeData { name, filled, error }
}

/// Home without Spotify sign-in: only Cadence's own data.
fn load_home_local(g: u64) {
    let w = weak();
    crate::runtime().spawn(async move {
        let mut filled = HashMap::new();
        filled.insert("mostplayed".to_string(), most_played_cards().await);
        let _ = w.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) == g {
                set_shelves(&ui, &filled, g);
            }
        });
    });
}

/// Releases from followed artists in the last 120 days, newest first (artist albums endpoint;
/// Spotify has no batch form of it, so the requests run four at a time).
async fn new_releases(src: Source) -> Vec<CardData> {
    static LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let artists = db::library_list("artist", "", "recent", 25);
    let cutoff = days_ago_date(120);
    let mut tasks = tokio::task::JoinSet::new();
    for a in artists {
        let cutoff = cutoff.clone();
        tasks.spawn(async move {
            let _permit = LIMIT.acquire().await.ok()?;
            let id = api::id_of(&a.uri).to_string();
            let j = api::get_from(src, &format!("artists/{id}/albums"), &[("include_groups", "album,single"), ("limit", "10")]).await.ok()?;
            Some(arr(&j["items"]).iter().filter(|al| s(&al["release_date"]) >= cutoff.as_str()).filter_map(api::album_card).collect::<Vec<_>>())
        });
    }
    let mut out = Vec::new();
    while let Some(done) = tasks.join_next().await {
        out.extend(done.ok().flatten().unwrap_or_default());
    }
    out.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.uri.cmp(&b.uri)));
    let mut seen = HashSet::new();
    out.retain(|c| seen.insert(c.uri.clone()));
    out.truncate(20);
    out
}

fn days_ago_date(days: i64) -> String {
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = (db::now() - days * 86_400) / 86_400 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

async fn most_played_cards() -> (Vec<CardData>, String, String) {
    let top = tokio::task::spawn_blocking(|| db::most_played_uris(None, 12)).await.unwrap_or_default();
    if top.is_empty() {
        return (Vec::new(), "Songs you play in Cadence show up here (counted on this PC only).".into(), String::new());
    }
    let mut cards = Vec::new();
    for (uri, title, artist, plays) in top {
        let art_url = if uri.starts_with("spotify:") { crate::queue::player_meta(&uri).await.and_then(|m| m.art_url).unwrap_or_default() } else { String::new() };
        cards.push(CardData { title, subtitle: format!("{artist} · {plays} plays"), uri, art_url, kind: "track".into(), ..Default::default() });
    }
    (cards, String::new(), "stats".into())
}

// ---------------------------------------------------------------------------------------------
// Search.

fn go_search(ui: &MainWindow) {
    new_page(ui, "search", "");
    let b = ui.global::<Browse>();
    finish(&b);
    refresh_history();
    let q = b.get_query().to_string();
    if !q.trim().is_empty() {
        search(&q);
    }
}

fn refresh_history() {
    let h = db::history();
    with_models(|m| m.history.set_vec(h.into_iter().map(SharedString::from).collect::<Vec<_>>()));
}

/// Debounced, cancels stale requests; a pasted link becomes the top result.
pub fn search(text: &str) {
    let Some(ui) = ui() else { return };
    let s_gen = SEARCH.fetch_add(1, Relaxed) + 1;
    let g = PAGE.load(Relaxed);
    let q = text.trim().to_string();
    let b = ui.global::<Browse>();
    if q.is_empty() {
        with_models(|m| {
            m.found_tracks.set(Vec::new());
            m.found_shelves.set_vec(Vec::new());
            m.found_cards.borrow_mut().clear();
        });
        b.set_has_top(false);
        b.set_search_note("".into());
        b.set_error("".into());
        finish(&b);
        refresh_history();
        return;
    }
    if let Some(uri) = api::parse_link(&q) {
        b.set_has_top(true);
        let kind = api::capitalize(api::kind_of(&uri));
        b.set_top(Card { title: format!("Open {kind}").into(), subtitle: "From the link you pasted".into(), uri: uri.into(), ..Default::default() });
        finish(&b);
        return;
    }
    if crate::web().is_none() {
        b.set_search_note("Sign in to Spotify to search.".into());
        finish(&b);
        return;
    }
    b.set_loading(true);
    b.set_status("Searching…".into());
    let w = weak();
    crate::runtime().spawn(async move {
        // Debounce: only the query typing paused on goes out; older responses are ignored below.
        tokio::time::sleep(Duration::from_millis(SEARCH_DEBOUNCE_MS)).await;
        if SEARCH.load(Relaxed) != s_gen {
            return;
        }
        let res = api::get("search", &[("q", &q), ("type", "track,artist,album,playlist,show,episode"), ("limit", "10")]).await;
        // The user's own signals for ranking, read off the UI thread.
        let me = tokio::task::spawn_blocking(personal).await.unwrap_or_default();
        let _ = w.upgrade_in_event_loop(move |ui| {
            if SEARCH.load(Relaxed) != s_gen || PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            finish(&b);
            match res {
                Ok(j) => apply_search(&ui, &q, &j, &me, g),
                Err(e) => show_error(&b, &e.to_string()),
            }
        });
    });
}

/// Test runs: the search page with `query` and a saved search response (no network).
pub fn test_search(query: &str, response_file: &str) {
    let Some(ui) = ui() else { return };
    let Some(j) = std::fs::read(response_file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
        return crate::logln!("test: cannot read {response_file}");
    };
    go("search");
    ui.global::<Browse>().set_query(query.into());
    apply_search(&ui, query, &j, &personal(), PAGE.load(Relaxed));
    finish(&ui.global::<Browse>());
}

/// Pause in typing before a search goes out.
const SEARCH_DEBOUNCE_MS: u64 = 250;

/// Local signals for search ranking: Liked Songs, Cadence's play history, followed artists and
/// the artists played most. All from the local database; nothing is requested.
fn personal() -> crate::rank::Personal {
    let uris = |items: Vec<db::LibItem>| items.into_iter().map(|l| l.uri).collect();
    crate::rank::Personal {
        liked: uris(db::library_list("track", "", "recent", 20_000)),
        played: db::most_played_uris(None, 500).into_iter().map(|p| p.0).collect(),
        followed: uris(db::library_list("artist", "", "recent", 5000)),
        frequent: db::top_artist_uris(50).into_iter().collect(),
    }
}

fn apply_search(ui: &MainWindow, q: &str, j: &Value, me: &crate::rank::Personal, g: u64) {
    let b = ui.global::<Browse>();
    b.set_error("".into());
    let list = |k: &str| arr(&j[k]["items"]).to_vec();
    let tracks: Vec<TrackData> = list("tracks").iter().map(api::track).filter(|t| !t.uri.is_empty()).filter(|t| !hide_explicit(t)).collect();
    let artists: Vec<CardData> = list("artists").iter().filter_map(api::artist_card).collect();
    let albums: Vec<CardData> = list("albums").iter().filter_map(api::album_card).collect();
    let playlists: Vec<CardData> = list("playlists").iter().filter_map(api::playlist_card).collect();
    let shows: Vec<CardData> = list("shows").iter().filter_map(api::show_card).collect();
    let episodes: Vec<CardData> = list("episodes").iter().filter_map(api::episode_card).collect();

    // Ranking (src/rank.rs): Songs order, Top result, which artists are worth showing.
    use crate::rank;
    let ranked = rank::rank(
        q,
        &tracks.iter().map(|t| rank::Song { uri: t.uri.clone(), title: t.title.clone(), artist_uris: t.artist_uris.clone(), album_type: t.album_type.clone(), popularity: t.popularity }).collect::<Vec<_>>(),
        &artists.iter().map(|a| rank::Artist { uri: a.uri.clone(), name: a.title.clone(), popularity: a.popularity, followers: a.followers }).collect::<Vec<_>>(),
        &albums.iter().map(|a| rank::Album { title: a.title.clone(), artist_uris: a.artist_uris.clone() }).collect::<Vec<_>>(),
        me,
    );
    let tracks: Vec<TrackData> = ranked.songs.iter().map(|&i| tracks[i].clone()).collect();
    let top = match ranked.top {
        Some(rank::Top::Song(i)) => tracks.get(i).map(track_as_card),
        Some(rank::Top::Artist(i)) => artists.get(i).cloned(),
        Some(rank::Top::Album(i)) => albums.get(i).cloned(),
        None => None,
    };
    let artists: Vec<CardData> = ranked.artists.iter().map(|&i| artists[i].clone()).collect();
    match &top {
        Some(c) => {
            b.set_has_top(true);
            let kind = match c.kind.as_str() {
                "artist" => "Artist".to_string(),
                "album" => api::capitalize(if c.album_type.is_empty() { "album" } else { &c.album_type }),
                _ => "Song".to_string(),
            };
            b.set_top_kind(match c.kind.as_str() {
                "artist" => "artist",
                "album" => "album",
                _ => "song",
            }.into());
            let sub = if c.kind == "artist" { kind } else { format!("{kind} · {}", c.subtitle) };
            b.set_top(Card { title: c.title.as_str().into(), subtitle: sub.into(), uri: c.uri.as_str().into(), round: c.round, ..Default::default() });
            art_into(&c.images, &c.art_url, TOP_ART, g, |ui, img, _, fresh| {
                let b = ui.global::<Browse>();
                let mut t = b.get_top();
                t.art = img;
                t.fresh = fresh;
                b.set_top(t);
            });
        }
        None => b.set_has_top(false),
    }
    with_models(|m| m.found_tracks.set(tracks.iter().take(5).enumerate().map(|(i, t)| song(t, i, LIST_ART)).collect()));
    let enc = q.to_string();
    let sections: Vec<(&str, &str, Vec<CardData>)> = vec![
        ("Artists", "artist", artists),
        ("Albums", "album", albums),
        ("Playlists", "playlist", playlists),
        ("Podcasts", "show", shows),
        ("Episodes", "episode", episodes),
    ];
    let mut shelves = Vec::new();
    let mut models = Vec::new();
    let mut none = tracks.is_empty();
    for (title, kind, cards) in sections {
        if cards.is_empty() {
            continue;
        }
        none = false;
        let model = Rc::new(VecModel::from(cards.iter().map(card).collect::<Vec<_>>()));
        load_cards_art(&model, &cards, 160.0, g);
        shelves.push(ShelfData {
            key: kind.into(),
            title: title.into(),
            round: kind == "artist",
            cards: model.clone().into(),
            more: format!("showall:{kind}:{enc}").into(),
            note: "".into(),
            hidden: false,
        });
        models.push(model);
    }
    with_models(|m| {
        m.found_shelves.set_vec(shelves);
        *m.found_cards.borrow_mut() = models;
    });
    b.set_search_note(if none { format!("No results for \u{201c}{q}\u{201d}").into() } else { "".into() });
}

fn track_as_card(t: &TrackData) -> CardData {
    CardData {
        title: t.title.clone(),
        subtitle: t.artist.clone(),
        uri: t.uri.clone(),
        images: t.images.clone(),
        kind: "track".into(),
        ..Default::default()
    }
}

fn search_enter(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let q = b.get_query().trim().to_string();
    if q.is_empty() {
        return;
    }
    if open_link(&q) {
        return;
    }
    db::history_add(&q);
    let sel = b.get_search_sel();
    let tracks = with_models(|m| m.found_tracks.clone());
    if sel >= 0
        && let Some(t) = tracks.peek(sel as usize)
    {
        crate::queue::forget_restore();
        crate::with_player(|p| p.play(&t.uri, 0));
        return;
    }
    if b.get_has_top() {
        let uri = b.get_top().uri.to_string();
        go(&uri);
    }
}

// "Show all" for a search category: 10 per page (the API maximum), "Load more" for the rest.
fn open_showall(ui: &MainWindow, route: &str) {
    let mut parts = route.splitn(3, ':');
    parts.next();
    let kind = parts.next().unwrap_or("track").to_string();
    let q = parts.next().unwrap_or("").to_string();
    let g = new_page(ui, "list", "Searching…");
    let b = ui.global::<Browse>();
    b.set_list_kind("showall".into());
    b.set_list_uri(route.into());
    b.set_list_type(format!("Search · {}", api::capitalize(&kind)).into());
    b.set_list_title(format!("\u{201c}{q}\u{201d}").into());
    b.set_list_playable(false);
    db::history_add(&q);
    data(|d| d.more = Some((format!("search|{kind}|{q}"), 0, "search")));
    load_more_search(g);
}

fn load_more_search(g: u64) {
    let Some((key, offset, _)) = data(|d| d.more.clone()) else { return };
    let mut it = key.splitn(3, '|');
    it.next();
    let kind = it.next().unwrap_or("track").to_string();
    let q = it.next().unwrap_or("").to_string();
    let w = weak();
    crate::runtime().spawn(async move {
        let off = offset.to_string();
        let res = api::get("search", &[("q", &q), ("type", &kind), ("limit", "10"), ("offset", &off)]).await;
        let _ = w.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            finish(&b);
            let j = match res {
                Ok(j) => j,
                Err(e) => return show_error(&b, &e.to_string()),
            };
            let block = &j[format!("{kind}s")];
            let items = arr(&block["items"]);
            let more = block["next"].is_string() && offset + items.len() < 1000;
            b.set_list_more(more);
            data(|d| d.more = more.then(|| (format!("search|{kind}|{q}"), offset + items.len(), "search")));
            if kind == "track" || kind == "episode" {
                let rows: Vec<TrackData> = items.iter().map(if kind == "track" { api::track } else { api::episode }).filter(|t| !t.uri.is_empty()).collect();
                let start = data(|d| d.rows.len());
                with_models(|m| m.tracks.extend(rows.iter().enumerate().map(|(i, t)| song(t, start + i, TABLE_ART))));
                data(|d| d.rows.extend(rows));
                b.set_list_meta(format!("{} results", data(|d| d.rows.len())).into());
            } else {
                let cards: Vec<CardData> = items
                    .iter()
                    .filter_map(|x| match kind.as_str() {
                        "artist" => api::artist_card(x),
                        "album" => api::album_card(x),
                        "playlist" => api::playlist_card(x),
                        "show" => api::show_card(x),
                        _ => None,
                    })
                    .collect();
                let model = with_models(|m| m.list_cards.clone());
                model.extend(cards.iter().map(|c| (card(c), image_url(c, 160.0).unwrap_or_default())));
                b.set_list_meta(format!("{} results", model.len()).into());
            }
            if data(|d| d.rows.is_empty()) && with_models(|m| m.list_cards.len()) == 0 {
                b.set_list_note("No results.".into());
            }
        });
    });
}

// ---------------------------------------------------------------------------------------------
// Library page and sidebar.

fn go_library(ui: &MainWindow) {
    new_page(ui, "library", "");
    let b = ui.global::<Browse>();
    finish(&b);
    fill_library(ui);
    if crate::web().is_some() && db::synced_at("playlist").is_none() {
        b.set_status("Loading your library…".into());
        let w = weak();
        crate::runtime().spawn(async move {
            crate::library::sync(false).await;
            let _ = w.upgrade_in_event_loop(|ui| {
                ui.global::<Browse>().set_status("".into());
                fill_library(&ui);
                fill_sidebar(&ui);
            });
        });
    }
}

fn fill_library(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let (tab, sort, filter) = (b.get_lib_tab().to_string(), b.get_lib_sort().to_string(), b.get_lib_filter().to_string());
    let pins = db::pins();
    let folders: HashMap<String, i64> = db::folder_items().into_iter().collect();
    let mut rows: Vec<(Card, String)> = Vec::new();
    if tab == "playlist" && filter.is_empty() {
        rows.push((Card { title: "Liked Songs".into(), subtitle: format!("Playlist · {} songs", db::library_count("track")).into(), uri: "liked".into(), liked: true, ..Default::default() }, String::new()));
        for (id, name) in db::folders() {
            let count = folders.values().filter(|f| **f == id).count();
            rows.push((Card { title: name.into(), subtitle: format!("{count} playlists").into(), uri: format!("folder:{id}").into(), local: true, ..Default::default() }, String::new()));
        }
    }
    // Rows show art at the thumbnail size (smaller in the compact layout).
    let logical = if b.get_compact() { 36.0 } else { 56.0 };
    let px = art::physical(logical, scale());
    let mut items = db::library_list(&tab, &filter, &sort, 5000);
    // Pinned first.
    items.sort_by_key(|i| pins.iter().position(|p| *p == i.uri).unwrap_or(usize::MAX));
    for it in items {
        let sub = match tab.as_str() {
            "playlist" => format!("Playlist · {}", it.subtitle),
            "album" => format!("Album · {}", it.subtitle),
            "artist" => "Artist".into(),
            "show" => format!("Podcast · {}", it.subtitle),
            _ => it.artist.clone(),
        };
        TITLES.lock().map(|mut t| t.insert(it.uri.clone(), it.title.clone())).ok();
        rows.push((
            Card { title: it.title.as_str().into(), subtitle: sub.into(), uri: it.uri.as_str().into(), round: tab == "artist", pinned: pins.contains(&it.uri), ..Default::default() },
            it.art_url(px).to_string(),
        ));
    }
    let empty = rows.is_empty();
    with_models(|m| {
        m.lib.px.set(logical);
        m.lib.set(rows);
    });
    b.set_lib_note(if signed_out() {
        "Sign in to Spotify to see your library.".into()
    } else if empty {
        if filter.is_empty() { "Nothing here yet.".into() } else { format!("Nothing matches \u{201c}{filter}\u{201d}.").into() }
    } else {
        "".into()
    });
}

thread_local! {
    static OPEN_FOLDERS: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
}

/// Sidebar: pinned, Liked Songs, local folders (expandable), playlists, local files.
pub fn fill_sidebar(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let filter = b.get_side_filter().to_lowercase();
    let pins = db::pins();
    let filed: HashMap<String, i64> = db::folder_items().into_iter().collect();
    let playlists = db::library_list("playlist", "", "recent", 5000);
    let item = |t: &str, uri: &str, kind: &str, depth: i32| SideItem {
        title: t.into(),
        subtitle: "".into(),
        uri: uri.into(),
        kind: kind.into(),
        pinned: pins.iter().any(|p| p == uri),
        depth,
        open: false,
        art: Image::default(),
    };
    let matches = |t: &str| filter.is_empty() || t.to_lowercase().contains(&filter);
    let mut out = Vec::new();
    for p in &pins {
        if let Some(l) = playlists.iter().find(|l| &l.uri == p)
            && matches(&l.title)
        {
            out.push(item(&l.title, &l.uri, "playlist", 0));
        }
    }
    // Liked Songs stays pinned whatever the search says.
    out.push(item("Liked Songs", "liked", "liked", 0));
    if b.get_show_local() && matches("Local files") {
        out.push(item("Local files", "local", "local", 0));
    }
    for (id, name) in db::folders() {
        let open = OPEN_FOLDERS.with(|o| o.borrow().contains(&id)) || !filter.is_empty();
        let children: Vec<_> = playlists.iter().filter(|l| filed.get(&l.uri) == Some(&id) && matches(&l.title)).collect();
        if !matches(&name) && children.is_empty() {
            continue;
        }
        let mut f = item(&name, &format!("folder:{id}"), "folder", 0);
        f.open = open;
        out.push(f);
        if open {
            out.extend(children.iter().map(|l| item(&l.title, &l.uri, "playlist", 1)));
        }
    }
    for l in &playlists {
        if !filed.contains_key(&l.uri) && !pins.contains(&l.uri) && matches(&l.title) {
            out.push(item(&l.title, &l.uri, "playlist", 0));
        }
    }
    with_models(|m| m.side.set_vec(out));
}

fn toggle_folder(ui: &MainWindow, uri: &str) {
    if let Some(id) = uri.strip_prefix("folder:").and_then(|x| x.parse::<i64>().ok()) {
        OPEN_FOLDERS.with(|o| {
            let mut o = o.borrow_mut();
            if !o.remove(&id) {
                o.insert(id);
            }
        });
        fill_sidebar(ui);
    }
}

pub fn reload_sidebar(ui: &MainWindow) {
    fill_sidebar(ui);
}

/// After an edit: resync playlists and refresh the sidebar and library page.
pub fn refresh_library(kind: &'static str) {
    let w = weak();
    crate::runtime().spawn(async move {
        crate::library::sync_kind(kind).await;
        let _ = w.upgrade_in_event_loop(|ui| {
            fill_sidebar(&ui);
            if ui.global::<Browse>().get_page() == "library" {
                fill_library(&ui);
            }
        });
    });
}

// ---------------------------------------------------------------------------------------------
// List pages.

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "playlist" => "Playlist",
        "album" => "Album",
        "show" => "Podcast",
        _ => "Playlist",
    }
}

fn open_list(ui: &MainWindow, uri: &str) {
    let uri = if uri == "liked" || uri.ends_with(":collection") { "liked".to_string() } else { uri.to_string() };
    let kind = match api::kind_of(&uri) {
        "playlist" => "playlist",
        "album" => "album",
        "show" => "show",
        _ if uri == "liked" => "liked",
        _ => return,
    };
    let g = new_page(ui, "list", match kind {
        "playlist" => "Loading playlist…",
        "album" => "Loading album…",
        "show" => "Loading podcast…",
        _ => "Loading your liked songs…",
    });
    let b = ui.global::<Browse>();
    b.set_list_kind(kind.into());
    b.set_list_uri(uri.as_str().into());
    b.set_list_type(kind_label(kind).into());
    if let Some(t) = known_title(&uri) {
        b.set_list_title(t.into());
    }
    if kind == "liked" {
        open_liked(ui, g);
        return;
    }
    if signed_out() {
        finish(&b);
        b.set_list_note("Sign in to Spotify to see this.".into());
        return;
    }
    // Playlists and albums open from the disk cache, then refresh in place. Podcasts read through
    // the cache as before (their episode rows carry local progress and are built differently).
    fetch_list(g, kind, uri, if kind == "show" { Source::Network } else { Source::Cache });
}

/// Loads a playlist, album or podcast page from `src` (header, then its rows page by page).
fn fetch_list(g: u64, kind: &'static str, uri: String, src: Source) {
    let Some(ui) = ui() else { return };
    let b = ui.global::<Browse>();
    if src == Source::Network && crate::web().is_none() {
        // Still signing in: the cached page stays; `on_web_ready` reloads it.
        return finish(&b);
    }
    data(|d| {
        d.src = Some(src);
        d.refreshing = src == Source::Network && !d.rows.is_empty();
        d.fresh.clear();
        d.more = None;
    });
    if src == Source::Network && !data(|d| d.refreshing) {
        b.set_loading(true);
    }
    let id = api::id_of(&uri).to_string();
    let w = weak();
    let uri2 = uri.clone();
    crate::runtime().spawn(async move {
        let (head, me) = tokio::join!(
            async { let path = format!("{}s/{id}", kind); api::get_from(src, &path, &[]).await },
            async { if kind == "playlist" { me_from(src).await.ok() } else { None } }
        );
        // Saved state is live data: only asked for on the network pass.
        let saved = if src == Source::Network { api::contains(std::slice::from_ref(&uri2)).await.ok().and_then(|v| v.first().copied()) } else { None };
        let _ = w.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            let head = match head {
                Ok(h) => h,
                Err(_) if src == Source::Cache => return fetch_list(g, kind, uri2, Source::Network),
                Err(e) => {
                    finish(&b);
                    if data(|d| d.refreshing) {
                        data(|d| d.refreshing = false);
                        return crate::logln!("browse: refresh failed: {e}");
                    }
                    return show_error(&b, &e.to_string());
                }
            };
            apply_head(&ui, kind, &uri2, &head, me.map(|m| m.0), saved, g);
            match kind {
                "album" => {
                    let items = arr(&head["tracks"]["items"]).to_vec();
                    append_tracks(&ui, items.iter().map(|t| {
                        let mut d = api::track(t);
                        if d.album.is_empty() {
                            d.album = s(&head["name"]).to_string();
                        }
                        d.images = api::sizes(api::images(&head["images"]));
                        d
                    }).collect());
                    let next = head["tracks"]["next"].is_string();
                    if next {
                        data(|d| d.more = Some((format!("albums/{id}/tracks"), items.len(), "album")));
                        load_more(g, true);
                    } else {
                        if !data(|d| d.refreshing) {
                            list_meta(&b, kind, false);
                        }
                        list_done(&ui, g, kind);
                    }
                }
                _ => {
                    let path = if kind == "playlist" { format!("playlists/{id}/items") } else { format!("shows/{id}/episodes") };
                    data(|d| d.more = Some((path, 0, if kind == "playlist" { "playlist" } else { "show" })));
                    load_more(g, true);
                }
            }
        });
    });
}

fn apply_head(ui: &MainWindow, kind: &str, uri: &str, h: &Value, me: Option<String>, saved: Option<bool>, g: u64) {
    let b = ui.global::<Browse>();
    let title = s(&h["name"]).to_string();
    TITLES.lock().map(|mut t| t.insert(uri.to_string(), title.clone())).ok();
    b.set_list_title(title.as_str().into());
    let desc = api::plain(s(&h["description"]));
    b.set_list_desc(desc.as_str().into());
    if saved.is_some() || data(|d| d.src) != Some(Source::Network) {
        b.set_list_saved(saved.unwrap_or(false));
        b.set_list_can_save(saved.is_some());
    }
    match kind {
        "playlist" => {
            let owner_id = s(&h["owner"]["id"]).to_string();
            let collab = h["collaborative"].as_bool().unwrap_or(false);
            let public = h["public"].as_bool().unwrap_or(false);
            let total = n(&api::present(&h["items"], &h["tracks"])["total"]) as usize;
            let owned = me.as_deref() == Some(owner_id.as_str());
            b.set_list_owner(api::owner_name(h).into());
            b.set_list_owner_uri(s(&h["owner"]["uri"]).into());
            b.set_list_type(if collab { "Collaborative playlist".into() } else if public { "Public playlist".into() } else { "Playlist".into() });
            b.set_list_editable(owned || collab);
            // Owned playlists can't be "saved"; the heart is for others' playlists.
            b.set_list_can_save(!owned && saved.is_some());
            if !data(|d| d.refreshing) {
                b.set_list_meta(format!("{} songs", total).into());
            }
            data(|d| {
                d.owner_id = owner_id;
                d.collaborative = collab;
                d.public = public;
                d.title = title.clone();
                d.description = desc.clone();
                d.total = total;
            });
        }
        "album" => {
            let artist = api::names(&h["artists"]);
            let total = n(&h["total_tracks"]);
            b.set_list_type(api::capitalize(s(&h["album_type"])).into());
            b.set_list_owner(artist.as_str().into());
            b.set_list_owner_uri(s(&h["artists"][0]["uri"]).into());
            let date = s(&h["release_date"]);
            if !data(|d| d.refreshing) {
                b.set_list_meta(format!("{} · {} songs", date, total).into());
            }
            data(|d| {
                d.title = title.clone();
                d.total = total as usize;
                d.artist_uri = s(&h["artists"][0]["uri"]).to_string();
            });
            ui.global::<Playback>().set_side_open(true);
        }
        "show" => {
            b.set_list_owner(s(&h["publisher"]).into());
            b.set_list_meta(format!("{} episodes", n(&h["total_episodes"])).into());
            b.set_list_playable(false);
            data(|d| d.title = title.clone());
        }
        _ => {}
    }
    let imgs = api::images(&h["images"]);
    art_into(&imgs, "", 176.0, g, |ui, img, _, fresh| set_list_art(ui, img, fresh));
}

fn hide_explicit(t: &TrackData) -> bool {
    t.explicit && crate::prefs::bool_or("explicit_filter", false)
}

/// Adds rows in original order, then rebuilds the display order (sort / filter).
fn append_tracks(ui: &MainWindow, rows: Vec<TrackData>) {
    // A refresh behind rows already on screen collects quietly (see `list_done`).
    if data(|d| d.refreshing) {
        return data(|d| d.fresh.extend(rows));
    }
    data(|d| {
        d.total_ms += rows.iter().map(|r| r.duration_ms).sum::<u64>();
        d.rows.extend(rows);
    });
    apply_order(ui, false);
}

/// "N songs, about 1 hr" under the title of a playlist, album or Liked Songs.
fn list_meta(b: &Browse, kind: &str, loading: bool) {
    let (n_rows, ms, total) = data(|d| (d.rows.len(), d.total_ms, d.total));
    let size = if total > n_rows && loading { format!("{n_rows} of {total} songs") } else { format!("{n_rows} songs") };
    let meta = if kind == "album" {
        let base = b.get_list_meta().to_string();
        let date = base.split(" · ").next().unwrap_or("").to_string();
        format!("{date} · {size}, {}", api::long_duration(ms))
    } else {
        format!("{size}, about {}", api::long_duration(ms))
    };
    b.set_list_meta(meta.into());
}

/// A playlist or album finished loading from its current source.
/// - From the cache: the page is already showing; now refresh it from the network.
/// - A refresh: swap the refreshed rows in. Unchanged rows are not touched, so the list stays
///   where it is; only real differences redraw.
fn list_done(ui: &MainWindow, g: u64, kind: &'static str) {
    let b = ui.global::<Browse>();
    if data(|d| d.src) == Some(Source::Cache) {
        let uri = b.get_list_uri().to_string();
        return fetch_list(g, kind, uri, Source::Network);
    }
    if data(|d| d.refreshing) {
        data(|d| {
            d.rows = std::mem::take(&mut d.fresh);
            d.total_ms = d.rows.iter().map(|r| r.duration_ms).sum();
            d.refreshing = false;
        });
        apply_order(ui, false);
        list_meta(&b, kind, false);
    }
    finish(&b);
    if kind == "playlist" && data(|d| d.rows.is_empty()) {
        b.set_list_note(if b.get_list_editable() { "This playlist is empty. Use Add songs to find something.".into() } else { UNAVAILABLE.into() });
    }
    load_hearts(g);
}

/// Display order: sort key (remembered per playlist) and the explicit filter.
fn apply_order(ui: &MainWindow, reset_hearts: bool) {
    let b = ui.global::<Browse>();
    let (key, desc) = (b.get_sort_key().to_string(), b.get_sort_desc());
    let old: HashMap<String, (bool, bool, bool, f32)> = if reset_hearts {
        HashMap::new()
    } else {
        with_models(|m| m.tracks.rows().into_iter().map(|r| (r.uri.to_string(), (r.liked, r.liked_known, r.marked, r.progress))).collect())
    };
    let rows = data(|d| {
        let mut order: Vec<usize> = (0..d.rows.len()).filter(|&i| !hide_explicit(&d.rows[i])).collect();
        let lc = |x: &str| x.to_lowercase();
        match key.as_str() {
            "title" => order.sort_by_key(|&i| lc(&d.rows[i].title)),
            "artist" => order.sort_by_key(|&i| lc(&d.rows[i].artist)),
            "album" => order.sort_by_key(|&i| lc(&d.rows[i].album)),
            "added" => order.sort_by(|&a, &b| d.rows[a].added.cmp(&d.rows[b].added)),
            "duration" => order.sort_by_key(|&i| d.rows[i].duration_ms),
            _ => {}
        }
        if desc {
            order.reverse();
        }
        d.order = order.clone();
        order
            .iter()
            .map(|&i| {
                let (mut r, url) = song(&d.rows[i], i, TABLE_ART);
                if let Some((l, k, m, p)) = old.get(&d.rows[i].uri) {
                    r.liked = *l;
                    r.liked_known = *k;
                    r.marked = *m;
                    r.progress = *p;
                }
                (r, url)
            })
            .collect::<Vec<_>>()
    });
    with_models(|m| m.tracks.patch(rows));
}

/// Fetches the next page of the current list. `auto` keeps going until done (playlists and
/// liked lists stream in fully so sorting, duplicates and totals see everything).
fn load_more(g: u64, auto: bool) {
    let Some((path, offset, kind)) = data(|d| d.more.clone()) else { return };
    if kind == "search" {
        return load_more_search(g);
    }
    let w = weak();
    let src = data(|d| d.src).unwrap_or(Source::Network);
    crate::runtime().spawn(async move {
        let off = offset.to_string();
        let limit = if kind == "artist" { "10" } else { "50" };
        let mut q = vec![("limit", limit), ("offset", off.as_str())];
        if kind == "artist" {
            q.push(("include_groups", "album,single,compilation"));
        }
        let res = api::get_from(src, &path, &q).await;
        let _ = w.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            let j = match res {
                Ok(j) => j,
                // The cache ran out part-way: the network pass takes over from the start.
                Err(_) if src == Source::Cache => return list_done(&ui, g, kind),
                Err(e) => {
                    // A failed refresh leaves the cached rows on screen.
                    if data(|d| d.refreshing) {
                        data(|d| {
                            d.refreshing = false;
                            d.fresh.clear();
                            d.more = None;
                        });
                        finish(&b);
                        return crate::logln!("browse: refresh failed: {e}");
                    }
                    finish(&b);
                    data(|d| d.more = None);
                    b.set_list_more(false);
                    if kind == "playlist" && (e.code() == Some(403) || e.code() == Some(404)) {
                        b.set_list_note(UNAVAILABLE.into());
                        return;
                    }
                    return show_error(&b, &e.to_string());
                }
            };
            let items = arr(&j["items"]).to_vec();
            let next = j["next"].is_string() && !items.is_empty();
            let new_offset = offset + items.len();
            match kind {
                "playlist" => {
                    if offset == 0 && !items.is_empty() {
                        let mut readable = lock(&READABLE);
                        if readable.len() > 2000 {
                            readable.clear();
                        }
                        readable.insert(b.get_list_uri().to_string());
                    }
                    append_tracks(&ui, items.iter().map(|x| {
                        let mut t = api::track(api::present(&x["item"], &x["track"]));
                        t.added = s(&x["added_at"]).get(..10).unwrap_or("").to_string();
                        t
                    }).collect());
                }
                "album" => append_tracks(&ui, items.iter().map(api::track).collect()),
                "show" => {
                    let rows: Vec<TrackData> = items.iter().map(api::episode).collect();
                    let start = data(|d| d.rows.len());
                    let shown: Vec<(Song, String)> = rows.iter().enumerate().map(|(i, t)| {
                        let (mut r, url) = song(t, start + i, TABLE_ART);
                        // Local progress wins over Spotify's resume point.
                        let api_pos = n(&items[i]["resume_point"]["resume_position_ms"]);
                        let api_done = items[i]["resume_point"]["fully_played"].as_bool().unwrap_or(false);
                        let (pos, played) = db::episode_progress(&t.uri).map_or((api_pos as i64, api_done), |(p, _, done)| (p, done));
                        r.marked = played;
                        r.progress = if played { 1.0 } else if t.duration_ms > 0 { pos as f32 / t.duration_ms as f32 } else { 0.0 };
                        r.added = if played { "Played".into() } else if pos > 0 { format!("{} left", api::long_duration(t.duration_ms.saturating_sub(pos as u64))).into() } else { "".into() };
                        (r, url)
                    }).collect();
                    with_models(|m| m.tracks.extend(shown));
                    data(|d| d.rows.extend(rows));
                }
                "artist" => {
                    let cards: Vec<CardData> = items.iter().filter_map(api::album_card).collect();
                    data(|d| d.disc_all.extend(cards));
                    apply_disc(&ui, g);
                }
                _ => {}
            }
            data(|d| d.more = next.then(|| (path.clone(), new_offset, kind)));
            let auto_next = next && auto && (kind == "playlist" || (kind == "album") || (kind == "artist" && new_offset < 30));
            let quiet = data(|d| d.refreshing);
            b.set_list_more(next && !auto_next);
            if !quiet && (kind == "playlist" || kind == "album") {
                list_meta(&b, kind, auto_next);
            }
            if auto_next {
                if !quiet && src == Source::Network {
                    b.set_status(format!("Loaded {new_offset}…").into());
                }
                load_more(g, true);
            } else if kind == "playlist" || kind == "album" {
                list_done(&ui, g, kind);
            } else {
                finish(&b);
            }
        });
    });
}

/// Liked state for the rows on the page (40 per request), then the hearts appear.
fn load_hearts(g: u64) {
    if crate::web().is_none() {
        return;
    }
    let uris: Vec<String> = data(|d| d.rows.iter().filter(|r| r.uri.starts_with("spotify:track:")).map(|r| r.uri.clone()).collect());
    if uris.is_empty() || uris.len() > 2000 {
        return;
    }
    // Liked Songs: everything is liked; no request needed.
    if ui().is_some_and(|u| u.global::<Browse>().get_list_kind() == "liked") {
        set_hearts(&uris.iter().map(|u| (u.clone(), true)).collect());
        return;
    }
    let w = weak();
    crate::runtime().spawn(async move {
        let Ok(flags) = api::contains(&uris).await else { return };
        let map: HashMap<String, bool> = uris.into_iter().zip(flags).collect();
        let _ = w.upgrade_in_event_loop(move |_| {
            if PAGE.load(Relaxed) == g {
                set_hearts(&map);
            }
        });
    });
}

fn set_hearts(map: &HashMap<String, bool>) {
    for model in with_models(|m| [m.tracks.clone(), m.popular.clone()]) {
        model.update_all(|r| match map.get(r.uri.as_str()) {
            Some(&l) if !(r.liked == l && r.liked_known) => {
                r.liked = l;
                r.liked_known = true;
                true
            }
            _ => false,
        });
    }
}

fn open_liked(ui: &MainWindow, g: u64) {
    let b = ui.global::<Browse>();
    b.set_list_title("Liked Songs".into());
    b.set_list_type("Playlist".into());
    b.set_list_owner(lock(&ME).as_ref().map_or("", |m| m.1.as_str()).into());
    b.set_sort_key(crate::prefs::get("sort:liked")["key"].as_str().unwrap_or("").into());
    b.set_sort_desc(crate::prefs::get("sort:liked")["desc"].as_bool().unwrap_or(false));
    let fill = |ui: &MainWindow| {
        let rows: Vec<TrackData> = db::library_list("track", "", "recent", 20_000)
            .into_iter()
            .map(|l| TrackData {
                images: lib_images(&l),
                title: l.title,
                artist: l.artist,
                artist_uri: l.artist_uri,
                album: l.album,
                album_uri: l.album_uri,
                added: l.added_at.get(..10).unwrap_or("").to_string(),
                duration_ms: l.duration_ms as u64,
                explicit: l.explicit,
                uri: l.uri,
                ..Default::default()
            })
            .collect();
        data(|d| {
            d.rows.clear();
            d.total_ms = 0;
        });
        let n_rows = rows.len();
        append_tracks(ui, rows);
        let ms = data(|d| d.total_ms);
        let b = ui.global::<Browse>();
        b.set_list_meta(format!("{n_rows} songs, about {}", api::long_duration(ms)).into());
        load_hearts(PAGE.load(Relaxed));
        n_rows
    };
    let have = fill(ui);
    finish(&b);
    if let Some((id, _)) = lock(&ME).clone() {
        b.set_list_uri(format!("spotify:user:{id}:collection").into());
    }
    if signed_out() {
        b.set_list_note("Sign in to Spotify to see your liked songs.".into());
        return;
    }
    if crate::web().is_none() {
        // Still signing in: the rows from disk are showing; `on_web_ready` refreshes them.
        return;
    }
    // Refresh from Spotify in the background when stale or empty.
    let stale = db::synced_at("track").is_none_or(|t| db::now() - t > 600);
    if stale || have == 0 {
        b.set_status(if have == 0 { "Loading your liked songs…".into() } else { "Updating…".into() });
        let w = weak();
        crate::runtime().spawn(async move {
            let me = ensure_me().await.ok();
            crate::library::sync_kind("track").await;
            let _ = w.upgrade_in_event_loop(move |ui| {
                if PAGE.load(Relaxed) != g {
                    return;
                }
                if let Some((id, name)) = me {
                    ui.global::<Browse>().set_list_uri(format!("spotify:user:{id}:collection").into());
                    ui.global::<Browse>().set_list_owner(name.into());
                }
                finish(&ui.global::<Browse>());
                fill(&ui);
            });
        });
    }
}

fn open_local(ui: &MainWindow) {
    new_page(ui, "list", "");
    let b = ui.global::<Browse>();
    b.set_list_kind("local".into());
    b.set_list_uri("local".into());
    b.set_list_type("Local files · on this PC".into());
    b.set_list_title("Local files".into());
    b.set_sort_key(crate::prefs::get("sort:local")["key"].as_str().unwrap_or("").into());
    b.set_sort_desc(crate::prefs::get("sort:local")["desc"].as_bool().unwrap_or(false));
    finish(&b);
    let all = db::local_all();
    let missing = all.iter().filter(|t| t.missing).count();
    let broken = all.iter().filter(|t| !t.error.is_empty() && !t.missing).count();
    let rows: Vec<TrackData> = all
        .into_iter()
        .filter(|t| t.error.is_empty() || t.missing)
        .map(|t| TrackData {
            title: t.title,
            artist: t.artist,
            album: t.album,
            duration_ms: t.duration_ms as u64,
            uri: t.uri,
            local: true,
            unavailable: t.missing,
            note: if t.missing { "File missing".into() } else { String::new() },
            ..Default::default()
        })
        .collect();
    let n_rows = rows.len();
    append_tracks(ui, rows);
    let mut meta = if n_rows == 1 { "1 file".to_string() } else { format!("{n_rows} files") };
    if missing > 0 {
        meta += &format!(", {missing} missing");
    }
    if broken > 0 {
        meta += &format!(", {broken} unreadable");
    }
    b.set_list_meta(meta.into());
    let folders = crate::local::folders();
    b.set_list_note(if folders.is_empty() {
        "Add a music folder in Settings > Local files.".into()
    } else {
        format!("From {}", folders.join(", ")).into()
    });
}

fn open_folder(ui: &MainWindow, route: &str) {
    let Some(id) = route.strip_prefix("folder:").and_then(|x| x.parse::<i64>().ok()) else { return };
    new_page(ui, "list", "");
    let b = ui.global::<Browse>();
    let name = db::folders().into_iter().find(|f| f.0 == id).map(|f| f.1).unwrap_or_default();
    b.set_list_kind("folder".into());
    b.set_list_uri(route.into());
    b.set_list_type("Folder · stored in Cadence on this PC".into());
    b.set_list_title(name.into());
    b.set_list_playable(false);
    finish(&b);
    let filed: Vec<String> = db::folder_items().into_iter().filter(|(_, f)| *f == id).map(|(u, _)| u).collect();
    let cards: Vec<CardData> = filed
        .iter()
        .filter_map(|u| db::library_get(u))
        .map(|l| CardData { art_url: l.art_url(art::physical(160.0, scale())).to_string(), title: l.title, subtitle: format!("Playlist · {}", l.subtitle), uri: l.uri, kind: "playlist".into(), ..Default::default() })
        .collect();
    b.set_list_meta(format!("{} playlists", cards.len()).into());
    if cards.is_empty() {
        b.set_list_note("Empty. Right-click a playlist and choose Move to folder.".into());
    }
    with_models(|m| m.list_cards.set(cards.iter().map(|c| (card(c), image_url(c, 160.0).unwrap_or_default())).collect()));
}

fn open_artist_liked(ui: &MainWindow, artist_uri: &str) {
    new_page(ui, "list", "");
    let b = ui.global::<Browse>();
    let liked = db::liked_by_artist(artist_uri);
    let name = known_title(artist_uri).unwrap_or_else(|| liked.first().map(|l| l.artist.clone()).unwrap_or_default());
    b.set_list_kind("artist-liked".into());
    b.set_list_uri(format!("artist-liked:{artist_uri}").into());
    b.set_list_type("Liked songs".into());
    b.set_list_title(name.into());
    finish(&b);
    let rows: Vec<TrackData> = liked
        .into_iter()
        .map(|l| TrackData { images: lib_images(&l), title: l.title, artist: l.artist, album: l.album, duration_ms: l.duration_ms as u64, explicit: l.explicit, uri: l.uri, ..Default::default() })
        .collect();
    b.set_list_meta(format!("{} songs", rows.len()).into());
    append_tracks(ui, rows);
    set_hearts(&data(|d| d.rows.iter().map(|r| (r.uri.clone(), true)).collect()));
}

// ---------------------------------------------------------------------------------------------
// Artist page.

fn open_artist(ui: &MainWindow, uri: &str) {
    let g = new_page(ui, "artist", "Loading artist…");
    let b = ui.global::<Browse>();
    b.set_list_kind("artist".into());
    b.set_list_uri(uri.into());
    if let Some(t) = known_title(uri) {
        b.set_list_title(t.into());
    }
    data(|d| d.artist_uri = uri.to_string());
    // Liked songs by this artist, from the local library index.
    let liked = db::liked_by_artist(uri).len();
    if liked > 0 {
        b.set_liked_note(format!("You've liked {liked} song{}", if liked == 1 { "" } else { "s" }).into());
    }
    let id = api::id_of(uri).to_string();
    let (w, uri) = (weak(), uri.to_string());
    crate::runtime().spawn(async move {
        let head = api::get(&format!("artists/{id}"), &[]).await;
        let following = api::contains(std::slice::from_ref(&uri)).await.ok().and_then(|v| v.first().copied());
        let popular = popular_tracks(&uri).await;
        let _ = w.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            finish(&b);
            match head {
                Ok(h) => {
                    let name = s(&h["name"]).to_string();
                    TITLES.lock().map(|mut t| t.insert(uri.clone(), name.clone())).ok();
                    b.set_list_title(name.into());
                    art_into(&api::images(&h["images"]), "", 216.0, g, |ui, img, _, fresh| set_list_art(ui, img, fresh));
                }
                Err(e) => {
                    // No Web API: name and photo from librespot's artist metadata.
                    if crate::web().is_some() {
                        show_error(&b, &e.to_string());
                    }
                    let (w, uri) = (weak(), uri.clone());
                    crate::runtime().spawn(async move {
                        let Some(p) = crate::PLAYER.get().cloned() else { return };
                        let Some((name, img)) = p.artist_meta(&uri).await else { return };
                        let _ = w.upgrade_in_event_loop(move |ui| {
                            if PAGE.load(Relaxed) == g {
                                ui.global::<Browse>().set_list_title(name.into());
                                art_into(&[], &img, 216.0, g, |ui, i, _, fresh| set_list_art(ui, i, fresh));
                            }
                        });
                    });
                }
            }
            b.set_following(following.unwrap_or(false));
            let (rows, title, note) = popular;
            b.set_popular_title(title.into());
            b.set_popular_note(note.into());
            let shown: Vec<(Song, String)> = rows.iter().filter(|t| !hide_explicit(t)).enumerate().map(|(i, t)| song(t, i, LIST_ART)).collect();
            with_models(|m| m.popular.set(shown));
            let uris: Vec<String> = rows.iter().map(|r| r.uri.clone()).filter(|u| u.starts_with("spotify:track:")).collect();
            data(|d| d.popular = rows);
            if crate::web().is_some() && !uris.is_empty() {
                let w = weak();
                crate::runtime().spawn(async move {
                    if let Ok(flags) = api::contains(&uris).await {
                        let map: HashMap<String, bool> = uris.into_iter().zip(flags).collect();
                        let _ = w.upgrade_in_event_loop(move |_| {
                            if PAGE.load(Relaxed) == g {
                                set_hearts(&map);
                            }
                        });
                    }
                });
            }
            if crate::web().is_some() {
                data(|d| d.more = Some((format!("artists/{id}/albums"), 0, "artist")));
                load_more(g, true);
            }
        });
    });
}

/// Popular tracks through librespot's artist metadata (unofficial, may fail), else an honest
/// local fallback from Cadence's own plays and Liked Songs.
async fn popular_tracks(artist_uri: &str) -> (Vec<TrackData>, String, String) {
    if let Some(p) = crate::PLAYER.get().cloned()
        && let Some(uris) = p.artist_top_tracks(artist_uri).await
        && !uris.is_empty()
    {
        let mut rows = Vec::new();
        for u in uris.iter().take(10) {
            if let Some(m) = crate::queue::meta(&p, u).await {
                rows.push(TrackData {
                    title: m.title,
                    artist: m.artist,
                    album: m.album,
                    album_uri: m.album_uri,
                    duration_ms: m.duration_ms as u64,
                    explicit: m.explicit,
                    uri: u.clone(),
                    images: m.images,
                    ..Default::default()
                });
            }
        }
        if !rows.is_empty() {
            return (rows, "Popular".into(), String::new());
        }
    }
    let a = artist_uri.to_string();
    let (played, liked) = tokio::task::spawn_blocking(move || (db::most_played_uris(Some(&a), 10), db::liked_by_artist(&a))).await.unwrap_or_default();
    let mut rows: Vec<TrackData> = Vec::new();
    for (uri, title, artist, _) in played {
        rows.push(TrackData { title, artist, uri, ..Default::default() });
    }
    for l in liked {
        if rows.len() >= 10 {
            break;
        }
        if !rows.iter().any(|r| r.uri == l.uri) {
            rows.push(TrackData { images: lib_images(&l), title: l.title, artist: l.artist, album: l.album, duration_ms: l.duration_ms as u64, explicit: l.explicit, uri: l.uri, ..Default::default() });
        }
    }
    let note = if rows.is_empty() {
        "Spotify's popular tracks aren't available to Cadence right now, and you haven't played or liked this artist yet.".to_string()
    } else {
        "Spotify's popular tracks aren't available to Cadence right now. These come from your Cadence plays and Liked Songs.".to_string()
    };
    (rows, "Your top tracks".into(), note)
}

fn apply_disc(ui: &MainWindow, g: u64) {
    let b = ui.global::<Browse>();
    let f = b.get_disc_filter().to_string();
    let cards: Vec<CardData> = data(|d| d.disc_all.iter().filter(|c| f == "all" || c.album_type == f).cloned().collect());
    // Load more appends; a filter change replaces. Either way cards already shown keep their art.
    let model = with_models(|m| m.disc.clone());
    patch_model(&model, card_rows(&model, &cards));
    load_cards_art(&model, &cards, 160.0, g);
}

// ---------------------------------------------------------------------------------------------
// Stats and profile.

fn go_stats(ui: &MainWindow) {
    new_page(ui, "stats", "");
    finish(&ui.global::<Browse>());
    fill_stats(ui);
}

fn fill_stats(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let since = match b.get_stats_range().as_str() {
        "4w" => db::now() - 28 * 86_400,
        "6m" => db::now() - 182 * 86_400,
        _ => 0,
    };
    let (plays, ms) = db::totals(since);
    b.set_stats_summary(format!("{plays} plays · {} listened", api::long_duration(ms as u64)).into());
    let rows = |what: &str| -> Vec<StatRow> {
        db::top(what, since, 10)
            .into_iter()
            .enumerate()
            .map(|(i, t)| StatRow {
                rank: i as i32 + 1,
                title: t.title.into(),
                subtitle: if t.subtitle.is_empty() { api::long_duration(t.ms as u64).into() } else { format!("{} · {}", t.subtitle, api::long_duration(t.ms as u64)).into() },
                value: format!("{} plays", t.plays).into(),
                // listening time in the subtitle for artists

                uri: if what == "songs" && t.key.starts_with("spotify:track:") { t.key.into() } else { "".into() },
            })
            .collect()
    };
    b.set_stat_songs(ModelRc::new(VecModel::from(rows("songs"))));
    b.set_stat_artists(ModelRc::new(VecModel::from(rows("artists"))));
    b.set_stat_albums(ModelRc::new(VecModel::from(rows("albums"))));
}

fn go_profile(ui: &MainWindow) {
    new_page(ui, "profile", "");
    let b = ui.global::<Browse>();
    finish(&b);
    let p = crate::prefs::get("profile");
    b.set_profile_name(s(&p["name"]).into());
    b.set_profile_bio(s(&p["bio"]).into());
    b.set_profile_pic(load_profile_pic());
}

fn profile_pic_path() -> std::path::PathBuf {
    crate::auth::data_dir().join("profile.png")
}

fn load_profile_pic() -> Image {
    Image::load_from_path(&profile_pic_path()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Playing from pages.

fn visible_uris() -> Vec<String> {
    data(|d| d.order.iter().map(|&i| d.rows[i].uri.clone()).collect())
}

/// What a row's play button does.
#[derive(Debug, PartialEq, Eq)]
enum RowAction {
    /// The row is the current track: pause or resume it.
    Toggle,
    /// Another track: start it from this row, in the list's context.
    Start,
}

fn row_action(is_current: bool) -> RowAction {
    if is_current { RowAction::Toggle } else { RowAction::Start }
}

/// A row's play button. The current track (playing or paused, including a restored session)
/// toggles; any other row starts playing exactly like a double-click.
fn row_toggle(ui: &MainWindow, context: &str, i: usize, uri: &str) {
    let current = !uri.is_empty() && ui.global::<Browse>().get_current_uri() == uri;
    match row_action(current) {
        RowAction::Toggle => crate::play_pause_cmd(),
        RowAction::Start if context == uri => {
            // A row without a list context (Add songs finder): that one track.
            crate::queue::forget_restore();
            ui.global::<Playback>().set_context_uri("".into());
            crate::with_player(|p| p.play(uri, 0));
        }
        RowAction::Start => play_row(ui, context, i),
    }
}

/// Double-click on row `i` of the current page (display order) or a header Play.
fn play_row(ui: &MainWindow, context: &str, i: usize) {
    crate::queue::forget_restore();
    let b = ui.global::<Browse>();
    let kind = b.get_list_kind().to_string();
    if context == "popular" {
        let uris: Vec<String> = data(|d| d.popular.iter().filter(|t| !hide_explicit(t)).map(|t| t.uri.clone()).collect());
        ui.global::<Playback>().set_context_uri(b.get_list_uri());
        crate::with_player(|p| p.play_list(&uris, i, None));
        return;
    }
    let Some(t) = data(|d| d.order.get(i).map(|&k| d.rows[k].clone())) else { return };
    if t.unavailable {
        crate::menu::toast_text(ui, "This track isn't available.");
        return;
    }
    match kind.as_str() {
        "show" => {
            crate::podcast::play_episode(&t.uri, t.duration_ms);
            ui.global::<Playback>().set_context_uri(b.get_list_uri());
        }
        "showall" | "artist-liked" | "local" => {
            let uris = visible_uris();
            let ctx = if kind == "local" { "local" } else { "" };
            ui.global::<Playback>().set_context_uri(ctx.into());
            crate::with_player(|p| p.play_list(&uris, i, None));
        }
        _ => {
            let sorted = !b.get_sort_key().is_empty() || data(|d| d.order.len() != d.rows.len());
            ui.global::<Playback>().set_context_uri(context.into());
            if sorted || context.is_empty() {
                // Sorted or filtered: play in the order shown.
                let uris = visible_uris();
                crate::with_player(|p| p.play_list(&uris, i, None));
            } else {
                crate::with_player(|p| p.play(context, t_index(&t, i)));
            }
        }
    }
}

fn t_index(_t: &TrackData, i: usize) -> usize {
    data(|d| d.order.get(i).copied().unwrap_or(i))
}

fn play_all(ui: &MainWindow, uri: &str, shuffle: bool) {
    if uri.is_empty() {
        return;
    }
    crate::queue::forget_restore();
    let b = ui.global::<Browse>();
    ui.global::<Playback>().set_context_uri(uri.into());
    let shuffle_on = shuffle || ui.global::<Playback>().get_shuffle();
    let fewer = crate::prefs::get("shuffle_style").as_str() == Some("fewer");
    let rows: Vec<TrackData> = data(|d| d.order.iter().map(|&i| d.rows[i].clone()).collect());
    let artist = uri.starts_with("spotify:artist:");
    let filtered = data(|d| d.order.len() != d.rows.len()) || !b.get_sort_key().is_empty();
    crate::with_player(|p| {
        if shuffle_on && fewer && !rows.is_empty() {
            p.set_shuffle(true);
            let recent = db::most_played_uris(None, 0);
            let order = fewer_repeats(&rows, &recent.into_iter().map(|r| r.0).collect::<HashSet<_>>());
            p.play_list(&order, 0, Some(false));
        } else if (filtered || uri == "local" || uri.starts_with("artist-liked:")) && !rows.is_empty() {
            let uris: Vec<String> = rows.iter().map(|r| r.uri.clone()).collect();
            if shuffle_on {
                p.set_shuffle(true);
            }
            let start = if shuffle_on { crate::player::random_below(uris.len()) } else { 0 };
            p.play_list(&uris, start, None);
        } else if shuffle_on {
            p.play_shuffled(uri, if artist { 0 } else { rows.len() });
        } else {
            p.play(uri, 0);
        }
    });
}

/// "Fewer repeats" shuffle: songs of one artist spread apart, recently played ones last.
pub fn fewer_repeats(rows: &[TrackData], recent: &HashSet<String>) -> Vec<String> {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    let mut late: Vec<String> = Vec::new();
    for r in rows {
        if r.uri.is_empty() || r.unavailable {
            continue;
        }
        if recent.contains(&r.uri) {
            late.push(r.uri.clone());
        } else {
            groups.entry(r.artist.clone()).or_default().push(r.uri.clone());
        }
    }
    // Next song: the artist with the most songs left (ties random), unless that would make a
    // run longer than unavoidable, ceil(biggest / (others + 1)).
    let total: usize = groups.values().map(Vec::len).sum();
    let biggest = groups.values().map(Vec::len).max().unwrap_or(0);
    let limit = biggest.div_ceil(total.saturating_sub(biggest) + 1).max(1);
    for g in groups.values_mut() {
        shuffle_vec(g);
    }
    let mut out = Vec::with_capacity(total);
    let (mut last, mut run) = (String::new(), 0usize);
    while groups.values().any(|g| !g.is_empty()) {
        let mut keys: Vec<(String, usize)> = groups.iter().filter(|(_, g)| !g.is_empty()).map(|(k, g)| (k.clone(), g.len())).collect();
        shuffle_vec(&mut keys);
        keys.sort_by(|a, b| b.1.cmp(&a.1));
        let pick = keys.iter().find(|(k, _)| !(*k == last && run >= limit)).or(keys.first()).map(|(k, _)| k.clone()).unwrap_or_default();
        if let Some(u) = groups.get_mut(&pick).and_then(|g| g.pop()) {
            out.push(u);
        }
        if pick == last {
            run += 1;
        } else {
            last = pick;
            run = 1;
        }
    }
    shuffle_vec(&mut late);
    out.extend(late);
    out
}

fn shuffle_vec<T>(v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let j = crate::player::random_below(i + 1);
        v.swap(i, j);
    }
}

fn play_card(ui: &MainWindow, uri: &str) {
    crate::queue::forget_restore();
    let uri = if uri == "liked" {
        my_id().map(|id| format!("spotify:user:{id}:collection")).unwrap_or_default()
    } else {
        uri.to_string()
    };
    if uri.is_empty() || uri.starts_with("folder:") || uri == "library" {
        return;
    }
    if uri == "local" {
        let uris: Vec<String> = db::local_all().into_iter().filter(|t| !t.missing && t.error.is_empty()).map(|t| t.uri).collect();
        crate::with_player(|p| p.play_list(&uris, 0, None));
        return;
    }
    if uri.starts_with("spotify:episode:") {
        crate::podcast::play_episode(&uri, 0);
        return;
    }
    let ctx = if uri.starts_with("spotify:track:") { "" } else { uri.as_str() };
    ui.global::<Playback>().set_context_uri(ctx.into());
    if uri.starts_with("spotify:show:") {
        // Latest episode.
        let id = api::id_of(&uri).to_string();
        crate::runtime().spawn(async move {
            if let Ok(j) = api::get(&format!("shows/{id}/episodes"), &[("limit", "1")]).await
                && let Some(e) = arr(&j["items"]).first()
            {
                let (u, d) = (s(&e["uri"]).to_string(), n(&e["duration_ms"]));
                let _ = slint::invoke_from_event_loop(move || crate::podcast::play_episode(&u, d));
            }
        });
        return;
    }
    crate::with_player(|p| p.play(&uri, 0));
}

// ---------------------------------------------------------------------------------------------
// Page actions.

fn toggle_save(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let uri = b.get_list_uri().to_string();
    let saved = b.get_list_saved();
    b.set_list_saved(!saved);
    let w = weak();
    crate::runtime().spawn(async move {
        let r = api::set_saved(std::slice::from_ref(&uri), !saved).await;
        let kind: &'static str = match api::kind_of(&uri) {
            "album" => "album",
            "show" => "show",
            _ => "playlist",
        };
        let _ = w.upgrade_in_event_loop(move |ui| match r {
            Ok(()) => {
                crate::menu::toast_text(&ui, if saved { "Removed from Your Library" } else { "Saved to Your Library" });
                refresh_library(kind);
            }
            Err(e) => {
                ui.global::<Browse>().set_list_saved(saved);
                crate::menu::toast_text(&ui, &format!("Couldn't update Your Library: {}", crate::menu::why(&e)));
            }
        });
    });
}

fn toggle_follow(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let uri = b.get_list_uri().to_string();
    let on = b.get_following();
    b.set_following(!on);
    let w = weak();
    crate::runtime().spawn(async move {
        let r = api::set_saved(std::slice::from_ref(&uri), !on).await;
        let _ = w.upgrade_in_event_loop(move |ui| match r {
            Ok(()) => {
                crate::menu::toast_text(&ui, if on { "Unfollowed" } else { "Following" });
                refresh_library("artist");
            }
            Err(e) => {
                ui.global::<Browse>().set_following(on);
                crate::menu::toast_text(&ui, &format!("Couldn't update: {}", crate::menu::why(&e)));
            }
        });
    });
}

/// Heart on row `i` (negative: popular row -1 - i).
fn track_heart(ui: &MainWindow, i: i32) {
    let model = if i < 0 { with_models(|m| m.popular.clone()) } else { with_models(|m| m.tracks.clone()) };
    let idx = if i < 0 { (-1 - i) as usize } else { i as usize };
    let Some(mut r) = model.peek(idx) else { return };
    if !r.liked_known {
        return;
    }
    let (uri, liked) = (r.uri.to_string(), r.liked);
    r.liked = !liked;
    model.set_row_data(idx, r);
    let w = weak();
    crate::runtime().spawn(async move {
        let res = api::set_saved(std::slice::from_ref(&uri), !liked).await;
        let _ = w.upgrade_in_event_loop(move |ui| match res {
            Ok(()) => {
                crate::library::note_saved(&uri, !liked, None);
                crate::menu::toast_text(&ui, if liked { "Removed from Liked Songs" } else { "Saved to Liked Songs" });
                crate::refresh_like_state(&ui);
            }
            Err(e) => crate::menu::toast_text(&ui, &format!("Couldn't update Liked Songs: {}", crate::menu::why(&e))),
        });
    });
}

fn sort_by(ui: &MainWindow, key: &str) {
    let b = ui.global::<Browse>();
    let (cur, desc) = (b.get_sort_key().to_string(), b.get_sort_desc());
    // Click cycles: ascending -> descending -> original order.
    let (k, d) = if cur != key { (key.to_string(), false) } else if !desc { (key.to_string(), true) } else { (String::new(), false) };
    b.set_sort_key(k.as_str().into());
    b.set_sort_desc(d);
    let uri = b.get_list_uri().to_string();
    let pref = if b.get_list_kind() == "liked" { "sort:liked".to_string() } else if uri == "local" { "sort:local".into() } else { format!("sort:{uri}") };
    crate::prefs::set(&pref, json!({ "key": k, "desc": d }));
    apply_order(ui, false);
}

// "Let's find something for your playlist".
fn find(ui: &MainWindow, text: &str) {
    let f = FIND.fetch_add(1, Relaxed) + 1;
    let g = PAGE.load(Relaxed);
    let q = text.trim().to_string();
    let b = ui.global::<Browse>();
    b.set_find_expanded("".into());
    if q.is_empty() {
        with_models(|m| {
            m.find_tracks.set(Vec::new());
            m.find_cards.set_vec(Vec::new());
        });
        b.set_find_note("".into());
        return;
    }
    b.set_find_note("Searching…".into());
    let w = weak();
    crate::runtime().spawn(async move {
        tokio::time::sleep(Duration::from_millis(280)).await;
        if FIND.load(Relaxed) != f {
            return;
        }
        let res = api::get("search", &[("q", &q), ("type", "track,artist,album"), ("limit", "10")]).await;
        let _ = w.upgrade_in_event_loop(move |ui| {
            if FIND.load(Relaxed) != f || PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            match res {
                Ok(j) => {
                    let tracks: Vec<TrackData> = arr(&j["tracks"]["items"]).iter().map(api::track).filter(|t| !t.uri.is_empty()).collect();
                    let mut cards: Vec<CardData> = arr(&j["artists"]["items"]).iter().take(3).filter_map(api::artist_card).collect();
                    cards.extend(arr(&j["albums"]["items"]).iter().take(4).filter_map(api::album_card));
                    b.set_find_note(if tracks.is_empty() && cards.is_empty() { "No results.".into() } else { "".into() });
                    set_find(&tracks, &cards);
                }
                Err(e) => b.set_find_note(friendly(&e.to_string()).into()),
            }
        });
    });
}

fn set_find(tracks: &[TrackData], cards: &[CardData]) {
    let have: HashSet<String> = data(|d| d.rows.iter().map(|r| r.uri.clone()).collect());
    let rows: Vec<(Song, String)> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let (mut r, url) = song(t, i, LIST_ART);
            r.marked = have.contains(&t.uri);
            (r, url)
        })
        .collect();
    with_models(|m| {
        m.find_tracks.set(rows);
        m.find_cards.set_vec(cards.iter().map(card).collect::<Vec<_>>());
    });
    data(|d| {
        d.find = tracks.to_vec();
        if !cards.is_empty() {
            d.find_cards = cards.to_vec();
        }
    });
}

/// Artist / album chip: show its tracks in the find list.
fn find_expand(ui: &MainWindow, uri: &str) {
    let b = ui.global::<Browse>();
    b.set_find_expanded(uri.into());
    b.set_find_note("Loading…".into());
    let g = PAGE.load(Relaxed);
    let (w, uri) = (weak(), uri.to_string());
    crate::runtime().spawn(async move {
        let tracks: Vec<TrackData> = if uri.starts_with("spotify:album:") {
            match api::get(&format!("albums/{}", api::id_of(&uri)), &[]).await {
                Ok(j) => arr(&j["tracks"]["items"]).iter().map(|t| {
                    let mut d = api::track(t);
                    d.album = s(&j["name"]).to_string();
                    d
                }).collect(),
                Err(_) => Vec::new(),
            }
        } else {
            popular_tracks(&uri).await.0
        };
        let _ = w.upgrade_in_event_loop(move |ui| {
            if PAGE.load(Relaxed) != g {
                return;
            }
            let b = ui.global::<Browse>();
            b.set_find_note(if tracks.is_empty() { "Nothing to add from here.".into() } else { "".into() });
            let cards = data(|d| d.find_cards.clone());
            set_find(&tracks, &cards);
        });
    });
}

fn find_add(ui: &MainWindow, i: usize) {
    let Some(t) = data(|d| d.find.get(i).cloned()) else { return };
    let b = ui.global::<Browse>();
    let playlist = b.get_list_uri().to_string();
    let dup = data(|d| d.rows.iter().any(|r| r.uri == t.uri));
    if dup {
        let (pl, uri) = (playlist.clone(), t.uri.clone());
        crate::dialogs::confirm(
            ui,
            "Already in this playlist",
            &format!("\u{201c}{}\u{201d} is already in this playlist. Add it again?", t.title),
            "Add anyway",
            false,
            move |ui| add_to_current(ui, &pl, &uri),
        );
        return;
    }
    add_to_current(ui, &playlist, &t.uri);
}

fn add_to_current(ui: &MainWindow, playlist: &str, uri: &str) {
    let id = api::id_of(playlist).to_string();
    let (w, uri, playlist) = (ui.as_weak(), uri.to_string(), playlist.to_string());
    crate::runtime().spawn(async move {
        let r = api::call(Method::POST, &format!("playlists/{id}/items"), &[], Some(&json!({ "uris": [uri] }))).await;
        let meta = crate::queue::player_meta(&uri).await;
        let _ = w.upgrade_in_event_loop(move |ui| match r {
            Ok(_) => {
                crate::menu::toast_text(&ui, "Added to this playlist");
                if ui.global::<Browse>().get_list_uri() == playlist.as_str() {
                    let t = meta.map(|m| TrackData { title: m.title, artist: m.artist, album: m.album, duration_ms: m.duration_ms as u64, explicit: m.explicit, images: m.images, uri: uri.clone(), added: "today".into(), ..Default::default() })
                        .unwrap_or(TrackData { uri: uri.clone(), title: uri.clone(), ..Default::default() });
                    append_tracks(&ui, vec![t]);
                    let (tracks, cards) = data(|d| (d.find.clone(), d.find_cards.clone()));
                    set_find(&tracks, &cards);
                    data(|d| d.total += 1);
                }
            }
            Err(e) => crate::menu::toast_text(&ui, &format!("Couldn't add it: {}", crate::menu::why(&e))),
        });
    });
}

/// Current page rows for menus (duplicates, CSV, add-all).
pub fn page_rows() -> Vec<TrackData> {
    data(|d| d.rows.clone())
}

pub fn page_info() -> (String, String, bool, bool, String) {
    data(|d| (d.title.clone(), d.description.clone(), d.public, d.collaborative, d.owner_id.clone()))
}

/// Reloads the current page (after an edit).
pub fn reload() {
    let route = NAV.with(|n| {
        let n = n.borrow();
        n.0.get(n.1).map(|r| r.0.clone())
    });
    if let (Some(ui), Some(r)) = (ui(), route) {
        show(&ui, &r, 0.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Init.

pub fn init(ui: &MainWindow) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    let models = Models {
        shelves: Rc::new(VecModel::default()),
        shelf_cards: RefCell::new(Vec::new()),
        found_tracks: LazyArt::new(LIST_ART),
        found_shelves: Rc::new(VecModel::default()),
        found_cards: RefCell::new(Vec::new()),
        tracks: LazyArt::new(TABLE_ART),
        list_cards: LazyCards::new(160.0),
        lib: LazyCards::new(56.0),
        card_urls: RefCell::new(HashMap::new()),
        side: Rc::new(VecModel::default()),
        popular: LazyArt::new(LIST_ART),
        disc: Rc::new(VecModel::default()),
        find_tracks: LazyArt::new(LIST_ART),
        find_cards: Rc::new(VecModel::default()),
        history: Rc::new(VecModel::default()),
    };
    let b = ui.global::<Browse>();
    b.set_shelves(models.shelves.clone().into());
    b.set_found_tracks(models.found_tracks.clone().into());
    b.set_found_shelves(models.found_shelves.clone().into());
    b.set_tracks(models.tracks.clone().into());
    b.set_list_cards(models.list_cards.clone().into());
    b.set_lib_items(models.lib.clone().into());
    b.set_side_items(models.side.clone().into());
    b.set_popular(models.popular.clone().into());
    b.set_disc(models.disc.clone().into());
    b.set_find_tracks(models.find_tracks.clone().into());
    b.set_find_cards(models.find_cards.clone().into());
    b.set_history(models.history.clone().into());
    MODELS.with(|m| *m.borrow_mut() = Some(models));
    b.set_compact(crate::prefs::bool_or("compact", false));
    b.set_show_local(crate::prefs::bool_or("show_local", false));
    b.set_lib_tab(crate::prefs::get("lib_tab").as_str().unwrap_or("playlist").into());
    b.set_lib_sort(crate::prefs::get("lib_sort").as_str().unwrap_or("recent").into());

    let w = ui.as_weak();
    b.on_scrolled(move || {
        if let Some(ui) = w.upgrade() {
            on_scrolled(&ui);
        }
    });
    b.on_top_pause(crate::play_pause_cmd);
    let w = ui.as_weak();
    b.on_row_toggle(move |context, i, uri| {
        if let Some(ui) = w.upgrade() {
            row_toggle(&ui, &context, i.max(0) as usize, &uri);
        }
    });
    b.on_open(|uri| go(&uri));
    b.on_go(|route| go(&route));
    b.on_back(back);
    b.on_forward(forward);
    b.on_search(|t| search(&t));
    let w = ui.as_weak();
    b.on_search_enter(move || {
        if let Some(ui) = w.upgrade() {
            search_enter(&ui);
        }
    });
    let w = ui.as_weak();
    b.on_history_pick(move |q| {
        if let Some(ui) = w.upgrade() {
            ui.global::<Browse>().set_query(q.clone());
            search(&q);
        }
    });
    b.on_history_remove(|q| {
        db::history_remove(&q);
        refresh_history();
    });
    b.on_history_clear(|| {
        db::history_clear();
        refresh_history();
    });
    let w = ui.as_weak();
    b.on_lib_changed(move || {
        if let Some(ui) = w.upgrade() {
            let b = ui.global::<Browse>();
            crate::prefs::set("lib_tab", json!(b.get_lib_tab().as_str()));
            crate::prefs::set("lib_sort", json!(b.get_lib_sort().as_str()));
            fill_library(&ui);
        }
    });
    let w = ui.as_weak();
    b.on_side_filter_edited(move |_| {
        if let Some(ui) = w.upgrade() {
            fill_sidebar(&ui);
        }
    });
    let w = ui.as_weak();
    b.on_side_toggle(move |uri| {
        if let Some(ui) = w.upgrade() {
            toggle_folder(&ui, &uri);
        }
    });
    let w = ui.as_weak();
    b.on_new_folder(move || {
        if let Some(ui) = w.upgrade() {
            crate::dialogs::new_folder(&ui);
        }
    });
    let w = ui.as_weak();
    b.on_create_playlist(move || {
        if let Some(ui) = w.upgrade() {
            crate::dialogs::create_playlist(&ui, None);
        }
    });
    let w = ui.as_weak();
    b.on_play(move |ctx, i| {
        if let Some(ui) = w.upgrade() {
            play_row(&ui, &ctx, i.max(0) as usize);
        }
    });
    let w = ui.as_weak();
    b.on_play_all(move |uri, shuffle| {
        if let Some(ui) = w.upgrade() {
            play_all(&ui, &uri, shuffle);
        }
    });
    let w = ui.as_weak();
    b.on_play_card(move |uri| {
        if let Some(ui) = w.upgrade() {
            play_card(&ui, &uri);
        }
    });
    b.on_card_menu(|uri, x, y| crate::menu::open_for_card(&uri, x, y));
    b.on_list_menu(|x, y| crate::menu::open_list_menu(x, y));
    let w = ui.as_weak();
    b.on_toggle_save(move || {
        if let Some(ui) = w.upgrade() {
            toggle_save(&ui);
        }
    });
    let w = ui.as_weak();
    b.on_toggle_follow(move || {
        if let Some(ui) = w.upgrade() {
            toggle_follow(&ui);
        }
    });
    let w = ui.as_weak();
    b.on_track_heart(move |i| {
        if let Some(ui) = w.upgrade() {
            track_heart(&ui, i);
        }
    });
    let w = ui.as_weak();
    b.on_sort_by(move |k| {
        if let Some(ui) = w.upgrade() {
            sort_by(&ui, &k);
        }
    });
    b.on_load_more(|| load_more(PAGE.load(Relaxed), false));
    let w = ui.as_weak();
    b.on_open_owner(move || {
        if let Some(ui) = w.upgrade() {
            let uri = ui.global::<Browse>().get_list_owner_uri().to_string();
            // Albums link to their artist; other users' profiles aren't available to dev-mode apps.
            if uri.starts_with("spotify:artist:") {
                go(&uri);
            }
        }
    });
    let w = ui.as_weak();
    b.on_find(move |t| {
        if let Some(ui) = w.upgrade() {
            find(&ui, &t);
        }
    });
    let w = ui.as_weak();
    b.on_find_add(move |i| {
        if let Some(ui) = w.upgrade() {
            find_add(&ui, i.max(0) as usize);
        }
    });
    let w = ui.as_weak();
    b.on_find_expand(move |u| {
        if let Some(ui) = w.upgrade() {
            find_expand(&ui, &u);
        }
    });
    let w = ui.as_weak();
    b.on_set_disc_filter(move |f| {
        if let Some(ui) = w.upgrade() {
            ui.global::<Browse>().set_disc_filter(f);
            apply_disc(&ui, PAGE.load(Relaxed));
        }
    });
    b.on_open_artist_liked(|| {
        let a = data(|d| d.artist_uri.clone());
        if !a.is_empty() {
            go(&format!("artist-liked:{a}"));
        }
    });
    let w = ui.as_weak();
    b.on_home_move(move |key, delta| {
        let mut l = home_layout();
        if let Some(i) = l.iter().position(|(k, _)| k == key.as_str()) {
            let j = (i as i32 + delta).clamp(0, l.len() as i32 - 1) as usize;
            let it = l.remove(i);
            l.insert(j, it);
            save_home_layout(&l);
        }
        if let Some(ui) = w.upgrade() {
            go_home(&ui);
            ui.global::<Browse>().set_home_editing(true);
        }
    });
    let w = ui.as_weak();
    b.on_home_hide(move |key, hidden| {
        let mut l = home_layout();
        if let Some(it) = l.iter_mut().find(|(k, _)| k == key.as_str()) {
            it.1 = hidden;
        }
        save_home_layout(&l);
        if let Some(ui) = w.upgrade() {
            go_home(&ui);
            ui.global::<Browse>().set_home_editing(true);
        }
    });
    let w = ui.as_weak();
    b.on_stats_changed(move || {
        if let Some(ui) = w.upgrade() {
            fill_stats(&ui);
        }
    });
    let w = ui.as_weak();
    b.on_stats_clear(move || {
        if let Some(ui) = w.upgrade() {
            crate::dialogs::confirm(&ui, "Clear listening history?", "This deletes every play Cadence recorded on this PC. It doesn't affect Spotify.", "Clear", true, |ui| {
                db::clear_plays();
                fill_stats(ui);
                crate::menu::toast_text(ui, "Listening history cleared");
            });
        }
    });
    let w = ui.as_weak();
    b.on_profile_save(move || {
        if let Some(ui) = w.upgrade() {
            let b = ui.global::<Browse>();
            crate::prefs::set("profile", json!({ "name": b.get_profile_name().as_str(), "bio": b.get_profile_bio().as_str() }));
            crate::menu::toast_text(&ui, "Profile saved on this PC");
        }
    });
    let w = ui.as_weak();
    b.on_profile_pick(move || {
        let w = w.clone();
        crate::winint::pick_file("Choose a profile picture", &[("Images", "*.jpg;*.jpeg;*.png")], move |path| {
            std::thread::spawn(move || {
                let ok = crate::art::save_square_png(&path, &profile_pic_path(), 256);
                let _ = w.upgrade_in_event_loop(move |ui| {
                    if ok {
                        ui.global::<Browse>().set_profile_pic(load_profile_pic());
                    } else {
                        crate::menu::toast_text(&ui, "Couldn't read that image");
                    }
                });
            });
        });
    });

    fill_sidebar(ui);
    NAV.with(|n| *n.borrow_mut() = (vec![("home".into(), 0.0)], 0));
    go_home(ui);
    update_nav(&b);

}

/// The page scrolled (or resized): tell the virtualized card lists which rows are on screen.
fn on_scrolled(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let (y, h) = ((-b.get_scroll_y()).max(0.0), b.get_view_height().max(1.0));
    match b.get_page().as_str() {
        "library" => {
            let row = if b.get_compact() { 48.0 } else { 68.0 };
            let first = (y / row) as usize;
            with_models(|m| m.lib.window(first, first + (h / row).ceil() as usize));
        }
        "list" if with_models(|m| m.tracks.len()) > 0 => {
            // Song table rows (Tokens.row-track).
            let first = (y / 56.0) as usize;
            with_models(|m| m.tracks.window(first, first + (h / 56.0).ceil() as usize));
        }
        "list" if with_models(|m| m.list_cards.len()) > 0 => {
            // The card grid: 176 px cells, 220 px rows, 32 px page margins (ui/browse.slint CardGrid).
            let width = ui.window().size().width as f32 / scale() - 232.0;
            let cols = (((width - 64.0 + 16.0) / 176.0).floor() as usize).max(1);
            let first = (y / 220.0) as usize * cols;
            with_models(|m| m.list_cards.window(first, first + (h / 220.0).ceil() as usize * cols));
        }
        _ => {}
    }
}

/// The launch sign-in finished: refresh what the cache-first start is showing, in place.
pub fn on_web_ready() {
    let Some(ui) = ui() else { return };
    let b = ui.global::<Browse>();
    if b.get_status().as_str().starts_with("Signing in") {
        b.set_status("".into());
    }
    match b.get_page().as_str() {
        "home" => refresh_home(PAGE.load(Relaxed), false),
        "settings" | "stats" | "profile" => {}
        _ => reload(),
    }
    // Library index: sync in the background, then refresh the sidebar and the library page.
    let w = ui.as_weak();
    crate::runtime().spawn(async move {
        let changed = crate::library::sync(false).await;
        if !changed.is_empty() {
            let _ = w.upgrade_in_event_loop(|ui| {
                fill_sidebar(&ui);
                if ui.global::<Browse>().get_page() == "library" {
                    fill_library(&ui);
                }
            });
        }
    });
}

/// The launch sign-in failed or there is no client ID: show the signed-out state.
pub fn on_web_unavailable(status: &str) {
    reload();
    if let Some(ui) = ui() {
        ui.global::<Browse>().set_status(status.into());
    }
}

/// Re-runs the explicit filter on the page (Settings change).
pub fn reapply_filters() {
    if let Some(ui) = ui() {
        apply_order(&ui, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_button_toggles_only_the_current_track() {
        assert_eq!(row_action(true), RowAction::Toggle);
        assert_eq!(row_action(false), RowAction::Start);
    }

    #[test]
    fn patch_model_keeps_what_did_not_change() {
        let model = VecModel::from(vec![1, 2, 3]);
        let rows = |m: &VecModel<i32>| m.iter().collect::<Vec<_>>();
        // Same rows: nothing to do.
        patch_model(&model, vec![1, 2, 3]);
        assert_eq!(rows(&model), [1, 2, 3]);
        // Longer with the same start: the new rows are appended.
        patch_model(&model, vec![1, 2, 3, 4, 5]);
        assert_eq!(rows(&model), [1, 2, 3, 4, 5]);
        // Same length, one row differs: that row is written.
        patch_model(&model, vec![1, 9, 3, 4, 5]);
        assert_eq!(rows(&model), [1, 9, 3, 4, 5]);
        // Shorter: replaced.
        patch_model(&model, vec![7]);
        assert_eq!(rows(&model), [7]);
        // From empty (first load).
        let empty: VecModel<i32> = VecModel::default();
        patch_model(&empty, vec![1, 2]);
        assert_eq!(rows(&empty), [1, 2]);
    }

    #[test]
    fn fewer_repeats_spreads_artists() {
        let rows: Vec<TrackData> = (0..9)
            .map(|i| TrackData { uri: format!("u{i}"), artist: if i < 6 { "A".into() } else { format!("B{i}") }, ..Default::default() })
            .collect();
        let order = fewer_repeats(&rows, &HashSet::new());
        assert_eq!(order.len(), 9);
        let artist = |u: &str| rows.iter().find(|r| r.uri == u).unwrap().artist.clone();
        // 6 of 9 by A: neighbours are unavoidable, but A is spread out (never 4 in a row),
        // and two artists with equal counts alternate.
        for _ in 0..50 {
            let order = fewer_repeats(&rows, &HashSet::new());
            assert!(order.windows(4).all(|w| !w.iter().all(|u| artist(u) == "A")));
        }
        let even: Vec<TrackData> = (0..8).map(|i| TrackData { uri: format!("e{i}"), artist: if i % 2 == 0 { "X".into() } else { "Y".into() }, ..Default::default() }).collect();
        let o = fewer_repeats(&even, &HashSet::new());
        let a2 = |u: &str| even.iter().find(|r| r.uri == u).unwrap().artist.clone();
        assert!(o.windows(3).all(|w| !(a2(&w[0]) == a2(&w[1]) && a2(&w[1]) == a2(&w[2]))));
        let recent: HashSet<String> = ["u0".to_string()].into();
        assert_eq!(fewer_repeats(&rows, &recent).last().map(String::as_str), Some("u0"));
    }

    #[test]
    fn dates() {
        assert_eq!(days_ago_date(0).len(), 10);
    }
}

/// Local index changed (scan finished).
pub fn local_changed() {
    if let Some(ui) = ui() {
        if ui.global::<Browse>().get_list_kind() == "local" {
            open_local(&ui);
        }
        fill_sidebar(&ui);
    }
}

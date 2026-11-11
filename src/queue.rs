//! The play queue as Cadence shows and edits it.
//!
//! Source of truth is librespot's Connect state (patched in vendor/librespot-connect): its "next"
//! list holds the user queue first, then the context. Cadence adds two things on top:
//! - Play Next: items inserted ahead of the rest of the user queue, in the order they were added
//!   (`PLAY_NEXT` remembers their uids), so "Play next" always runs before "Add to queue" items
//!   and context advancement never touches either.
//! - Persistence: `queue.json` keeps context, track, position and queued items, so a restart
//!   resumes where playback stopped (on the first press of Play, never on its own).
//!
//! Memory: only the rows on screen (≤ 60 upcoming + 10 recent) and a 400-entry metadata cache.

use crate::player::{ItemMeta, Player};
use crate::{MainWindow, Playback, Queue, QueueRow};
use serde_json::{Value, json};
use slint::{ComponentHandle, VecModel, Weak};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Upcoming rows shown (queue + context).
const SHOW_NEXT: usize = 60;
const META_CAP: usize = 400;

static META: Mutex<Option<HashMap<String, ItemMeta>>> = Mutex::new(None);
/// uids of Play Next items, oldest first.
static PLAY_NEXT: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Bumped per refresh; a slow refresh drops its result when a newer one started.
static GEN: AtomicU64 = AtomicU64::new(0);
/// Saved session waiting for the first Play (`None` once used or replaced).
static RESTORE: Mutex<Option<Saved>> = Mutex::new(None);
/// Last snapshot's queued uris (for saving).
static QUEUED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The playing track came from the user queue, not the context.
static FROM_QUEUE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The panel is showing: only then are rows (and their metadata) loaded.
static PANEL_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_panel_open(open: bool) {
    PANEL_OPEN.store(open, Ordering::Relaxed);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub async fn meta(p: &Player, uri: &str) -> Option<ItemMeta> {
    if let Some(m) = lock(&META).as_ref().and_then(|c| c.get(uri)) {
        return Some(m.clone());
    }
    let m = p.item_meta(uri).await?;
    let mut c = lock(&META);
    let c = c.get_or_insert_with(HashMap::new);
    if c.len() >= META_CAP {
        c.clear();
    }
    c.insert(uri.to_string(), m.clone());
    Some(m)
}

/// Track / episode metadata through the player session (None before streaming is up).
pub async fn player_meta(uri: &str) -> Option<ItemMeta> {
    let p = crate::PLAYER.get()?.clone();
    meta(&p, uri).await
}

pub fn mmss(ms: u32) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)
}

/// "1 hr 12 min" / "14 min".
fn total(ms: u64) -> String {
    let min = (ms + 30_000) / 60_000;
    if min >= 60 { format!("{} hr {} min", min / 60, min % 60) } else { format!("{min} min") }
}

fn row(uri: &str, uid: &str, queued: bool, play_next: bool, m: Option<&ItemMeta>) -> QueueRow {
    let unavailable = m.is_none();
    QueueRow {
        title: m.map_or_else(|| "Unavailable".into(), |m| m.title.clone()).into(),
        artist: m.map(|m| m.artist.clone()).unwrap_or_default().into(),
        duration: m.map(|m| mmss(m.duration_ms)).unwrap_or_default().into(),
        uri: uri.into(),
        uid: uid.into(),
        queued,
        play_next,
        unavailable,
        explicit: m.is_some_and(|m| m.explicit),
        art: slint::Image::default(),
        fresh: false,
    }
}

/// Logical size of a queue row's cover (TrackRow's list variant).
const ROW_ART: f32 = 48.0;

/// Covers for the rows of `model`: from memory at once, the rest loaded and set as they arrive.
/// The queue shows at most `SHOW_NEXT` + 10 rows, so this is bounded.
fn load_art(model: &Rc<VecModel<QueueRow>>, metas: Vec<Option<ItemMeta>>, scale: f32) {
    use slint::Model;
    let px = crate::art::physical(ROW_ART, scale);
    for (i, m) in metas.into_iter().enumerate() {
        let Some(url) = m.and_then(|m| crate::art::pick_url(&m.images, ROW_ART, scale)) else { continue };
        if let Some(a) = crate::art::cached(&url, px) {
            if let Some(mut row) = model.row_data(i) {
                row.art = slint::Image::from_rgb8(a.buf);
                model.set_row_data(i, row);
            }
            continue;
        }
        let weak = SendWeak(Rc::downgrade(model));
        let uri = model.row_data(i).map(|r| r.uri.to_string()).unwrap_or_default();
        crate::runtime().spawn(async move {
            let Some((a, fresh)) = crate::art::load_fresh(&url, px).await else { return };
            let _ = slint::invoke_from_event_loop(move || {
                let weak = weak;
                let Some(model) = weak.0.upgrade() else { return };
                if let Some(mut row) = model.row_data(i).filter(|r| r.uri.as_str() == uri) {
                    row.art = slint::Image::from_rgb8(a.buf);
                    row.fresh = fresh;
                    model.set_row_data(i, row);
                }
            });
        });
    }
}

/// `invoke_from_event_loop` needs `Send`; the weak model is only touched back on the UI thread.
struct SendWeak<T>(std::rc::Weak<T>);
unsafe impl<T> Send for SendWeak<T> {}

/// Reloads the panel from librespot's queue. Cheap when the panel is closed (no metadata).
pub fn refresh(ui: Weak<MainWindow>, rt: &tokio::runtime::Handle) {
    let Some(p) = crate::PLAYER.get().cloned() else { return };
    let g = GEN.fetch_add(1, Ordering::Relaxed) + 1;
    rt.spawn(async move {
        let Some(snap) = p.queue_snapshot().await else { return };
        let queued: Vec<_> = snap.next.iter().filter(|i| i.queued).collect();
        // Forget Play Next uids that already played or were removed.
        let play_next: Vec<String> = {
            let mut pn = lock(&PLAY_NEXT);
            pn.retain(|uid| queued.iter().any(|q| &q.uid == uid));
            pn.clone()
        };
        *lock(&QUEUED) = queued.iter().map(|q| q.uri.clone()).collect();
        FROM_QUEUE.store(snap.current.as_ref().is_some_and(|c| c.queued), Ordering::Relaxed);
        // Player bar "Autoplaying similar songs": the current track's provider is autoplay.
        let autoplaying = snap.current.as_ref().is_some_and(|c| c.autoplay);
        let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<crate::Playback>().set_autoplaying(autoplaying));
        save_now(&p);
        if !PANEL_OPEN.load(Ordering::Relaxed) {
            return;
        }

        let mut next = Vec::new();
        let mut remaining = 0u64;
        for it in snap.next.iter().take(SHOW_NEXT) {
            let m = meta(&p, &it.uri).await;
            remaining += m.as_ref().map_or(0, |m| m.duration_ms as u64);
            next.push((it.uri.clone(), it.uid.clone(), it.queued, play_next.contains(&it.uid), m));
        }
        let mut recent = Vec::new();
        for it in snap.prev.iter().rev().take(10) {
            recent.push((it.uri.clone(), meta(&p, &it.uri).await));
        }
        let context = p.context();
        let context_name = context_name(&p, &context).await;
        let queued_count = queued.len() as i32;
        let more = snap.next.len().saturating_sub(SHOW_NEXT);
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if GEN.load(Ordering::Relaxed) != g {
                return;
            }
            let q = ui.global::<Queue>();
            let scale = ui.window().scale_factor();
            let rows: Vec<QueueRow> = next.iter().map(|(u, id, qd, pn, m)| row(u, id, *qd, *pn, m.as_ref())).collect();
            let model = Rc::new(VecModel::from(rows));
            load_art(&model, next.iter().map(|n| n.4.clone()).collect(), scale);
            q.set_next(model.into());
            let rec: Vec<QueueRow> = recent.iter().map(|(u, m)| row(u, "", false, false, m.as_ref())).collect();
            let model = Rc::new(VecModel::from(rec));
            load_art(&model, recent.into_iter().map(|r| r.1).collect(), scale);
            q.set_recent(model.into());
            q.set_queued_count(queued_count);
            q.set_context_name(context_name.into());
            let n = next.len() + more;
            let tracks = if n == 1 { "1 track".to_string() } else { format!("{n} tracks") };
            q.set_summary(if n == 0 {
                "".into()
            } else if more > 0 {
                format!("{tracks} · {}+ listed", total(remaining)).into()
            } else {
                format!("{tracks} · {}", total(remaining)).into()
            });
        });
    });
}

async fn context_name(p: &Player, context: &str) -> String {
    if context.ends_with(":collection") {
        return "Liked Songs".into();
    }
    if context.starts_with("spotify:track:") || context.is_empty() {
        return String::new();
    }
    if let Some(name) = crate::browse::known_title(context) {
        return name;
    }
    if context.starts_with("spotify:album:") {
        // Any track of the album names it; the metadata cache usually has the current one.
        if let Some(cur) = p.queue_snapshot().await.and_then(|s| s.current)
            && let Some(m) = meta(p, &cur.uri).await
        {
            return m.album;
        }
    }
    String::new()
}

/// Queues `uri` (track or episode). Nothing playing on Cadence yet: plays it instead.
pub fn add(ui: &MainWindow, uri: &str, play_next: bool) -> &'static str {
    let Some(p) = crate::PLAYER.get().cloned() else { return "Playback isn't ready yet." };
    if ui.global::<Playback>().get_title().is_empty() || !crate::is_local_active() {
        p.play(uri, 0);
        return "Playing now: nothing was playing on Cadence.";
    }
    let rt = crate::runtime();
    let (weak, uri) = (ui.as_weak(), uri.to_string());
    rt.spawn(async move {
        let pos = if play_next {
            // After the Play Next items still waiting.
            let snap = p.queue_snapshot().await;
            let pn = lock(&PLAY_NEXT).clone();
            Some(snap.map_or(0, |s| s.next.iter().filter(|i| i.queued && pn.contains(&i.uid)).count()))
        } else {
            None
        };
        p.queue_insert(&uri, pos);
        if let (Some(pos), Some(snap)) = (pos, p.queue_snapshot().await)
            && let Some(it) = snap.next.iter().filter(|i| i.queued).nth(pos)
        {
            lock(&PLAY_NEXT).push(it.uid.clone());
        }
        refresh(weak, &tokio::runtime::Handle::current());
    });
    if play_next { "Playing next" } else { "Added to queue" }
}

pub fn init(ui: &MainWindow) {
    let q = ui.global::<Queue>();
    let w = ui.as_weak();
    q.on_remove(move |uid| {
        crate::with_player(|p| p.queue_remove(&uid));
        lock(&PLAY_NEXT).retain(|u| *u != uid.as_str());
        refresh(w.clone(), &crate::runtime());
    });
    let w = ui.as_weak();
    q.on_move_to(move |uid, pos| {
        let pos = pos.max(0) as usize;
        crate::with_player(|p| p.queue_move(&uid, pos));
        // A moved item keeps or loses Play Next status by where it lands.
        {
            let mut pn = lock(&PLAY_NEXT);
            pn.retain(|u| *u != uid.as_str());
            if pos < pn.len() {
                pn.insert(pos, uid.to_string());
            }
        }
        refresh(w.clone(), &crate::runtime());
    });
    let w = ui.as_weak();
    q.on_clear(move || {
        crate::with_player(|p| p.queue_clear());
        lock(&PLAY_NEXT).clear();
        refresh(w.clone(), &crate::runtime());
    });
    // Skips forward to upcoming row `index` (queue or context).
    let w = ui.as_weak();
    q.on_play_row(move |index| {
        let n = index.max(0) as usize + 1;
        crate::with_player(|p| {
            for _ in 0..n.min(SHOW_NEXT) {
                p.next();
            }
        });
        refresh(w.clone(), &crate::runtime());
    });
}

// ---------------------------------------------------------------------------------------------
// Session persistence.

#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub context: String,
    pub track: String,
    pub position_ms: u32,
    pub queued: Vec<String>,
    /// The track was a queued item: resume it on its own (it may not be in the context).
    pub from_queue: bool,
}

impl Saved {
    fn to_json(&self) -> Value {
        json!({ "context": self.context, "track": self.track, "position_ms": self.position_ms, "queued": self.queued, "from_queue": self.from_queue })
    }

    fn from_json(v: &Value) -> Option<Saved> {
        let track = v["track"].as_str()?.to_string();
        if track.is_empty() {
            return None;
        }
        Some(Saved {
            context: v["context"].as_str().unwrap_or("").to_string(),
            track,
            position_ms: v["position_ms"].as_u64().unwrap_or(0) as u32,
            queued: v["queued"].as_array().map_or(Vec::new(), |a| a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect()),
            from_queue: v["from_queue"].as_bool().unwrap_or(false),
        })
    }
}

fn path() -> std::path::PathBuf {
    crate::auth::data_dir().join("queue.json")
}

/// Current track and position, set by the event loop.
static NOW: Mutex<(String, u32)> = Mutex::new((String::new(), 0));

pub fn set_now(track: &str, position_ms: u32) {
    *lock(&NOW) = (track.to_string(), position_ms);
}

pub fn set_now_position(position_ms: u32) {
    lock(&NOW).1 = position_ms;
}

fn save_now(p: &Player) {
    let (track, position_ms) = lock(&NOW).clone();
    if track.is_empty() {
        return;
    }
    let s = Saved { context: p.context(), track, position_ms, queued: lock(&QUEUED).clone(), from_queue: FROM_QUEUE.load(Ordering::Relaxed) };
    let tmp = path().with_extension("json.tmp");
    let _ = std::fs::write(&tmp, s.to_json().to_string()).and_then(|_| std::fs::rename(&tmp, path()));
}

/// Called on pause, track change and exit.
pub fn save() {
    if let Some(p) = crate::PLAYER.get() {
        save_now(p);
    }
}

/// At startup: shows the saved track in the bar (paused) and arms `resume`.
pub fn load_saved(ui: Weak<MainWindow>, rt: &tokio::runtime::Handle) {
    let Some(saved) = std::fs::read(path()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| Saved::from_json(&v)) else {
        return;
    };
    let Some(p) = crate::PLAYER.get().cloned() else { return };
    *lock(&RESTORE) = Some(saved.clone());
    rt.spawn(async move {
        let Some(m) = meta(&p, &saved.track).await else { return };
        let _ = ui.upgrade_in_event_loop(move |ui| {
            // Something already started playing: leave it.
            if lock(&RESTORE).is_none() || !ui.global::<Playback>().get_title().is_empty() {
                return;
            }
            crate::show_restored(&ui, &saved, &m);
        });
    });
}

/// First Play after launch: loads the saved session. Returns false when there is none.
pub fn resume() -> bool {
    let Some(s) = lock(&RESTORE).take() else { return false };
    let Some(p) = crate::PLAYER.get().cloned() else { return false };
    // A queued or single track resumes on its own; otherwise the context at that track.
    let alone = s.from_queue || s.context.is_empty() || s.context.starts_with("spotify:track:");
    let (ctx, track) = if alone {
        (s.track.clone(), librespot_connect::PlayingTrack::Index(0))
    } else {
        (s.context.clone(), librespot_connect::PlayingTrack::Uri(s.track.clone()))
    };
    crate::logln!("queue: resuming session ({} queued)", s.queued.len());
    p.load(&ctx, track, s.position_ms, true);
    // Spirc handles commands in order, so these land after the load.
    for uri in &s.queued {
        p.queue_insert(uri, None);
    }
    true
}

/// Any explicit play replaces the saved session.
pub fn forget_restore() {
    lock(&RESTORE).take();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_round_trip() {
        let s = Saved { context: "spotify:album:x".into(), track: "spotify:track:y".into(), position_ms: 1234, queued: vec!["spotify:track:z".into()], from_queue: true };
        assert_eq!(Saved::from_json(&s.to_json()), Some(s));
        assert_eq!(Saved::from_json(&json!({"track": ""})), None);
    }

    #[test]
    fn totals() {
        assert_eq!(total(14 * 60_000), "14 min");
        assert_eq!(total(72 * 60_000), "1 hr 12 min");
        assert_eq!(mmss(61_000), "1:01");
    }
}

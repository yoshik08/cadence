//! Context menus (ui/menu.slint): tracks, episodes, cards (playlists, albums, artists, podcasts,
//! folders), the ⋯ menu of each page, the sleep timer and playback speed. Items only list actions
//! that work for the target; Web API actions report the real outcome in a toast.

use crate::api::{self, arr, s};
use crate::db;
use crate::web::ApiError;
use crate::{Browse, MainWindow, Menu, MenuItem, Playback};
use reqwest::Method;
use serde_json::json;
use slint::{ComponentHandle, VecModel, Weak};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Default)]
struct Target {
    uri: String,
    /// Playlist / album / collection the row belongs to ("" for search results).
    context: String,
    index: i32,
    /// Queue uid when opened from the queue panel.
    uid: String,
    liked: Option<bool>,
    /// "row" (track / episode / card) | "list" (page ⋯) | "sleep" | "speed".
    mode: &'static str,
}

thread_local! {
    static TARGET: RefCell<Target> = RefCell::new(Target::default());
    static UI: RefCell<Option<Weak<MainWindow>>> = const { RefCell::new(None) };
}

static OPEN_GEN: AtomicU64 = AtomicU64::new(0);
static TOAST_GEN: AtomicU64 = AtomicU64::new(0);

fn ui() -> Option<MainWindow> {
    UI.with(|u| u.borrow().as_ref().and_then(|w| w.upgrade()))
}

fn kind(uri: &str) -> &str {
    if uri == "liked" || uri.ends_with(":collection") {
        return "collection";
    }
    if uri.starts_with("folder:") {
        return "folder";
    }
    if uri == "local" {
        return "local";
    }
    uri.split(':').nth(1).unwrap_or("")
}

fn item(label: &str, action: &str) -> MenuItem {
    MenuItem { label: label.into(), action: action.into(), enabled: true, separator: false, sub: false }
}

fn sep(mut m: MenuItem) -> MenuItem {
    m.separator = true;
    m
}

fn web_item(label: &str, action: &str) -> MenuItem {
    MenuItem { enabled: crate::web().is_some(), ..item(label, action) }
}

fn sub(m: MenuItem) -> MenuItem {
    MenuItem { sub: true, ..m }
}

fn owned_playlist(uri: &str) -> bool {
    let me = crate::browse::my_id();
    db::library_get(uri).is_some_and(|l| me.as_deref() == Some(l.owner_id.as_str()))
}

fn row_items(ui: &MainWindow, t: &Target) -> Vec<MenuItem> {
    let current = ui.global::<Playback>().get_title() != "" && ui.global::<Browse>().get_current_uri() == t.uri.as_str();
    let mut v = Vec::new();
    match kind(&t.uri) {
        "track" | "episode" | "local" if t.uri != "local" => {
            v.push(item("Play", "play"));
            v.push(item("Play next", "next"));
            v.push(item("Add to queue", "queue"));
            if kind(&t.uri) == "episode" {
                let played = db::episode_progress(&t.uri).is_some_and(|p| p.2);
                v.push(sep(item(if played { "Mark as unplayed" } else { "Mark as played" }, "played")));
            } else if kind(&t.uri) == "track" {
                let like = if t.liked == Some(true) { "Remove from Liked Songs" } else { "Save to Liked Songs" };
                v.push(sep(MenuItem { enabled: crate::web().is_some() && t.liked.is_some(), ..item(like, "like") }));
                v.push(sub(web_item("Add to playlist", "addpl")));
                v.push(web_item("Create playlist with this song", "newpl"));
                v.push(sep(item("Go to album", "album")));
                v.push(item("Go to artist", "artist"));
            }
            if !t.uri.starts_with("spotify:local:") {
                v.push(sep(item("Copy link", "link")));
                v.push(item("Copy Spotify URI", "uri"));
            }
            if current {
                v.push(item("View lyrics", "lyrics"));
            }
            if !t.uid.is_empty() {
                v.push(sep(item("Remove from queue", "rmqueue")));
            } else if kind(&t.context) == "playlist" && crate::browse::is_readable(&t.context) {
                v.push(sep(web_item("Remove from this playlist", "rmpl")));
            }
        }
        "album" | "playlist" | "collection" | "show" | "artist" | "folder" | "local" => {
            let k = kind(&t.uri);
            if k != "folder" {
                v.push(item("Play", "play"));
            }
            if k == "album" || k == "playlist" {
                v.push(web_item("Add to queue", "queue-all"));
            }
            if k == "playlist" {
                let pinned = db::pins().iter().any(|p| *p == t.uri);
                v.push(sep(item(if pinned { "Unpin from sidebar" } else { "Pin to sidebar" }, "pin")));
                v.push(sub(item("Move to folder", "folder")));
            }
            if matches!(k, "album" | "show" | "artist") || (k == "playlist" && !owned_playlist(&t.uri)) {
                let saved = t.liked == Some(true) || db::library_contains(&t.uri);
                let (on, off) = match k {
                    "artist" | "show" => ("Unfollow", "Follow"),
                    _ => ("Remove from Your Library", "Save to Your Library"),
                };
                v.push(sep(web_item(if saved { on } else { off }, if saved { "unsave" } else { "save" })));
            }
            if k == "album" {
                v.push(item("Go to artist", "artist"));
            }
            if k == "folder" {
                v.push(item("Rename folder", "folder-rename"));
                v.push(item("Delete folder", "folder-delete"));
            }
            if matches!(k, "album" | "playlist" | "show" | "artist") {
                v.push(sep(item("Copy link", "link")));
                v.push(item("Copy Spotify URI", "uri"));
            }
        }
        _ => {}
    }
    v
}

/// The ⋯ menu of the current page.
fn list_items(ui: &MainWindow) -> Vec<MenuItem> {
    let b = ui.global::<Browse>();
    let (kind_s, uri) = (b.get_list_kind().to_string(), b.get_list_uri().to_string());
    let (_, _, public, collab, owner) = crate::browse::page_info();
    let owned = crate::browse::my_id().is_some_and(|me| me == owner);
    let mut v = Vec::new();
    match kind_s.as_str() {
        "playlist" => {
            v.push(web_item("Add to queue", "queue-all"));
            if owned {
                v.push(sep(web_item("Edit details", "edit")));
                v.push(web_item(if public { "Make private" } else { "Make public" }, "public"));
                v.push(MenuItem { enabled: crate::web().is_some() && (!public || collab), ..item(if collab { "Turn off collaborative" } else { "Make collaborative" }, "collab") });
                v.push(item("Invite collaborators (copy link)", "invite"));
            }
            v.push(sep(sub(web_item("Add to other playlist", "addpl-all"))));
            v.push(sub(item("Move to folder", "folder")));
            let pinned = db::pins().iter().any(|p| *p == uri);
            v.push(item(if pinned { "Unpin from sidebar" } else { "Pin to sidebar" }, "pin"));
            if b.get_list_editable() {
                v.push(sep(web_item("Find duplicates", "dupes")));
            }
            v.push(item("Export to CSV", "csv"));
            v.push(sep(item("Copy link", "link")));
            v.push(item("Copy Spotify URI", "uri"));
            v.push(sep(web_item(if owned { "Delete" } else { "Remove from Your Library" }, "delete")));
        }
        "album" => {
            v.push(web_item("Add to queue", "queue-all"));
            v.push(sub(web_item("Add to playlist", "addpl-all")));
            v.push(sep(item("Go to artist", "owner")));
            v.push(sep(item("Copy link", "link")));
            v.push(item("Copy Spotify URI", "uri"));
        }
        "liked" | "local" | "artist-liked" => {
            v.push(item("Export to CSV", "csv"));
            if kind_s == "local" {
                v.push(item("Rescan folders", "rescan"));
            }
        }
        "artist" | "show" => {
            let on = if kind_s == "artist" { b.get_following() } else { b.get_list_saved() };
            v.push(web_item(if on { "Unfollow" } else { "Follow" }, "follow"));
            v.push(sep(item("Copy link", "link")));
            v.push(item("Copy Spotify URI", "uri"));
        }
        "folder" => {
            v.push(item("Rename folder", "folder-rename"));
            v.push(item("Delete folder", "folder-delete"));
        }
        _ => {}
    }
    v
}

fn show(ui: &MainWindow, title: &str, items: Vec<MenuItem>) {
    let m = ui.global::<Menu>();
    m.set_title(title.into());
    m.set_items(Rc::new(VecModel::from(items)).into());
    m.set_hot(-1);
    m.set_open(true);
}

fn close(ui: &MainWindow) {
    ui.global::<Menu>().set_open(false);
}

/// A short confirmation at the bottom of the window for 2.5 s.
pub fn toast_text(ui: &MainWindow, text: &str) {
    let g = TOAST_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    ui.global::<Menu>().set_toast(text.into());
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(2600), move || {
        if TOAST_GEN.load(Ordering::Relaxed) == g
            && let Some(ui) = weak.upgrade()
        {
            ui.global::<Menu>().set_toast("".into());
        }
    });
}

fn toast_later(weak: &Weak<MainWindow>, text: String) {
    let _ = weak.upgrade_in_event_loop(move |ui| toast_text(&ui, &text));
}

/// Short user-facing reason for a failed Web API call.
pub fn why(e: &ApiError) -> &'static str {
    match (e, e.code()) {
        (ApiError::Auth(_), _) => "sign in to Spotify first.",
        (_, Some(403)) => "Spotify didn't allow this.",
        (_, Some(404)) => "Spotify couldn't find it.",
        (_, Some(429)) => "Spotify is limiting requests. Try again in a moment.",
        (_, Some(_)) => "Spotify returned an error.",
        _ => "can't reach Spotify.",
    }
}

fn open_at(ui: &MainWindow, t: Target, x: f32, y: f32) {
    let g = OPEN_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let m = ui.global::<Menu>();
    m.set_at_x(x);
    m.set_at_y(y);
    TARGET.with(|c| *c.borrow_mut() = t.clone());
    let items = if t.mode == "list" { list_items(ui) } else { row_items(ui, &t) };
    if items.is_empty() {
        return;
    }
    show(ui, "", items);
    // Liked / saved state, then rebuild the items.
    let k = kind(&t.uri).to_string();
    if t.mode == "row" && crate::web().is_some() && matches!(k.as_str(), "track" | "album" | "artist" | "show" | "playlist") {
        let (weak, uri) = (ui.as_weak(), t.uri.clone());
        crate::runtime().spawn(async move {
            let liked = api::contains(std::slice::from_ref(&uri)).await.ok().and_then(|v| v.first().copied());
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if OPEN_GEN.load(Ordering::Relaxed) != g || !ui.global::<Menu>().get_open() || !ui.global::<Menu>().get_title().is_empty() {
                    return;
                }
                let t = TARGET.with(|c| {
                    c.borrow_mut().liked = liked;
                    c.borrow().clone()
                });
                let hot = ui.global::<Menu>().get_hot();
                show(&ui, "", row_items(&ui, &t));
                ui.global::<Menu>().set_hot(hot);
            });
        });
    }
}

pub fn open_for_card(uri: &str, x: f32, y: f32) {
    if let Some(ui) = ui() {
        open_at(&ui, Target { uri: uri.into(), index: -1, mode: "row", ..Default::default() }, x, y);
    }
}

pub fn open_list_menu(x: f32, y: f32) {
    if let Some(ui) = ui() {
        let uri = ui.global::<Browse>().get_list_uri().to_string();
        open_at(&ui, Target { uri, index: -1, mode: "list", ..Default::default() }, x, y);
    }
}

const SLEEP: [(&str, &str); 9] = [
    ("Off", "sleep:off"),
    ("2 minutes", "sleep:2"),
    ("5 minutes", "sleep:5"),
    ("15 minutes", "sleep:15"),
    ("30 minutes", "sleep:30"),
    ("45 minutes", "sleep:45"),
    ("1 hour", "sleep:60"),
    ("End of this track", "sleep:track"),
    ("Custom…", "sleep:custom"),
];

pub fn init(ui: &MainWindow) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    let m = ui.global::<Menu>();
    let w = ui.as_weak();
    m.on_for_item(move |uri, context, index, uid, x, y| {
        if let Some(ui) = w.upgrade() {
            open_at(&ui, Target { uri: uri.into(), context: context.into(), index, uid: uid.into(), liked: None, mode: "row" }, x, y);
        }
    });
    let w = ui.as_weak();
    m.on_close(move || {
        if let Some(ui) = w.upgrade() {
            close(&ui);
        }
    });
    let w = ui.as_weak();
    m.on_back(move || {
        if let Some(ui) = w.upgrade() {
            let t = TARGET.with(|c| c.borrow().clone());
            let items = if t.mode == "list" { list_items(&ui) } else { row_items(&ui, &t) };
            show(&ui, "", items);
        }
    });
    let w = ui.as_weak();
    m.on_pick(move |action| {
        if let Some(ui) = w.upgrade() {
            let t = TARGET.with(|c| c.borrow().clone());
            pick(&ui, &t, &action);
        }
    });
    let w = ui.as_weak();
    ui.global::<crate::Queue>().on_row_menu(move |uri, uid, x, y| {
        if let Some(ui) = w.upgrade() {
            open_at(&ui, Target { uri: uri.into(), uid: uid.into(), index: -1, mode: "row", ..Default::default() }, x, y);
        }
    });
    let pb = ui.global::<Playback>();
    let w = ui.as_weak();
    pb.on_sleep_menu(move |x, y| {
        if let Some(ui) = w.upgrade() {
            TARGET.with(|c| *c.borrow_mut() = Target { mode: "sleep", ..Default::default() });
            let m = ui.global::<Menu>();
            m.set_at_x(x);
            m.set_at_y(y - (SLEEP.len() as f32 * 34.0 + 50.0));
            show(&ui, "Sleep timer", SLEEP.iter().map(|(l, a)| item(l, a)).collect());
        }
    });
    let w = ui.as_weak();
    pb.on_speed_menu(move |x, y| {
        if let Some(ui) = w.upgrade() {
            TARGET.with(|c| *c.borrow_mut() = Target { mode: "speed", ..Default::default() });
            let m = ui.global::<Menu>();
            let speeds = ["0.5", "0.8", "1", "1.2", "1.5", "1.8", "2"];
            m.set_at_x(x);
            m.set_at_y(y - (speeds.len() as f32 * 34.0 + 50.0));
            show(&ui, "Playback speed", speeds.iter().map(|sp| item(&format!("{sp}×"), &format!("speed:{sp}"))).collect());
        }
    });
}

fn folder_menu(ui: &MainWindow) {
    let mut items: Vec<MenuItem> = db::folders().into_iter().map(|(id, name)| item(&name, &format!("folder-put:{id}"))).collect();
    items.push(sep(item("New folder…", "folder-new")));
    items.push(item("Remove from folder", "folder-put:none"));
    show(ui, "Move to folder (stored in Cadence)", items);
}

fn pick(ui: &MainWindow, t: &Target, action: &str) {
    let weak = ui.as_weak();
    let rt = crate::runtime();
    // Submenus keep the menu open.
    match action {
        "addpl" | "addpl-all" => return add_to_playlist_menu(ui, action == "addpl-all"),
        "folder" => return folder_menu(ui),
        _ => {}
    }
    close(ui);
    let uri = t.uri.clone();
    match action {
        "play" => {
            crate::queue::forget_restore();
            match kind(&uri) {
                "track" | "episode" => {
                    if kind(&uri) == "episode" {
                        crate::podcast::play_episode(&uri, 0);
                    } else if t.context.is_empty() || t.index < 0 {
                        crate::with_player(|p| p.play(&uri, 0));
                    } else {
                        // Same rules as a double-click on the row.
                        ui.global::<Browse>().invoke_play(t.context.as_str().into(), t.index);
                    }
                }
                "local" if uri.starts_with("spotify:local:") => crate::with_player(|p| p.play_list(std::slice::from_ref(&uri), 0, None)),
                _ => ui.global::<Browse>().invoke_play_card(uri.as_str().into()),
            }
        }
        "next" | "queue" => {
            let msg = crate::queue::add(ui, &uri, action == "next");
            toast_text(ui, msg);
        }
        "queue-all" => {
            let uri = if t.mode == "list" { ui.global::<Browse>().get_list_uri().to_string() } else { uri };
            rt.spawn(async move {
                match track_uris(&uri, 100).await {
                    Ok(list) if !list.is_empty() => {
                        let n = list.len();
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            for u in &list {
                                crate::queue::add(&ui, u, false);
                            }
                            toast_text(&ui, &format!("Added {n} tracks to the queue"));
                        });
                    }
                    Ok(_) => toast_later(&weak, "No playable tracks found".into()),
                    Err(e) => toast_later(&weak, format!("Couldn't add them: {}", why(&e))),
                }
            });
        }
        "like" | "save" | "unsave" => {
            let saved_now = if action == "like" { t.liked == Some(true) } else { action == "unsave" };
            let k = kind(&uri).to_string();
            rt.spawn(async move {
                let r = api::set_saved(std::slice::from_ref(&uri), !saved_now).await;
                let place = match k.as_str() {
                    "track" => "Liked Songs",
                    _ => "Your Library",
                };
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    match &r {
                        Ok(()) => {
                            crate::library::note_saved(&uri, !saved_now, None);
                            let verb = match (k.as_str(), saved_now) {
                                ("artist" | "show", true) => "Unfollowed".to_string(),
                                ("artist" | "show", false) => "Following".to_string(),
                                (_, true) => format!("Removed from {place}"),
                                (_, false) => format!("Saved to {place}"),
                            };
                            toast_text(&ui, &verb);
                            crate::refresh_like_state(&ui);
                            let kind_static: &'static str = match k.as_str() {
                                "album" => "album",
                                "artist" => "artist",
                                "show" => "show",
                                "playlist" => "playlist",
                                _ => "track",
                            };
                            crate::browse::refresh_library(kind_static);
                        }
                        Err(e) => toast_text(&ui, &format!("Couldn't update {place}: {}", why(e))),
                    }
                });
            });
        }
        a if a.starts_with("addpl:") || a.starts_with("addpl!:") || a.starts_with("addall:") => {
            let force = a.starts_with("addpl!:");
            let all = a.starts_with("addall:");
            let pl = a.split_once(':').map(|(_, p)| p.to_string()).unwrap_or_default();
            let uris: Vec<String> = if all {
                crate::browse::page_rows().into_iter().map(|r| r.uri).filter(|u| u.starts_with("spotify:track:") || u.starts_with("spotify:episode:")).collect()
            } else {
                vec![uri.clone()]
            };
            rt.spawn(async move { add_to_playlist(weak, &pl, uris, force || all).await });
        }
        "newpl" => crate::dialogs::create_playlist(ui, Some(uri)),
        "album" | "artist" => {
            let want_album = action == "album";
            if kind(&uri) == "album" {
                rt.spawn(async move {
                    if let Ok(v) = api::get(&format!("albums/{}", api::id_of(&uri)), &[]).await
                        && let Some(a) = v["artists"][0]["uri"].as_str()
                    {
                        let a = a.to_string();
                        let _ = weak.upgrade_in_event_loop(move |_| crate::browse::go(&a));
                    }
                });
                return;
            }
            rt.spawn(async move {
                let Some(m) = crate::queue::player_meta(&uri).await else {
                    return toast_later(&weak, "Couldn't look this track up".into());
                };
                let dest = if want_album { m.album_uri } else { m.artist_uri };
                if !dest.is_empty() {
                    let _ = weak.upgrade_in_event_loop(move |_| crate::browse::go(&dest));
                }
            });
        }
        "owner" => {
            let o = ui.global::<Browse>().get_list_owner_uri().to_string();
            if !o.is_empty() {
                crate::browse::go(&o);
            }
        }
        "link" | "invite" => match crate::clip::web_link(&uri) {
            Some(link) if crate::clip::set_text(&link) => toast_text(
                ui,
                if action == "invite" {
                    "Link copied. Spotify doesn't let apps send invites: share this link with the playlist made collaborative."
                } else {
                    "Link copied"
                },
            ),
            _ => toast_text(ui, "Couldn't copy the link"),
        },
        "uri" => toast_text(ui, if crate::clip::set_text(&uri) { "Spotify URI copied" } else { "Couldn't copy" }),
        "lyrics" => ui.global::<Playback>().set_expanded(true),
        "rmqueue" => {
            ui.global::<crate::Queue>().invoke_remove(t.uid.as_str().into());
            toast_text(ui, "Removed from queue");
        }
        "rmpl" => {
            let pl = t.context.clone();
            rt.spawn(async move {
                let r = api::call(Method::DELETE, &format!("playlists/{}/items", api::id_of(&pl)), &[], Some(&json!({ "items": [{ "uri": uri }] }))).await;
                let _ = weak.upgrade_in_event_loop(move |ui| match r {
                    Ok(_) => {
                        toast_text(&ui, "Removed from this playlist");
                        if ui.global::<Browse>().get_list_uri() == pl.as_str() {
                            crate::browse::reload();
                        }
                    }
                    Err(e) => toast_text(&ui, &format!("Couldn't remove it: {}", why(&e))),
                });
            });
        }
        "played" => {
            let played = db::episode_progress(&uri).is_some_and(|p| p.2);
            let dur = db::episode_progress(&uri).map_or(0, |p| p.1);
            db::set_episode_progress(&uri, if played { 0 } else { dur }, dur, !played);
            toast_text(ui, if played { "Marked as unplayed" } else { "Marked as played" });
            if ui.global::<Browse>().get_list_kind() == "show" {
                crate::browse::reload();
            }
        }
        "pin" => {
            let pinned = db::pins().iter().any(|p| *p == uri);
            db::set_pinned(&uri, !pinned);
            crate::browse::reload_sidebar(ui);
            toast_text(ui, if pinned { "Unpinned" } else { "Pinned to the sidebar" });
        }
        a if a.starts_with("folder-put:") => {
            let id = a["folder-put:".len()..].parse::<i64>().ok();
            db::folder_put(&uri, id);
            crate::browse::reload_sidebar(ui);
            toast_text(ui, if id.is_some() { "Moved to folder" } else { "Removed from folder" });
        }
        "folder-new" => crate::dialogs::new_folder_with(ui, Some(uri)),
        "folder-rename" => crate::dialogs::rename_folder(ui, &uri),
        "folder-delete" => {
            let Some(id) = uri.strip_prefix("folder:").and_then(|x| x.parse::<i64>().ok()) else { return };
            crate::dialogs::confirm(ui, "Delete this folder?", "The playlists inside stay in your library; only the Cadence folder goes.", "Delete", true, move |ui| {
                db::folder_delete(id);
                crate::browse::reload_sidebar(ui);
                if ui.global::<Browse>().get_list_uri().as_str() == format!("folder:{id}") {
                    crate::browse::go("library");
                }
            });
        }
        "edit" => crate::dialogs::edit_playlist(ui),
        "public" | "collab" => {
            let (_, _, public, collab, _) = crate::browse::page_info();
            let body = if action == "public" { json!({ "public": !public }) } else if collab { json!({ "collaborative": false }) } else { json!({ "collaborative": true, "public": false }) };
            rt.spawn(async move {
                let r = api::call(Method::PUT, &format!("playlists/{}", api::id_of(&uri)), &[], Some(&body)).await;
                let _ = weak.upgrade_in_event_loop(move |ui| match r {
                    Ok(_) => {
                        toast_text(&ui, "Playlist updated");
                        crate::browse::reload();
                        crate::browse::refresh_library("playlist");
                    }
                    Err(e) => toast_text(&ui, &format!("Couldn't update the playlist: {}", why(&e))),
                });
            });
        }
        "delete" => {
            let owned = owned_playlist(&uri) || crate::browse::my_id().is_some_and(|me| crate::browse::page_info().4 == me);
            let (title, msg, ok) = if owned {
                ("Delete this playlist?", "It disappears from Your Library. Spotify keeps it recoverable for 90 days at spotify.com/account/recover-playlists.", "Delete")
            } else {
                ("Remove from Your Library?", "You can save it again later.", "Remove")
            };
            crate::dialogs::confirm(ui, title, msg, ok, true, move |ui| {
                let (w, uri) = (ui.as_weak(), uri.clone());
                crate::runtime().spawn(async move {
                    let r = api::set_saved(std::slice::from_ref(&uri), false).await;
                    let _ = w.upgrade_in_event_loop(move |ui| match r {
                        Ok(()) => {
                            db::library_set("playlist", None, &uri);
                            db::set_pinned(&uri, false);
                            db::folder_put(&uri, None);
                            crate::browse::reload_sidebar(&ui);
                            toast_text(&ui, if owned { "Playlist deleted" } else { "Removed from Your Library" });
                            crate::browse::go("library");
                        }
                        Err(e) => toast_text(&ui, &format!("Couldn't do that: {}", why(&e))),
                    });
                });
            });
        }
        "dupes" => crate::dialogs::duplicates(ui),
        "csv" => export_csv(ui),
        "rescan" => {
            crate::local::rescan();
            toast_text(ui, "Rescanning local folders…");
        }
        "follow" => {
            if ui.global::<Browse>().get_list_kind() == "artist" {
                ui.global::<Browse>().invoke_toggle_follow();
            } else {
                ui.global::<Browse>().invoke_toggle_save();
            }
        }
        a if a.starts_with("sleep:") => crate::sleep::choose(ui, &a["sleep:".len()..]),
        a if a.starts_with("speed:") => {
            let v: f32 = a["speed:".len()..].parse().unwrap_or(1.0);
            crate::podcast::set_speed(ui, v);
        }
        _ => {}
    }
}

/// Track uris of an album or a readable playlist, at most `max`.
pub async fn track_uris(uri: &str, max: usize) -> Result<Vec<String>, ApiError> {
    let id = api::id_of(uri);
    let (path, playlist) = if uri.starts_with("spotify:album:") { (format!("albums/{id}/tracks"), false) } else { (format!("playlists/{id}/items"), true) };
    let mut out = Vec::new();
    let mut offset = 0usize;
    while out.len() < max {
        let off = offset.to_string();
        let j = api::get_fresh(&path, &[("limit", "50"), ("offset", &off)]).await?;
        let list = arr(&j["items"]);
        for x in list {
            let t = if playlist { api::present(&x["item"], &x["track"]) } else { x };
            let u = s(&t["uri"]);
            if !u.is_empty() && !u.starts_with("spotify:local:") {
                out.push(u.to_string());
            }
        }
        offset += list.len();
        if list.is_empty() || !j["next"].is_string() {
            break;
        }
    }
    out.truncate(max);
    Ok(out)
}

/// (title, uri) of every playlist the user owns or collaborates on (from the library index).
fn writable_playlists() -> Vec<(String, String)> {
    let me = crate::browse::my_id().unwrap_or_default();
    db::library_list("playlist", "", "alpha", 5000)
        .into_iter()
        .filter(|l| l.owner_id == me || l.collaborative)
        .map(|l| (l.title, l.uri))
        .collect()
}

fn add_to_playlist_menu(ui: &MainWindow, all: bool) {
    let current = ui.global::<Browse>().get_list_uri().to_string();
    let prefix = if all { "addall" } else { "addpl" };
    let list = writable_playlists();
    let mut items: Vec<MenuItem> = list.into_iter().filter(|(_, u)| !all || *u != current).map(|(title, uri)| item(&title, &format!("{prefix}:{uri}"))).collect();
    if items.is_empty() {
        items.push(MenuItem { enabled: false, ..item(if crate::browse::my_id().is_none() { "Loading your playlists…" } else { "No playlists you can edit" }, "") });
    }
    let hot = if items.first().is_some_and(|i| i.enabled) { 0 } else { -1 };
    show(ui, "Add to playlist", items);
    ui.global::<Menu>().set_hot(hot);
}

async fn add_to_playlist(weak: Weak<MainWindow>, playlist: &str, uris: Vec<String>, force: bool) {
    let id = api::id_of(playlist).to_string();
    let name = crate::browse::known_title(playlist).or_else(|| db::library_get(playlist).map(|l| l.title)).unwrap_or_else(|| "the playlist".into());
    if uris.is_empty() {
        return toast_later(&weak, "Nothing to add".into());
    }
    if !force
        && let Ok(list) = track_uris(playlist, 10_000).await
        && list.iter().any(|u| *u == uris[0])
    {
        let (pl, name) = (playlist.to_string(), name.clone());
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let m = ui.global::<Menu>();
            show(&ui, &format!("Already in {name}"), vec![item("Add it again", &format!("addpl!:{pl}")), item("Cancel", "cancel")]);
            m.set_hot(1);
        });
        return;
    }
    let mut result = Ok(Default::default());
    for chunk in uris.chunks(100) {
        result = api::call(Method::POST, &format!("playlists/{id}/items"), &[], Some(&json!({ "uris": chunk }))).await;
        if result.is_err() {
            break;
        }
    }
    toast_later(&weak, match result {
        Ok(_) if uris.len() > 1 => format!("Added {} songs to {name}", uris.len()),
        Ok(_) => format!("Added to {name}"),
        Err(e) => format!("Couldn't add to {name}: {}", why(&e)),
    });
}

/// Saves the current list as CSV (native save dialog).
fn export_csv(ui: &MainWindow) {
    let b = ui.global::<Browse>();
    let title = b.get_list_title().to_string();
    let rows = crate::browse::page_rows();
    if rows.is_empty() {
        return toast_text(ui, "Nothing to export yet");
    }
    let safe: String = title.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
    let w = ui.as_weak();
    crate::winint::save_file("Export to CSV", &format!("{}.csv", safe.trim()), &[("CSV", "*.csv")], move |path| {
        let ok = write_csv(&path, &rows);
        if let Some(ui) = w.upgrade() {
            toast_text(&ui, if ok { "Exported" } else { "Couldn't write the file" });
        }
    });
}

fn write_csv(path: &std::path::Path, rows: &[api::TrackData]) -> bool {
    let esc = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut out = String::from("Title,Artist,Album,Duration,Added,Explicit,URI\r\n");
    for r in rows {
        out += &format!(
            "{},{},{},{},{},{},{}\r\n",
            esc(&r.title),
            esc(&r.artist),
            esc(&r.album),
            api::mmss(r.duration_ms),
            esc(&r.added),
            if r.explicit { "yes" } else { "no" },
            esc(&r.uri)
        );
    }
    // UTF-8 BOM so Excel reads accents correctly.
    std::fs::write(path, [b"\xEF\xBB\xBF".as_slice(), out.as_bytes()].concat()).is_ok()
}

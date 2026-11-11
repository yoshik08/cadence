//! Modal dialogs (ui/dialog.slint): confirmations, create playlist, folders, edit details with
//! cover upload, and the duplicate finder. One dialog at a time; its OK action is kept here.

use crate::api::{self, TrackData};
use crate::db;
use crate::{Browse, Dialog, MainWindow};
use reqwest::Method;
use serde_json::json;
use slint::{ComponentHandle, Image};
use std::cell::RefCell;

type Action = Box<dyn Fn(&MainWindow)>;

thread_local! {
    static OK: RefCell<Option<Action>> = RefCell::new(None);
    static EXTRA: RefCell<Option<Action>> = RefCell::new(None);
    /// Cover chosen in Edit details (base64 JPEG).
    static COVER: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

fn reset(d: &Dialog) {
    d.set_title("".into());
    d.set_message("".into());
    d.set_field1_label("".into());
    d.set_field1("".into());
    d.set_field2_label("".into());
    d.set_field2("".into());
    d.set_ok_label("OK".into());
    d.set_cancel_label("Cancel".into());
    d.set_danger(false);
    d.set_extra_label("".into());
    d.set_extra_note("".into());
    d.set_extra_image(Image::default());
    d.set_busy(false);
    EXTRA.with(|e| *e.borrow_mut() = None);
    COVER.with(|c| *c.borrow_mut() = None);
}

fn open(ui: &MainWindow, setup: impl FnOnce(&Dialog), ok: impl Fn(&MainWindow) + 'static) {
    let d = ui.global::<Dialog>();
    reset(&d);
    setup(&d);
    OK.with(|o| *o.borrow_mut() = Some(Box::new(ok)));
    d.set_open(true);
}

pub fn close(ui: &MainWindow) {
    ui.global::<Dialog>().set_open(false);
    OK.with(|o| *o.borrow_mut() = None);
}

pub fn init(ui: &MainWindow) {
    let d = ui.global::<Dialog>();
    let w = ui.as_weak();
    d.on_ok(move || {
        if let Some(ui) = w.upgrade() {
            let f = OK.with(|o| o.borrow_mut().take());
            if let Some(f) = f {
                f(&ui);
                // The action may keep the dialog open (busy) by putting itself back.
                OK.with(|o| {
                    if o.borrow().is_none() && !ui.global::<Dialog>().get_busy() {
                        ui.global::<Dialog>().set_open(false);
                    }
                });
            }
        }
    });
    let w = ui.as_weak();
    d.on_cancel(move || {
        if let Some(ui) = w.upgrade() {
            close(&ui);
        }
    });
    let w = ui.as_weak();
    d.on_extra(move || {
        if let Some(ui) = w.upgrade() {
            let f = EXTRA.with(|e| e.borrow_mut().take());
            if let Some(f) = f {
                f(&ui);
                EXTRA.with(|e| {
                    if e.borrow().is_none() {
                        *e.borrow_mut() = Some(f);
                    }
                });
            }
        }
    });
}

pub fn confirm(ui: &MainWindow, title: &str, message: &str, ok_label: &str, danger: bool, ok: impl Fn(&MainWindow) + 'static) {
    open(
        ui,
        |d| {
            d.set_title(title.into());
            d.set_message(message.into());
            d.set_ok_label(ok_label.into());
            d.set_danger(danger);
        },
        ok,
    );
}

/// Create playlist (optionally with a first song). Opens the new playlist.
pub fn create_playlist(ui: &MainWindow, with_track: Option<String>) {
    if crate::web().is_none() {
        return crate::menu::toast_text(ui, "Sign in to Spotify to create playlists.");
    }
    let initial = with_track.as_ref().and_then(|u| crate::browse::known_title(u)).unwrap_or_default();
    open(
        ui,
        |d| {
            d.set_title("Create playlist".into());
            d.set_field1_label("Name".into());
            d.set_field1(if initial.is_empty() { "My playlist".into() } else { initial.as_str().into() });
            d.set_field2_label("Description (optional)".into());
            d.set_ok_label("Create".into());
        },
        move |ui| {
            let d = ui.global::<Dialog>();
            let name = d.get_field1().trim().to_string();
            if name.is_empty() {
                return;
            }
            let desc = d.get_field2().trim().to_string();
            let (w, track) = (ui.as_weak(), with_track.clone());
            crate::runtime().spawn(async move {
                let mut body = json!({ "name": name, "public": false });
                if !desc.is_empty() {
                    body["description"] = json!(desc);
                }
                let r = api::call(Method::POST, "me/playlists", &[], Some(&body)).await;
                let res = match r {
                    Ok(v) => {
                        let id = api::s(&v["id"]).to_string();
                        let added = match &track {
                            Some(t) => api::call(Method::POST, &format!("playlists/{id}/items"), &[], Some(&json!({ "uris": [t] }))).await.map(|_| ()),
                            None => Ok(()),
                        };
                        Ok((format!("spotify:playlist:{id}"), added))
                    }
                    Err(e) => Err(e),
                };
                crate::library::sync_kind("playlist").await;
                let _ = w.upgrade_in_event_loop(move |ui| match res {
                    Ok((uri, added)) => {
                        crate::browse::reload_sidebar(&ui);
                        match added {
                            Ok(()) => crate::menu::toast_text(&ui, &format!("Created \u{201c}{name}\u{201d}")),
                            Err(e) => crate::menu::toast_text(&ui, &format!("Created \u{201c}{name}\u{201d}, but adding the song failed: {}", crate::menu::why(&e))),
                        }
                        crate::browse::go(&uri);
                    }
                    Err(e) => crate::menu::toast_text(&ui, &format!("Couldn't create the playlist: {}", crate::menu::why(&e))),
                });
            });
        },
    );
}

pub fn new_folder(ui: &MainWindow) {
    new_folder_with(ui, None);
}

/// A Cadence-local folder (stored on this PC), optionally moving a playlist into it.
pub fn new_folder_with(ui: &MainWindow, playlist: Option<String>) {
    open(
        ui,
        |d| {
            d.set_title("New folder".into());
            d.set_message("Folders are stored in Cadence on this PC. Spotify doesn't let apps create real playlist folders.".into());
            d.set_field1_label("Name".into());
            d.set_field1("New folder".into());
            d.set_ok_label("Create".into());
        },
        move |ui| {
            let name = ui.global::<Dialog>().get_field1().trim().to_string();
            if name.is_empty() {
                return;
            }
            let id = db::folder_create(&name);
            if let Some(p) = &playlist {
                db::folder_put(p, Some(id));
            }
            crate::browse::reload_sidebar(ui);
            crate::menu::toast_text(ui, "Folder created");
        },
    );
}

pub fn rename_folder(ui: &MainWindow, uri: &str) {
    let Some(id) = uri.strip_prefix("folder:").and_then(|x| x.parse::<i64>().ok()) else { return };
    let name = db::folders().into_iter().find(|f| f.0 == id).map(|f| f.1).unwrap_or_default();
    open(
        ui,
        |d| {
            d.set_title("Rename folder".into());
            d.set_field1_label("Name".into());
            d.set_field1(name.as_str().into());
            d.set_ok_label("Rename".into());
        },
        move |ui| {
            let n = ui.global::<Dialog>().get_field1().trim().to_string();
            if !n.is_empty() {
                db::folder_rename(id, &n);
                crate::browse::reload_sidebar(ui);
            }
        },
    );
}

/// Edit details: name, description and cover (ugc-image-upload) of an owned playlist.
pub fn edit_playlist(ui: &MainWindow) {
    let uri = ui.global::<Browse>().get_list_uri().to_string();
    let (title, desc, ..) = crate::browse::page_info();
    open(
        ui,
        |d| {
            d.set_title("Edit details".into());
            d.set_field1_label("Name".into());
            d.set_field1(title.as_str().into());
            d.set_field2_label("Description".into());
            d.set_field2(desc.as_str().into());
            d.set_ok_label("Save".into());
            d.set_extra_label("Choose image…".into());
            d.set_extra_note("JPEG or PNG; Cadence crops it square.".into());
        },
        move |ui| {
            let d = ui.global::<Dialog>();
            let name = d.get_field1().trim().to_string();
            if name.is_empty() {
                return;
            }
            let description = d.get_field2().trim().to_string();
            let cover = COVER.with(|c| c.borrow_mut().take());
            let id = api::id_of(&uri).to_string();
            d.set_busy(true);
            let w = ui.as_weak();
            crate::runtime().spawn(async move {
                let r1 = api::call(Method::PUT, &format!("playlists/{id}"), &[], Some(&json!({ "name": name, "description": description }))).await;
                let r2 = match cover {
                    Some(jpg) => match crate::web() {
                        Some(web) => web.put_bytes(&format!("playlists/{id}/images"), "image/jpeg", jpg).await,
                        None => Err(api::not_signed_in()),
                    },
                    None => Ok(()),
                };
                api::library_changed();
                let _ = w.upgrade_in_event_loop(move |ui| {
                    ui.global::<Dialog>().set_busy(false);
                    close(&ui);
                    match (r1, r2) {
                        (Ok(_), Ok(())) => crate::menu::toast_text(&ui, "Playlist updated"),
                        (Err(e), _) => crate::menu::toast_text(&ui, &format!("Couldn't save the details: {}", crate::menu::why(&e))),
                        (Ok(_), Err(e)) => crate::menu::toast_text(&ui, &format!("Details saved, but the cover upload failed: {}", crate::menu::why(&e))),
                    }
                    crate::browse::reload();
                    crate::browse::refresh_library("playlist");
                });
            });
        },
    );
    EXTRA.with(|e| {
        *e.borrow_mut() = Some(Box::new(|ui: &MainWindow| {
            let w = ui.as_weak();
            crate::winint::pick_file("Choose a playlist cover", &[("Images", "*.jpg;*.jpeg;*.png")], move |path| {
                let Some(ui) = w.upgrade() else { return };
                match crate::art::cover_jpeg_base64(&path) {
                    Some((b64, preview)) => {
                        COVER.with(|c| *c.borrow_mut() = Some(b64));
                        let d = ui.global::<Dialog>();
                        d.set_extra_image(Image::from_rgb8(preview));
                        d.set_extra_note("New cover ready; it uploads when you save.".into());
                    }
                    None => ui.global::<Dialog>().set_extra_note("Couldn't read that image (8-bit JPEG or PNG).".into()),
                }
            });
        }))
    });
}

/// Duplicate finder: same track more than once. Removing keeps the first copy of each.
pub fn duplicates(ui: &MainWindow) {
    let rows = crate::browse::page_rows();
    let dupes = find_duplicates(&rows);
    if dupes.is_empty() {
        return crate::menu::toast_text(ui, "No duplicates in this playlist");
    }
    let extra: usize = dupes.iter().map(|d| d.positions.len() - 1).sum();
    let names: Vec<String> = dupes.iter().take(6).map(|d| format!("\u{2022} {} ({}×)", d.title, d.positions.len())).collect();
    let more = if dupes.len() > 6 { format!("\nand {} more", dupes.len() - 6) } else { String::new() };
    let uri = ui.global::<Browse>().get_list_uri().to_string();
    confirm(
        ui,
        &format!("{} songs appear more than once", dupes.len()),
        &format!("{}{more}\n\nRemoving keeps the first copy of each ({extra} extra copies go).", names.join("\n")),
        "Remove duplicates",
        true,
        move |ui| {
            let (w, uri, dupes) = (ui.as_weak(), uri.clone(), dupes.clone());
            crate::runtime().spawn(async move {
                let r = remove_duplicates(&uri, &dupes).await;
                let _ = w.upgrade_in_event_loop(move |ui| {
                    match r {
                        Ok(n) => crate::menu::toast_text(&ui, &format!("Removed {n} duplicate copies")),
                        Err(e) => crate::menu::toast_text(&ui, &format!("Couldn't remove all duplicates: {}", crate::menu::why(&e))),
                    }
                    crate::browse::reload();
                });
            });
        },
    );
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dupe {
    pub uri: String,
    pub title: String,
    /// Positions in the playlist, ascending.
    pub positions: Vec<usize>,
}

pub fn find_duplicates(rows: &[TrackData]) -> Vec<Dupe> {
    let mut seen: Vec<Dupe> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if r.uri.is_empty() || r.uri.starts_with("spotify:local:") {
            continue;
        }
        match seen.iter_mut().find(|d| d.uri == r.uri) {
            Some(d) => d.positions.push(i),
            None => seen.push(Dupe { uri: r.uri.clone(), title: r.title.clone(), positions: vec![i] }),
        }
    }
    seen.retain(|d| d.positions.len() > 1);
    seen
}

/// The removal plan: Spotify's DELETE removes every copy of a uri, so each duplicated song is
/// deleted and one copy re-added at its first position. Positions shift as earlier songs lose
/// copies; this returns (uri, insert position) in the order to apply.
pub fn removal_plan(dupes: &[Dupe]) -> Vec<(String, usize)> {
    let mut ds: Vec<&Dupe> = dupes.iter().collect();
    ds.sort_by_key(|d| d.positions[0]);
    let mut removed_before: Vec<usize> = Vec::new();
    let mut plan = Vec::new();
    for d in ds {
        let first = d.positions[0];
        // Extra copies of earlier-processed songs that sat before `first` are gone by now.
        let shift = removed_before.iter().filter(|&&p| p < first).count();
        plan.push((d.uri.clone(), first - shift));
        removed_before.extend(d.positions[1..].iter().copied());
    }
    plan
}

async fn remove_duplicates(playlist: &str, dupes: &[Dupe]) -> Result<usize, crate::web::ApiError> {
    let id = api::id_of(playlist).to_string();
    let mut removed = 0;
    for (uri, pos) in removal_plan(dupes) {
        api::call(Method::DELETE, &format!("playlists/{id}/items"), &[], Some(&json!({ "items": [{ "uri": uri }] }))).await?;
        api::call(Method::POST, &format!("playlists/{id}/items"), &[], Some(&json!({ "uris": [uri], "position": pos }))).await?;
        removed += dupes.iter().find(|d| d.uri == uri).map_or(0, |d| d.positions.len() - 1);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(uri: &str) -> TrackData {
        TrackData { uri: uri.into(), title: uri.into(), ..Default::default() }
    }

    #[test]
    fn plan_keeps_first_copies_in_place() {
        // a b a c b a  ->  a b c
        let rows: Vec<TrackData> = ["a", "b", "a", "c", "b", "a"].iter().map(|u| t(u)).collect();
        let d = find_duplicates(&rows);
        assert_eq!(d.len(), 2);
        let plan = removal_plan(&d);
        // Simulate Spotify: delete all copies, insert one at pos.
        let mut list: Vec<String> = rows.iter().map(|r| r.uri.clone()).collect();
        for (u, pos) in plan {
            list.retain(|x| *x != u);
            list.insert(pos.min(list.len()), u);
        }
        assert_eq!(list, vec!["a", "b", "c"]);
    }

    #[test]
    fn plan_with_shift() {
        // x a y a b z b  ->  x a y b z
        let rows: Vec<TrackData> = ["x", "a", "y", "a", "b", "z", "b"].iter().map(|u| t(u)).collect();
        let plan = removal_plan(&find_duplicates(&rows));
        let mut list: Vec<String> = rows.iter().map(|r| r.uri.clone()).collect();
        for (u, pos) in plan {
            list.retain(|x| *x != u);
            list.insert(pos.min(list.len()), u);
        }
        assert_eq!(list, vec!["x", "a", "y", "b", "z"]);
    }
}

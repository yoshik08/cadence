#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "karaoke")]
mod apple;
mod api;
mod art;
mod audio;
mod auth;
mod browse;
mod cache;
mod clip;
mod db;
mod dialogs;
mod dsp;
mod library;
mod local;
mod log;
mod lyrics_view;
mod mem;
mod menu;
mod mini;
mod player;
mod podcast;
mod prefs;
mod queue;
mod settings;
mod sleep;
mod smtc;
mod stats;
mod taskbar;
mod thumbbar;
mod web;
mod winint;

use player::{Event, Player};
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

slint::include_modules!();

static SMOKE_FAILED: AtomicBool = AtomicBool::new(false);
/// Set once streaming is up; shared by the UI callbacks and the SMTC key handler.
static PLAYER: OnceLock<Arc<Player>> = OnceLock::new();
/// Bumped on every Track event; a slow art / lyrics load for an earlier track checks it before applying.
static TRACK_GEN: AtomicU64 = AtomicU64::new(0);
/// Days left (+1) on the Spotify sign-in when it ends within two weeks; 0 = fine.
static AUTH_WARNING: AtomicU64 = AtomicU64::new(0);
/// Playback was paused because Bluetooth audio disconnected (resume when it returns).
static PAUSED_BY_BT: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The main window, for modules that act on it from callbacks.
    static MAIN: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
}

fn main_ui() -> Option<MainWindow> {
    MAIN.with(|m| m.borrow().as_ref().and_then(|w| w.upgrade()))
}

/// Shows and focuses the main window (tray, mini player).
pub fn show_main() {
    if let Some(ui) = main_ui() {
        let _ = ui.show();
        ui.window().set_minimized(false);
        if let Some(h) = hwnd(ui.window()) {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(windows::Win32::Foundation::HWND(h as *mut _));
            }
        }
    }
}

/// Liked state of the playing track for the bar heart (Web API).
pub fn refresh_like_state(ui: &MainWindow) {
    let uri = ui.global::<Browse>().get_current_uri().to_string();
    let pb = ui.global::<Playback>();
    if !uri.starts_with("spotify:track:") || web().is_none() {
        pb.set_like_known(false);
        return;
    }
    let w = ui.as_weak();
    runtime().spawn(async move {
        let liked = api::contains(std::slice::from_ref(&uri)).await.ok().and_then(|v| v.first().copied());
        let _ = w.upgrade_in_event_loop(move |ui| {
            if ui.global::<Browse>().get_current_uri() == uri.as_str() {
                let pb = ui.global::<Playback>();
                pb.set_like_known(liked.is_some());
                pb.set_liked(liked.unwrap_or(false));
            }
        });
    });
}

fn toggle_like(ui: &MainWindow) {
    let pb = ui.global::<Playback>();
    let uri = ui.global::<Browse>().get_current_uri().to_string();
    if !pb.get_like_known() || uri.is_empty() {
        return;
    }
    let liked = pb.get_liked();
    pb.set_liked(!liked);
    let w = ui.as_weak();
    runtime().spawn(async move {
        let r = api::set_saved(std::slice::from_ref(&uri), !liked).await;
        let _ = w.upgrade_in_event_loop(move |ui| match r {
            Ok(()) => {
                library::note_saved(&uri, !liked, None);
                menu::toast_text(&ui, if liked { "Removed from Liked Songs" } else { "Saved to Liked Songs" });
                mini::osd(&ui, if liked { "Removed from Liked Songs" } else { "Saved to Liked Songs" });
            }
            Err(e) => {
                ui.global::<Playback>().set_liked(liked);
                menu::toast_text(&ui, &format!("Couldn't update Liked Songs: {}", menu::why(&e)));
            }
        });
    });
}

/// "Artist - Title" of the playing track to the clipboard.
fn copy_now_playing(ui: &MainWindow) {
    let pb = ui.global::<Playback>();
    if pb.get_title().is_empty() {
        return;
    }
    let text = format!("{} - {}", pb.get_artist(), pb.get_title());
    let ok = clip::set_text(&text);
    menu::toast_text(ui, if ok { "Copied: Artist - Title" } else { "Couldn't copy" });
    if ok {
        mini::osd(ui, "Copied to clipboard");
    }
}

/// Tray, hotkeys and lock events.
fn on_winint(cmd: winint::Cmd) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = main_ui() else { return };
        match cmd {
            winint::Cmd::PlayPause => {
                play_pause_cmd();
                mini::osd(&ui, if ui.global::<Playback>().get_playing() { "Paused" } else { "Playing" });
            }
            winint::Cmd::Next => {
                with_player(|p| p.next());
                mini::osd(&ui, "Next");
            }
            winint::Cmd::Prev => {
                with_player(|p| p.prev());
                mini::osd(&ui, "Previous");
            }
            winint::Cmd::Like => toggle_like(&ui),
            winint::Cmd::CopyNowPlaying => copy_now_playing(&ui),
            winint::Cmd::Show => show_main(),
            winint::Cmd::Mini => mini::toggle(&ui),
            winint::Cmd::Quit => {
                let _ = slint::quit_event_loop();
            }
            winint::Cmd::Device(id) => {
                prefs::set("device", serde_json::json!(id));
                winint::set_device_label(&id);
                audio::set_device((!id.is_empty()).then_some(id));
            }
            winint::Cmd::Locked => {
                if prefs::bool_or("lock_pause", false) && ui.global::<Playback>().get_playing() {
                    with_player(|p| p.pause());
                    logln!("lock: paused");
                }
            }
        }
    });
}

fn on_device(ev: audio::DeviceEvent) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = main_ui() else { return };
        let playing = ui.global::<Playback>().get_playing();
        match ev {
            audio::DeviceEvent::BluetoothLost if playing && prefs::bool_or("bt_pause", true) => {
                with_player(|p| p.pause());
                PAUSED_BY_BT.store(true, Ordering::Relaxed);
                menu::toast_text(&ui, "Paused: Bluetooth audio disconnected");
            }
            audio::DeviceEvent::BluetoothBack if PAUSED_BY_BT.swap(false, Ordering::Relaxed) && prefs::bool_or("bt_resume", false) => {
                with_player(|p| p.play_pause());
            }
            _ => {}
        }
    });
}

fn with_player(f: impl FnOnce(&Player)) {
    if let Some(p) = PLAYER.get() {
        f(p);
    }
}

static RT: OnceLock<tokio::runtime::Handle> = OnceLock::new();
/// The signed-in Web API client (absent without a client ID or before sign-in).
static WEB: OnceLock<Arc<web::Web>> = OnceLock::new();

pub fn web() -> Option<Arc<web::Web>> {
    WEB.get().cloned()
}
/// Cadence itself is playing (it has a track loaded and no other device took over).
static LOCAL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn runtime() -> tokio::runtime::Handle {
    RT.get().cloned().expect("runtime set in main")
}

pub fn is_local_active() -> bool {
    LOCAL_ACTIVE.load(Ordering::Relaxed)
}

/// Play/pause from any source (bar, Space, SMTC, taskbar). The first press after launch resumes
/// the saved session when nothing is loaded yet.
fn play_pause_cmd() {
    PAUSED_BY_BT.store(false, Ordering::Relaxed);
    if !is_local_active() && queue::resume() {
        return;
    }
    with_player(|p| p.play_pause());
}

/// UI thread: the saved session's track in the bar, paused at its position.
pub fn show_restored(ui: &MainWindow, saved: &queue::Saved, m: &player::ItemMeta) {
    let seq = TRACK_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    apply_track(ui, runtime(), seq, saved.track.clone(), m.title.clone(), m.artist.clone(), m.album.clone(), m.art_url.clone(), m.duration_ms);
    let pb = ui.global::<Playback>();
    pb.set_position_ms(saved.position_ms as i32);
    pb.set_playing(false);
    pb.set_context_uri(if saved.context.starts_with("spotify:track:") { "".into() } else { saved.context.as_str().into() });
    lyrics_view::on_position(saved.position_ms, false);
}

thread_local! {
    /// UI thread only.
    static SMTC: RefCell<Option<smtc::Smtc>> = const { RefCell::new(None) };
}

/// Native HWND of the Slint window. Only `Some` once the winit window exists: call it from
/// inside the running event loop after `show()`, not before `run()`.
pub fn hwnd(window: &slint::Window) -> Option<isize> {
    match window.window_handle().window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(feature = "smoke")]
fn save_snapshot(window: &slint::Window, path: &str) -> Result<(), String> {
    let buf = window.take_snapshot().map_err(|e| e.to_string())?;
    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), buf.width(), buf.height());
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut w| w.write_image_data(buf.as_bytes()))
        .map_err(|e| e.to_string())?;
    eprintln!("smoke: wrote {path} ({}x{})", buf.width(), buf.height());
    Ok(())
}

#[cfg(not(feature = "smoke"))]
fn save_snapshot(_: &slint::Window, _: &str) -> Result<(), String> {
    Ok(())
}

/// Creates the SMTC session and the taskbar identity on the UI thread once the native window
/// exists (polls every 50 ms).
fn init_native(ui: slint::Weak<MainWindow>, tries: u32) {
    let delay = Duration::from_millis(if tries == 0 { 0 } else { 50 });
    slint::Timer::single_shot(delay, move || {
        let Some(win) = ui.upgrade() else { return };
        match hwnd(win.window()) {
            Some(h) => {
                taskbar::set_window_identity(h);
                thumbbar::install(h, |cmd| {
                    with_player(|p| match cmd {
                        thumbbar::Cmd::PlayPause => play_pause_cmd(),
                        thumbbar::Cmd::Next => p.next(),
                        thumbbar::Cmd::Prev => p.prev(),
                    })
                });
                winint::set_owner(h);
                // Media keys arrive on a WinRT thread; act on the UI thread.
                let on_key = |key: smtc::Key| {
                    let _ = slint::invoke_from_event_loop(move || {
                        let label = match key {
                            smtc::Key::PlayPause => {
                                play_pause_cmd();
                                "Play / pause"
                            }
                            smtc::Key::Next => {
                                with_player(|p| p.next());
                                "Next"
                            }
                            smtc::Key::Prev => {
                                with_player(|p| p.prev());
                                "Previous"
                            }
                        };
                        if let Some(ui) = main_ui() {
                            mini::osd(&ui, label);
                        }
                    });
                };
                match smtc::Smtc::new(h, on_key) {
                    Ok(s) => {
                        SMTC.with(|c| *c.borrow_mut() = Some(s));
                        eprintln!("smtc: ready");
                    }
                    Err(e) => eprintln!("smtc: {e}"),
                }
            }
            None if tries < 40 => init_native(ui.clone(), tries + 1),
            None => eprintln!("smtc: no window handle"),
        }
    });
}

fn smtc_update(title: String, artist: String, playing: bool, art_url: Option<String>) {
    let _ = slint::invoke_from_event_loop(move || {
        SMTC.with(|c| {
            if let Some(s) = c.borrow().as_ref() {
                s.update(&title, &artist, playing, art_url.as_deref());
            }
        })
    });
}

fn describe(ev: &Event) -> String {
    match ev {
        Event::Track { uri, title, artist, album, art_url, duration_ms, .. } => {
            format!("Track {uri} \"{title}\" by {artist} on {album} ({duration_ms} ms, art: {art_url:?})")
        }
        Event::Playing { position_ms } => format!("Playing at {position_ms} ms"),
        Event::Paused { position_ms } => format!("Paused at {position_ms} ms"),
        Event::Stopped => "Stopped".into(),
        Event::Volume(v) => format!("Volume {v:.2}"),
        Event::Shuffle(on) => format!("Shuffle {on}"),
        Event::Repeat(mode) => format!("Repeat {mode}"),
        Event::Error(e) => format!("Error {e}"),
    }
}

/// Lyrics for a track: the disk cache first (a miss is remembered for a day), else the pipeline.
async fn lyrics_for(uri: &str, title: &str, artist: &str, album: &str, duration_ms: u32) -> Option<player::Lyrics> {
    let name = format!("{}.json", cache::key(&format!("{uri}|{title}|{artist}")));
    let cached = {
        let n = name.clone();
        tokio::task::spawn_blocking(move || {
            // A hit is good forever; "none found" only for a day (providers add lyrics over time).
            cache::read("lyrics", &n, None).map(|b| {
                if b.starts_with(b"{\"none\"") {
                    let fresh = cache::read("lyrics", &n, Some(Duration::from_secs(86_400))).is_some();
                    (None, fresh)
                } else {
                    (player::Lyrics::from_json(&b), true)
                }
            })
        })
        .await
        .ok()
        .flatten()
    };
    match cached {
        Some((Some(l), _)) => return Some(l),
        Some((None, true)) => return None,
        _ => {}
    }
    let found = match PLAYER.get() {
        Some(p) => p.lyrics(uri, title, artist, album, duration_ms).await,
        None => player::lyrics_pipeline(None, title, artist, album, duration_ms).await,
    };
    let data = found.as_ref().map_or_else(|| "{\"none\":true}".to_string(), |l| l.to_json());
    tokio::task::spawn_blocking(move || cache::write("lyrics", &name, data.as_bytes()));
    found
}

/// Apple Music word lyrics for the karaoke stage, cached on disk like the line lyrics.
#[cfg(feature = "karaoke")]
async fn karaoke_for(uri: &str, title: &str, artist: &str, duration_ms: u32) -> Option<player::Lyrics> {
    let name = format!("{}.k.json", cache::key(&format!("{uri}|{title}|{artist}")));
    let n = name.clone();
    if let Some(b) = tokio::task::spawn_blocking(move || cache::read("lyrics", &n, None)).await.ok().flatten() {
        if b.starts_with(b"{\"none\"") {
            let n = name.clone();
            let fresh = tokio::task::spawn_blocking(move || cache::read("lyrics", &n, Some(Duration::from_secs(86_400))))
                .await
                .ok()
                .flatten()
                .is_some();
            if fresh {
                return None;
            }
        } else if let Some(l) = player::Lyrics::from_json(&b) {
            return Some(l);
        }
    }
    let isrc = match PLAYER.get() {
        Some(p) if uri.starts_with("spotify:track:") => p.isrc(uri).await,
        _ => None,
    };
    let found = apple::word_lyrics(isrc, title, artist, duration_ms).await;
    let data = found.as_ref().map_or_else(|| "{\"none\":true}".to_string(), |l| l.to_json());
    tokio::task::spawn_blocking(move || cache::write("lyrics", &name, data.as_bytes()));
    found
}

/// UI thread: show the new track and kick off its art and lyrics.
#[allow(clippy::too_many_arguments)]
fn apply_track(
    ui: &MainWindow,
    rt: tokio::runtime::Handle,
    seq: u64,
    uri: String,
    title: String,
    artist: String,
    album: String,
    art_url: Option<String>,
    duration_ms: u32,
) {
    let pb = ui.global::<Playback>();
    pb.set_title(title.clone().into());
    pb.set_artist(artist.clone().into());
    pb.set_album(album.clone().into());
    pb.set_duration_ms(duration_ms as i32);
    pb.set_position_ms(0);
    lyrics_view::on_track(duration_ms);
    lyrics_view::set_offset(db::lyrics_offset(&uri));
    ui.global::<Browse>().set_current_uri(uri.clone().into());
    pb.set_like_known(false);
    refresh_like_state(ui);
    winint::set_now_playing(&format!("{title} · {artist}"), pb.get_playing());
    mini::sync(ui);

    // Local files carry their artwork inside the file.
    let art_url = if art_url.is_none() && uri.starts_with("spotify:local:") {
        let (weak, u) = (ui.as_weak(), uri.clone());
        let scale = ui.window().scale_factor();
        rt.spawn_blocking(move || {
            let px = (540.0 * scale) as u32;
            let img = local::artwork(&u).and_then(|d| art::decode_square(&d, px));
            let small = img.as_ref().map(|i| art::decode_rescale(i, (56.0 * scale) as u32));
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if TRACK_GEN.load(Ordering::Relaxed) != seq {
                    return;
                }
                let pb = ui.global::<Playback>();
                if let Some(big) = img {
                    pb.set_side_art(slint::Image::from_rgba8(art::rounded(&big, 14.0 / 540.0)));
                    pb.set_bg_art(slint::Image::from_rgb8(art::blurred_bg(&big)));
                }
                if let Some(s) = small {
                    pb.set_thumb_art(slint::Image::from_rgba8(art::rounded(&s, 10.0 / 44.0)));
                    pb.set_bar_art(slint::Image::from_rgb8(s));
                }
                mini::sync(&ui);
            });
        });
        None
    } else {
        art_url
    };

    {
        let weak = ui.as_weak();
        let (uri, title, artist, album) = (uri.clone(), title.clone(), artist.clone(), album.clone());
        rt.spawn(async move {
            let lyrics = lyrics_for(&uri, &title, &artist, &album, duration_ms).await;
            let _ = weak.upgrade_in_event_loop(move |_| {
                if TRACK_GEN.load(Ordering::Relaxed) == seq {
                    lyrics_view::set_lyrics(lyrics);
                }
            });
            #[cfg(feature = "karaoke")]
            if let Some(k) = karaoke_for(&uri, &title, &artist, duration_ms).await {
                let _ = weak.upgrade_in_event_loop(move |_| {
                    if TRACK_GEN.load(Ordering::Relaxed) == seq {
                        lyrics_view::set_karaoke_words(k);
                    }
                });
            }
        });
    }

    let Some(url) = art_url else {
        for set in [Playback::set_art, Playback::set_bar_art, Playback::set_side_art, Playback::set_thumb_art, Playback::set_bg_art] {
            set(&pb, slint::Image::default());
        }
        return;
    };
    let scale = ui.window().scale_factor();
    // The lyrics page shows the art at up to 540 px; the bar at 56.
    let (big_px, bar_px) = ((540.0 * scale).round() as u32, (56.0 * scale).round() as u32);
    let weak = ui.as_weak();
    rt.spawn(async move {
        let (big, small) = tokio::join!(art::load(&url, big_px), art::load(&url, bar_px));
        // Derived images are made off the UI thread.
        let derived = tokio::task::spawn_blocking(move || {
            let side = big.as_ref().map(|a| art::rounded(&a.buf, 14.0 / 540.0));
            let bg = big.as_ref().or(small.as_ref()).map(|a| art::blurred_bg(&a.buf));
            let thumb = small.as_ref().map(|a| art::rounded(&a.buf, 10.0 / 44.0));
            let accent = big.as_ref().or(small.as_ref()).map(|a| a.accent);
            (side, bg, thumb, small.map(|a| a.buf), accent)
        })
        .await;
        let Ok((side, bg, thumb, small, accent)) = derived else { return };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if TRACK_GEN.load(Ordering::Relaxed) != seq {
                return;
            }
            let pb = ui.global::<Playback>();
            if let Some((r, g, b)) = accent {
                let c = slint::Color::from_rgb_u8(r, g, b);
                pb.set_accent(c);
                ui.global::<Browse>().set_accent(c);
            }
            let rgb = |b: Option<slint::SharedPixelBuffer<slint::Rgb8Pixel>>| b.map_or_else(slint::Image::default, slint::Image::from_rgb8);
            let rgba = |b: Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>| b.map_or_else(slint::Image::default, slint::Image::from_rgba8);
            pb.set_art(slint::Image::default());
            pb.set_bar_art(rgb(small));
            pb.set_bg_art(rgb(bg));
            pb.set_side_art(rgba(side));
            pb.set_thumb_art(rgba(thumb));
            mini::sync(&ui);
        });
    });
}

/// After a track change on the UI thread: notifications, panel, podcasts, sleep timer, stats.
fn after_track(ui: &MainWindow, uri: &str, title: &str, artist: &str, album: &str, duration_ms: u32, explicit: bool, art_url: Option<String>) {
    if explicit && prefs::bool_or("explicit_filter", false) {
        logln!("explicit filter: skipping {uri}");
        menu::toast_text(ui, "Skipped an explicit track (Settings > Filter explicit content)");
        with_player(|p| p.next());
        return;
    }
    podcast::on_track(ui, uri, duration_ms);
    sleep::on_track(ui, uri);
    stats::on_track(uri, title, artist, album, duration_ms);
    if prefs::bool_or("panel_on_play", false) && !ui.global::<Playback>().get_queue_open() {
        ui.global::<Playback>().set_side_open(true);
    }
    // Track-change notification while Cadence is in the background.
    if prefs::bool_or("notify", false) && !mini::main_has_focus(ui) {
        let (t, a) = (title.to_string(), artist.to_string());
        runtime().spawn(async move {
            let path = match art_url {
                Some(url) => match art::load(&url, 128).await {
                    Some(a) => {
                        let p = cache::dir("..").join("toast.png");
                        let ok = tokio::task::spawn_blocking({
                            let p = p.clone();
                            move || art::save_rgb_png(&a.buf, &p)
                        })
                        .await
                        .unwrap_or(false);
                        ok.then_some(p)
                    }
                    None => None,
                },
                None => None,
            };
            winint::toast(&t, &a, path.as_deref());
        });
    }
}

/// Text under the page title until browse takes over (`browse::init` overwrites it).
fn set_status(ui: &slint::Weak<MainWindow>, text: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Browse>().set_status(text.into()));
}

/// Web API sign-in (then browse starts), streaming sign-in and playback events.
/// Runs on the tokio runtime.
async fn startup(ui: slint::Weak<MainWindow>) {
    let rt = tokio::runtime::Handle::current();
    let mut browse_started = false;
    let _ = &rt;
    match auth::client_id() {
        Some(id) => {
            set_status(&ui, "Signing in to Spotify…".into());
            match web::Web::connect(&id).await {
                Ok(spotify) => {
                    logln!("auth: Web API ready");
                    let _ = WEB.set(spotify.clone());
                    let w = ui.clone();
                    spotify.set_sign_in_notifier(move |on| {
                        let text = if on { "Sign in to Spotify in your browser to continue…" } else { "" };
                        set_status(&w, text.into());
                    });
                    if let Some(days) = spotify.days_left().await
                        && days <= 14
                    {
                        logln!("auth: refresh token ends in {days} days");
                        AUTH_WARNING.store(days.max(0) as u64 + 1, Ordering::Relaxed);
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            menu::toast_text(&ui, &format!("Your Spotify sign-in ends in {} days. Settings > Re-authorize renews it.", days.max(0)));
                        });
                    }
                    let _ = ui.upgrade_in_event_loop(|ui| browse::init(&ui));
                    browse_started = true;
                }
                Err(e) => {
                    logln!("auth: Web API failed: {e}");
                    let _ = ui.upgrade_in_event_loop(|ui| browse::init(&ui));
                    browse_started = true;
                    set_status(&ui, "Couldn't sign in to Spotify. Settings > Re-authorize, or restart Cadence.".into());
                }
            }
        }
        None => {
            eprintln!(
                "auth: SPOTIFY_CLIENT_ID is missing (env var or %APPDATA%\\cadence\\client_id.txt); no Web API, streaming only"
            );
            let _ = ui.upgrade_in_event_loop(|ui| browse::init(&ui));
            browse_started = true;
            set_status(&ui, "Add your Spotify client ID to load your library. See README.".into());
        }
    }

    if let Err(e) = stream(&ui, rt).await {
        eprintln!("startup: {e}");
        if !browse_started {
            let short: String = e.chars().take(80).collect();
            set_status(&ui, format!("Couldn't start playback: {short}"));
        }
    }
}

/// Streaming sign-in, player start and the playback event loop.
async fn stream(ui: &slint::Weak<MainWindow>, rt: tokio::runtime::Handle) -> Result<(), String> {
    let cache = auth::librespot_cache();
    let cached = cache.credentials().is_some();
    let creds = auth::streaming_credentials(&cache).await?;
    let (player, mut events) = match Player::start(creds, cache).await {
        Ok(started) => started,
        Err(e) if cached => {
            // Stale cached credentials: drop them and sign in again once.
            eprintln!("player: start failed with cached credentials ({e}), signing in again");
            let _ = std::fs::remove_file(auth::data_dir().join("librespot").join("credentials.json"));
            let cache = auth::librespot_cache();
            let creds = auth::streaming_credentials(&cache).await?;
            Player::start(creds, cache).await?
        }
        Err(e) => return Err(e),
    };
    eprintln!("player: started");
    let _ = PLAYER.set(Arc::new(player));
    local::init();
    if std::env::var("CADENCE_TEST_PLAY").is_err() {
        queue::load_saved(ui.clone(), &rt);
    }
    test_play(ui);

    let (mut title, mut artist, mut playing, mut art_url) =
        (String::new(), String::new(), false, None);
    let mut current_uri = String::new();
    while let Some(ev) = events.recv().await {
        eprintln!("player: {}", describe(&ev));
        match ev {
            Event::Track { uri, title: t, artist: a, album, art_url: u, duration_ms, explicit } => {
                current_uri = uri.clone();
                // Local files show their tag title / artist (librespot only knows the file name).
                let (t, a, album) = match (uri.starts_with("spotify:local:"), db::local_by_uri(&uri)) {
                    (true, Some(l)) => (l.title, l.artist, l.album),
                    _ => (t, a, album),
                };
                (title, artist, art_url) = (t.clone(), a.clone(), u.clone());
                LOCAL_ACTIVE.store(true, Ordering::Relaxed);
                queue::forget_restore();
                queue::set_now(&uri, 0);
                queue::refresh(ui.clone(), &rt);
                smtc_update(title.clone(), artist.clone(), playing, art_url.clone());
                let seq = TRACK_GEN.fetch_add(1, Ordering::Relaxed) + 1;
                let rt = rt.clone();
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    apply_track(&ui, rt, seq, uri.clone(), t.clone(), a.clone(), album.clone(), u.clone(), duration_ms);
                    after_track(&ui, &uri, &t, &a, &album, duration_ms, explicit, u);
                });
            }
            Event::Playing { position_ms } | Event::Paused { position_ms } => {
                let now_playing = matches!(ev, Event::Playing { .. });
                queue::set_now(&current_uri, position_ms);
                if !now_playing {
                    queue::save();
                }
                stats::on_playing(now_playing);
                podcast::on_position(position_ms);
                if now_playing != playing {
                    playing = now_playing;
                    smtc_update(title.clone(), artist.clone(), playing, art_url.clone());
                }
                let t2 = format!("{title} · {artist}");
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.global::<Playback>().set_playing(now_playing);
                    thumbbar::set_playing(now_playing);
                    winint::set_now_playing(&t2, now_playing);
                    lyrics_view::on_position(position_ms, now_playing);
                    mini::sync(&ui);
                });
            }
            Event::Stopped => {
                playing = false;
                LOCAL_ACTIVE.store(false, Ordering::Relaxed);
                queue::save();
                stats::on_playing(false);
                let _ = ui.upgrade_in_event_loop(|ui| {
                    ui.global::<Playback>().set_playing(false);
                    thumbbar::set_playing(false);
                    lyrics_view::on_stopped();
                });
            }
            Event::Volume(v) => {
                let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Playback>().set_volume(v));
            }
            Event::Shuffle(on) => {
                let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Playback>().set_shuffle(on));
            }
            Event::Repeat(mode) => {
                let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Playback>().set_repeat(mode as i32));
            }
            Event::Error(_) => {}
        }
    }
    Ok(())
}

/// CADENCE_TEST_PLAY=<track uri> (test runs with CADENCE_INACTIVE): play it silently for
/// CADENCE_TEST_SECS seconds with the lyrics page open, then pause and restore the volume.
fn test_play(ui: &slint::Weak<MainWindow>) {
    // CADENCE_TEST_RESUME=1: press Play once (resumes the saved session) silently, log the queue.
    if std::env::var("CADENCE_TEST_RESUME").is_ok_and(|v| v == "1") {
        tokio::spawn(async move {
            let Some(p) = PLAYER.get() else { return };
            tokio::time::sleep(Duration::from_secs(4)).await;
            p.set_volume(0.0);
            let _ = slint::invoke_from_event_loop(play_pause_cmd);
            tokio::time::sleep(Duration::from_secs(6)).await;
            if let Some(s) = p.queue_snapshot().await {
                let cur = s.current.map(|c| c.uri).unwrap_or_default();
                let queued: Vec<_> = s.next.iter().filter(|i| i.queued).map(|i| i.uri.clone()).collect();
                eprintln!("test: resumed current={cur} queued={queued:?}");
            }
            p.play_pause();
        });
        return;
    }
    let Ok(uri) = std::env::var("CADENCE_TEST_PLAY") else { return };
    let secs: u64 = std::env::var("CADENCE_TEST_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    let ui = ui.clone();
    tokio::spawn(async move {
        let Some(p) = PLAYER.get() else { return };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            let pb = ui.global::<Playback>();
            let _ = tx.send(pb.get_volume());
            pb.set_expanded(true);
        });
        let before = rx.await.unwrap_or(0.5);
        p.set_volume(0.0);
        if uri.starts_with("spotify:show:") {
            // Podcasts: play the show's first listed episode (librespot metadata).
            use librespot_metadata::{Metadata, Show};
            let Ok(id) = librespot_core::SpotifyUri::from_uri(&uri) else { return };
            match Show::get(p.session(), &id).await {
                Ok(show) => {
                    let ep = show.episodes.0.first().and_then(|e| e.to_uri().ok()).unwrap_or_default();
                    eprintln!("test: show {} has {} episodes, playing {ep}", show.name, show.episodes.0.len());
                    p.play(&ep, 0);
                }
                Err(e) => eprintln!("test: show metadata failed: {e}"),
            }
        } else if uri == "local" {
            // Local files: wait for the folder scan, then play them all.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let uris: Vec<String> = db::local_all().into_iter().filter(|t| !t.missing && t.error.is_empty()).map(|t| t.uri).collect();
            eprintln!("test: local files {uris:?}");
            p.play_list(&uris, 0, None);
        } else {
            p.play(&uri, 0);
        }
        // CADENCE_TEST_QUEUE="uri,uri,...": after 4 s queue them (the first as Play next, the
        // rest at the end), open the queue panel, and with CADENCE_TEST_NEXT=1 skip once at 8 s.
        let queued = std::env::var("CADENCE_TEST_QUEUE").unwrap_or_default();
        let mut waited = 0;
        if !queued.is_empty() {
            tokio::time::sleep(Duration::from_secs(4)).await;
            waited = 4;
            let list: Vec<String> = queued.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                for (i, u) in list.iter().rev().enumerate() {
                    // Reverse order with Play next on the last one checks Play next ordering.
                    let msg = queue::add(&ui, u, i == list.len() - 1);
                    eprintln!("test: queue {u}: {msg}");
                }
                ui.global::<Playback>().set_expanded(false);
                ui.global::<Playback>().invoke_toggle_queue();
            });
            if std::env::var("CADENCE_TEST_NEXT").is_ok_and(|v| v == "1") {
                tokio::time::sleep(Duration::from_secs(4)).await;
                waited = 8;
                eprintln!("test: next");
                p.next();
            }
        }
        tokio::time::sleep(Duration::from_secs(secs.saturating_sub(waited))).await;
        p.play_pause();
        tokio::time::sleep(Duration::from_millis(500)).await;
        p.set_volume(before);
        eprintln!("test: done, volume restored to {before:.2}");
    });
}

/// CADENCE_DEMO: no Spotify at all. Shows one track (CADENCE_DEMO_TITLE / _ARTIST / _ALBUM /
/// _URI / _ART / _DURATION, ms) with its real lyrics, the clock running from CADENCE_DEMO_POS,
/// and writes snapshots at the wall-clock offsets in CADENCE_SHOTS ("ms:file.png,..."), then quits.
/// CADENCE_EXPANDED=1 opens the lyrics page, CADENCE_KARAOKE=1 karaoke, CADENCE_SIZE=WxH sizes it.
fn demo(ui: &MainWindow, rt: &tokio::runtime::Runtime) {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let num = |k: &str| env(k).parse::<u32>().unwrap_or(0);
    let (title, artist, album, uri, art) = (env("CADENCE_DEMO_TITLE"), env("CADENCE_DEMO_ARTIST"), env("CADENCE_DEMO_ALBUM"), env("CADENCE_DEMO_URI"), env("CADENCE_DEMO_ART"));
    let duration = num("CADENCE_DEMO_DURATION");
    let seq = TRACK_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    apply_track(ui, rt.handle().clone(), seq, uri, title, artist, album, (!art.is_empty()).then_some(art), duration);
    let pb = ui.global::<Playback>();
    pb.set_playing(true);
    pb.set_expanded(env("CADENCE_EXPANDED") == "1");
    lyrics_view::demo_clock(num("CADENCE_DEMO_POS"), duration, env("CADENCE_DEMO_PAUSED") != "1");
    if env("CADENCE_KARAOKE") == "1" {
        let weak = ui.as_weak();
        // Wait for the word lyrics, then switch.
        let poll = slint::Timer::default();
        poll.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
            if let Some(ui) = weak.upgrade() {
                let pb = ui.global::<Playback>();
                if pb.get_karaoke_available() && !pb.get_karaoke() {
                    pb.invoke_set_karaoke(true);
                }
            }
        });
        std::mem::forget(poll);
    }
    // CADENCE_RESIZE="ms:WxH,...": resize the window mid-run (fullscreen / windowed transitions).
    for spec in env("CADENCE_RESIZE").split(',').filter(|s| !s.is_empty()) {
        let Some((ms, size)) = spec.split_once(':') else { continue };
        let Some((w, h)) = size.split_once('x') else { continue };
        let (w, h): (f32, f32) = (w.parse().unwrap_or(1200.0), h.parse().unwrap_or(780.0));
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(ms.parse().unwrap_or(0)), move || {
            if let Some(ui) = weak.upgrade() {
                // 0x0 toggles fullscreen, 1x1 maximizes, anything else is a size.
                match (w as u32, h as u32) {
                    (0, 0) => ui.window().set_fullscreen(!ui.window().is_fullscreen()),
                    (1, 1) => ui.window().set_maximized(true),
                    _ => ui.window().set_size(slint::LogicalSize::new(w, h)),
                }
            }
        });
    }
    schedule_shots(ui);
}

/// CADENCE_SHOTS="ms:file.png,...": snapshots at those offsets, then quit (test builds).
fn schedule_shots(ui: &MainWindow) {
    let shots = std::env::var("CADENCE_SHOTS").unwrap_or_default();
    if shots.is_empty() {
        return;
    }
    let mut last = 0u64;
    for spec in shots.split(',').filter(|s| !s.is_empty()) {
        let Some((ms, file)) = spec.split_once(':') else { continue };
        let ms: u64 = ms.parse().unwrap_or(0);
        last = last.max(ms);
        let (weak, file) = (ui.as_weak(), file.to_string());
        slint::Timer::single_shot(Duration::from_millis(ms), move || {
            if let Some(ui) = weak.upgrade() {
                let pb = ui.global::<Playback>();
                eprintln!(
                    "shot {file}: pos={} lyric={} active={} lines={} karaoke={}",
                    pb.get_position_ms(),
                    pb.get_lyric_ms(),
                    pb.get_lyr_active(),
                    pb.get_lyr_lines().row_count(),
                    pb.get_karaoke()
                );
                if let Err(e) = save_snapshot(ui.window(), &file) {
                    eprintln!("snapshot failed: {e}");
                    SMOKE_FAILED.store(true, Ordering::Relaxed);
                }
            }
        });
    }
    slint::Timer::single_shot(Duration::from_millis(last + 300), || {
        let _ = slint::quit_event_loop();
    });
}

use slint::Model;

fn main() -> Result<(), slint::PlatformError> {
    taskbar::register_app_id();
    mem::start_logger();
    mem::start_guard();

    // librespot-playback 0.8.0 builds its own `Runtime::new()` (one worker per core); this caps it at one thread.
    // Safe here: nothing else is running yet.
    unsafe { std::env::set_var("TOKIO_WORKER_THREADS", "1") };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _rt_guard = rt.enter();
    let _ = RT.set(rt.handle().clone());
    rt.spawn_blocking(cache::prune_all);

    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    // CADENCE_SMOKE: inactive window, snapshot, auto-quit, no network.
    // CADENCE_OFFLINE: inactive window, no network, runs until closed (for measuring idle memory).
    // CADENCE_DEMO: inactive window, lyrics only (see demo()).
    let (smoke, offline, demo_mode) = (flag("CADENCE_SMOKE"), flag("CADENCE_OFFLINE"), flag("CADENCE_DEMO"));
    let inactive = smoke || offline || demo_mode || flag("CADENCE_INACTIVE");

    settings::apply_startup();
    let mut backend = slint::BackendSelector::new();
    // Tests never steal focus; the media-key overlay never takes focus or a taskbar button.
    backend = backend.with_winit_window_attributes_hook(move |attrs| {
        use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
        if mini::OVERLAY_NEXT.swap(false, Ordering::Relaxed) {
            attrs.with_active(false).with_skip_taskbar(true)
        } else if inactive {
            attrs.with_active(false)
        } else {
            attrs
        }
    });
    // Settings > Hardware acceleration off: the software renderer.
    if !prefs::bool_or("gpu", true) {
        backend = backend.renderer_name("software".into());
    }
    backend.select()?;

    let ui = MainWindow::new()?;
    if let Some((w, h)) = std::env::var("CADENCE_SIZE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        ui.window().set_size(slint::LogicalSize::new(w, h));
    }
    lyrics_view::init(&ui);
    // GPU renderer (femtovg, vsync on): run the karaoke stage once per displayed frame. The
    // software fallback renderer has no notifier; lyrics_view then keeps its timer.
    if ui
        .window()
        .set_rendering_notifier(|state, _| {
            if let slint::RenderingState::BeforeRendering = state {
                lyrics_view::frame();
            }
        })
        .is_ok()
    {
        eprintln!("renderer: GPU, vsync");
        lyrics_view::enable_vsync();
    } else {
        eprintln!("renderer: software");
    }
    MAIN.with(|m| *m.borrow_mut() = Some(ui.as_weak()));
    ui.on_play_pause_key(play_pause_cmd);
    let weak = ui.as_weak();
    ui.on_shortcut(move |s| {
        let Some(ui) = weak.upgrade() else { return };
        match s.as_str() {
            "zoom-in" | "zoom-out" | "zoom-reset" => settings::zoom(&ui, &s),
            "back" => browse::back(),
            "forward" => browse::forward(),
            "copy-now-playing" => copy_now_playing(&ui),
            "paste" => {
                // A Spotify link pasted anywhere outside a text field opens / plays it.
                if let Some(text) = clip::get_text()
                    && !browse::open_link(&text)
                {
                    menu::toast_text(&ui, "Paste a Spotify link or URI to open it");
                }
            }
            _ => {}
        }
    });
    ui.on_lyrics_key(|k| lyrics_view::key(&k));
    ui.on_relayout_lyrics(lyrics_view::relayout);

    let pb = ui.global::<Playback>();
    pb.on_play_pause(play_pause_cmd);
    pb.on_next(|| with_player(|p| p.next()));
    pb.on_prev(|| with_player(|p| p.prev()));
    pb.on_seek(|ms| {
        let ms = ms.max(0) as u32;
        lyrics_view::on_seek(ms);
        with_player(|p| p.seek(ms));
    });
    pb.on_set_volume(|v| with_player(|p| p.set_volume(v)));
    let weak = ui.as_weak();
    pb.on_toggle_shuffle(move || {
        if let Some(ui) = weak.upgrade() {
            with_player(|p| p.set_shuffle(!ui.global::<Playback>().get_shuffle()));
        }
    });
    let weak = ui.as_weak();
    pb.on_toggle_repeat(move || {
        if let Some(ui) = weak.upgrade() {
            // Off -> all -> one -> off.
            with_player(|p| p.set_repeat(((ui.global::<Playback>().get_repeat() + 1) % 3) as u8));
        }
    });
    let weak = ui.as_weak();
    pb.on_open_context(move || {
        if let Some(ui) = weak.upgrade() {
            let uri = ui.global::<Playback>().get_context_uri();
            if !uri.is_empty() {
                ui.global::<Playback>().set_expanded(false);
                ui.global::<Browse>().invoke_open(uri);
            }
        }
    });

    let weak = ui.as_weak();
    pb.on_toggle_queue(move || {
        if let Some(ui) = weak.upgrade() {
            let pb = ui.global::<Playback>();
            pb.set_queue_open(!pb.get_queue_open());
            queue::set_panel_open(pb.get_queue_open());
            queue::refresh(ui.as_weak(), &runtime());
        }
    });
    queue::init(&ui);
    menu::init(&ui);
    // CADENCE_TEST_MENU=<uri>: open its context menu after 3 s (screenshot tests).
    if let Ok(uri) = std::env::var("CADENCE_TEST_MENU") {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_secs(3), move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<Menu>().invoke_for_item(uri.into(), "".into(), 0, "".into(), 420.0, 160.0);
            }
        });
    }
    let weak = ui.as_weak();
    pb.on_lyrics_offset(move |delta| {
        if let Some(ui) = weak.upgrade() {
            let uri = ui.global::<Browse>().get_current_uri().to_string();
            let ms = (lyrics_view::offset() + delta as i64).clamp(-10_000, 10_000);
            db::set_lyrics_offset(&uri, ms);
            lyrics_view::set_offset(ms);
        }
    });
    pb.on_seek_line(|i| {
        if let Some(ms) = lyrics_view::line_start(i.max(0) as usize) {
            lyrics_view::on_seek(ms);
            with_player(|p| p.seek(ms));
        }
    });
    let weak = ui.as_weak();
    pb.on_open_artist(move || {
        if let Some(ui) = weak.upgrade() {
            let uri = ui.global::<Browse>().get_current_uri().to_string();
            runtime().spawn(async move {
                if let Some(m) = queue::player_meta(&uri).await
                    && !m.artist_uri.is_empty()
                {
                    let _ = slint::invoke_from_event_loop(move || browse::go(&m.artist_uri));
                }
            });
        }
    });
    let weak = ui.as_weak();
    pb.on_toggle_like(move || {
        if let Some(ui) = weak.upgrade() {
            toggle_like(&ui);
        }
    });
    let weak = ui.as_weak();
    pb.on_mini_player(move || {
        if let Some(ui) = weak.upgrade() {
            mini::toggle(&ui);
        }
    });
    dialogs::init(&ui);
    settings::init(&ui);
    // Podcasts: save the episode position every 15 s while playing.
    let progress = slint::Timer::default();
    {
        let weak = ui.as_weak();
        progress.start(slint::TimerMode::Repeated, Duration::from_secs(15), move || {
            if let Some(ui) = weak.upgrade() {
                let pb = ui.global::<Playback>();
                if pb.get_is_episode() && pb.get_playing() {
                    podcast::on_position(pb.get_position_ms().max(0) as u32);
                }
            }
        });
    }
    let weak = ui.as_weak();
    pb.on_track_menu(move |x, y| {
        if let Some(ui) = weak.upgrade() {
            let uri = ui.global::<Browse>().get_current_uri();
            let ctx = ui.global::<Playback>().get_context_uri();
            if !uri.is_empty() {
                ui.global::<Menu>().invoke_for_item(uri, ctx, -1, "".into(), x, y);
            }
        }
    });

    init_native(ui.as_weak(), 0);
    if !inactive {
        winint::start(prefs::bool_or("hotkeys", true), on_winint);
        audio::on_device_event(on_device);
    }
    // Mouse back / forward buttons navigate.
    {
        use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::{ElementState, MouseButton, WindowEvent}};
        ui.window().on_winit_window_event(|_, ev| {
            if let WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } = ev {
                match button {
                    MouseButton::Back => browse::back(),
                    MouseButton::Forward => browse::forward(),
                    _ => {}
                }
            }
            EventResult::Propagate
        });
    }
    // Close button: hide to the tray (Settings) or quit.
    ui.window().on_close_requested(|| {
        if prefs::bool_or("close_to_tray", false) {
            slint::CloseRequestResponse::HideWindow
        } else {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        }
    });
    if prefs::u64_or("zoom", 100) != 100 {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(50), move || {
            if let Some(ui) = weak.upgrade() {
                settings::apply_zoom(&ui);
            }
        });
    }

    if smoke {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(1500), move || {
            if let Some(ui) = weak.upgrade() {
                eprintln!("smoke: hwnd = {:?}", hwnd(ui.window()));
                let path = std::env::var("CADENCE_SNAPSHOT").unwrap_or_else(|_| "smoke.png".into());
                if let Err(e) = save_snapshot(ui.window(), &path) {
                    eprintln!("smoke: snapshot failed: {e}");
                    SMOKE_FAILED.store(true, Ordering::Relaxed);
                }
            }
            let _ = slint::quit_event_loop();
        });
    } else if demo_mode {
        demo(&ui, &rt);
    } else if offline {
        browse::init(&ui);
        // Tests: CADENCE_ROUTES="ms:route,..." navigates; CADENCE_SHOTS snapshots.
        for spec in std::env::var("CADENCE_ROUTES").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
            let Some((ms, route)) = spec.split_once(':') else { continue };
            let route = route.to_string();
            slint::Timer::single_shot(Duration::from_millis(ms.parse().unwrap_or(0)), move || browse::go(&route));
        }
        schedule_shots(&ui);
    } else {
        rt.spawn(startup(ui.as_weak()));
        // Test runs only: CADENCE_SHOTS with the real library (inactive window, nothing played).
        if flag("CADENCE_INACTIVE") {
            for spec in std::env::var("CADENCE_ROUTES").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
                let Some((ms, route)) = spec.split_once(':') else { continue };
                let route = route.to_string();
                slint::Timer::single_shot(Duration::from_millis(ms.parse().unwrap_or(0)), move || browse::go(&route));
            }
            schedule_shots(&ui);
        }
    }

    // --minimized (start with Windows): stay in the tray when close-to-tray is on, else minimized.
    let minimized = std::env::args().any(|a| a == "--minimized");
    if !(minimized && prefs::bool_or("close_to_tray", false)) {
        ui.show()?;
        if minimized {
            ui.window().set_minimized(true);
        }
    }
    slint::run_event_loop_until_quit()?;
    let _ = ui.hide();
    winint::remove_tray();
    stats::flush();
    drop(progress);

    // Remember where playback stopped, then disconnect the Connect device (waits at most 2 s).
    if is_local_active() {
        queue::set_now_position(ui.global::<Playback>().get_position_ms().max(0) as u32);
        queue::save();
    }
    if let Some(p) = PLAYER.get() {
        rt.block_on(p.shutdown());
    }

    if SMOKE_FAILED.load(Ordering::Relaxed) {
        std::process::exit(1);
    }
    Ok(())
}

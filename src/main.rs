#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "karaoke")]
mod apple;
mod art;
mod auth;
mod browse;
mod cache;
mod lyrics_view;
mod player;
mod smtc;
mod taskbar;

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

fn with_player(f: impl FnOnce(&Player)) {
    if let Some(p) = PLAYER.get() {
        f(p);
    }
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
                let on_key = |key: smtc::Key| {
                    if let Some(p) = PLAYER.get() {
                        match key {
                            smtc::Key::PlayPause => p.play_pause(),
                            smtc::Key::Next => p.next(),
                            smtc::Key::Prev => p.prev(),
                        }
                    }
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
        Event::Track { uri, title, artist, album, art_url, duration_ms } => {
            format!("Track {uri} \"{title}\" by {artist} on {album} ({duration_ms} ms, art: {art_url:?})")
        }
        Event::Playing { position_ms } => format!("Playing at {position_ms} ms"),
        Event::Paused { position_ms } => format!("Paused at {position_ms} ms"),
        Event::Stopped => "Stopped".into(),
        Event::Volume(v) => format!("Volume {v:.2}"),
        Event::Shuffle(on) => format!("Shuffle {on}"),
        Event::Repeat(on) => format!("Repeat {on}"),
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
    ui.global::<Browse>().set_current_uri(uri.clone().into());

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
        });
    });
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
    match auth::client_id() {
        Some(id) => {
            set_status(&ui, "Signing in to Spotify…".into());
            match auth::web_api(&id).await {
                Ok(spotify) => {
                    eprintln!("auth: Web API ready");
                    let play: Arc<dyn Fn(String, usize) + Send + Sync> =
                        Arc::new(|uri, index| match PLAYER.get() {
                            Some(p) => p.play(&uri, index),
                            None => eprintln!("player: not ready yet"),
                        });
                    let rt = rt.clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| browse::init(&ui, rt, spotify, play));
                    browse_started = true;
                }
                Err(e) => {
                    eprintln!("auth: Web API failed: {e}");
                    set_status(&ui, "Couldn't sign in to Spotify. Restart Cadence to try again.".into());
                }
            }
        }
        None => {
            eprintln!(
                "auth: SPOTIFY_CLIENT_ID is missing (env var or %APPDATA%\\cadence\\client_id.txt); no Web API, streaming only"
            );
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
    test_play(ui);

    let (mut title, mut artist, mut playing, mut art_url) =
        (String::new(), String::new(), false, None);
    while let Some(ev) = events.recv().await {
        eprintln!("player: {}", describe(&ev));
        match ev {
            Event::Track { uri, title: t, artist: a, album, art_url: u, duration_ms } => {
                (title, artist, art_url) = (t.clone(), a.clone(), u.clone());
                smtc_update(title.clone(), artist.clone(), playing, art_url.clone());
                let seq = TRACK_GEN.fetch_add(1, Ordering::Relaxed) + 1;
                let rt = rt.clone();
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    apply_track(&ui, rt, seq, uri, t, a, album, u, duration_ms)
                });
            }
            Event::Playing { position_ms } | Event::Paused { position_ms } => {
                let now_playing = matches!(ev, Event::Playing { .. });
                if now_playing != playing {
                    playing = now_playing;
                    smtc_update(title.clone(), artist.clone(), playing, art_url.clone());
                }
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.global::<Playback>().set_playing(now_playing);
                    lyrics_view::on_position(position_ms, now_playing);
                });
            }
            Event::Stopped => {
                playing = false;
                let _ = ui.upgrade_in_event_loop(|ui| {
                    ui.global::<Playback>().set_playing(false);
                    lyrics_view::on_stopped();
                });
            }
            Event::Volume(v) => {
                let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Playback>().set_volume(v));
            }
            Event::Shuffle(on) => {
                let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Playback>().set_shuffle(on));
            }
            Event::Repeat(on) => {
                let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Playback>().set_repeat(on));
            }
            Event::Error(_) => {}
        }
    }
    Ok(())
}

/// CADENCE_TEST_PLAY=<track uri> (test runs with CADENCE_INACTIVE): play it silently for
/// CADENCE_TEST_SECS seconds with the lyrics page open, then pause and restore the volume.
fn test_play(ui: &slint::Weak<MainWindow>) {
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
        p.play(&uri, 0);
        tokio::time::sleep(Duration::from_secs(secs)).await;
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
    rt.spawn_blocking(cache::prune_all);

    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    // CADENCE_SMOKE: inactive window, snapshot, auto-quit, no network.
    // CADENCE_OFFLINE: inactive window, no network, runs until closed (for measuring idle memory).
    // CADENCE_DEMO: inactive window, lyrics only (see demo()).
    let (smoke, offline, demo_mode) = (flag("CADENCE_SMOKE"), flag("CADENCE_OFFLINE"), flag("CADENCE_DEMO"));
    let inactive = smoke || offline || demo_mode || flag("CADENCE_INACTIVE");

    let mut backend = slint::BackendSelector::new();
    if inactive {
        // Create the window without activating it so tests never steal focus.
        backend = backend.with_winit_window_attributes_hook(|attrs| attrs.with_active(false));
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
    ui.on_play_pause_key(|| with_player(|p| p.play_pause()));
    ui.on_lyrics_key(|k| lyrics_view::key(&k));
    ui.on_relayout_lyrics(lyrics_view::relayout);

    let pb = ui.global::<Playback>();
    pb.on_play_pause(|| with_player(|p| p.play_pause()));
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
            with_player(|p| p.set_repeat(!ui.global::<Playback>().get_repeat()));
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

    init_native(ui.as_weak(), 0);

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
    } else if !offline {
        rt.spawn(startup(ui.as_weak()));
        // Test runs only: CADENCE_SHOTS with the real library (inactive window, nothing played).
        if flag("CADENCE_INACTIVE") {
            schedule_shots(&ui);
        }
    }

    ui.run()?;

    // Disconnect the Connect device cleanly (waits at most 2 s).
    if let Some(p) = PLAYER.get() {
        rt.block_on(p.shutdown());
    }

    if SMOKE_FAILED.load(Ordering::Relaxed) {
        std::process::exit(1);
    }
    Ok(())
}

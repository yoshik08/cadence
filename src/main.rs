#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod art;
mod auth;
mod browse;
mod player;
mod smtc;

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
/// Bumped on every Track event; a slow art load for an earlier track checks it before applying.
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

/// Native HWND of the Slint window, for SMTC. Only `Some` once the winit window exists:
/// call it from inside the running event loop after `show()` (e.g. `slint::Timer::single_shot`
/// or `invoke_from_event_loop`), not before `run()`.
pub fn hwnd(window: &slint::Window) -> Option<isize> {
    match window.window_handle().window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(feature = "smoke")]
fn save_snapshot(window: &slint::Window) -> Result<(), String> {
    let buf = window.take_snapshot().map_err(|e| e.to_string())?;
    let path = std::env::var("CADENCE_SNAPSHOT").unwrap_or_else(|_| "smoke.png".into());
    let file = std::fs::File::create(&path).map_err(|e| format!("{path}: {e}"))?;
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
fn save_snapshot(_: &slint::Window) -> Result<(), String> {
    Ok(())
}

/// Creates the SMTC session on the UI thread once the native window exists (polls every 50 ms).
fn init_smtc(ui: slint::Weak<MainWindow>, tries: u32) {
    let delay = Duration::from_millis(if tries == 0 { 0 } else { 50 });
    slint::Timer::single_shot(delay, move || {
        let Some(win) = ui.upgrade() else { return };
        match hwnd(win.window()) {
            Some(h) => {
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
            None if tries < 40 => init_smtc(ui.clone(), tries + 1),
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

// Last fetched lyrics + track duration, so a window resize can re-split the word-mode
// rows for the new width without re-fetching. UI thread only.
thread_local! {
    static LAST_LYRICS: RefCell<Option<(player::Lyrics, u32)>> = const { RefCell::new(None) };
}

/// Max chars on one karaoke row for the current window, mirroring the NowPlayingView
/// geometry in ui/now_playing.slint (side column + gaps) and the LyricsPanel scale, so a
/// word-mode row never overflows the lyrics column at any window size.
fn lyric_row_budget(ui: &MainWindow) -> usize {
    let scale = ui.window().scale_factor();
    let w = ui.window().size().width as f32 / scale;
    let side_w = (w * 0.36).clamp(260.0, 520.0);
    let side_x = 36.0f32.max(w * 0.06);
    let gap = 36.0f32.max(w * 0.05);
    let panel_w = w - side_x - side_w - gap - side_x;
    let font = 42.0 * (panel_w / 900.0).clamp(0.65, 1.0);
    let text_w = (panel_w - 16.0).max(0.0); // two space-sm (8px) insets
    // Bold lyrics average ~0.5em per glyph; 0.58em leaves room for wide words.
    ((text_w / (0.58 * font)).floor() as usize).clamp(12, 60)
}

/// Build the per-word letter models for the karaoke line, assigning each word to visual
/// row 0 or 1 so the line wraps instead of truncating. `budget` is the max chars on one
/// row for the current window (see lyric_row_budget). Returns an empty model (and 1 row)
/// when the line has no word timings or cannot fit on two rows; the UI then falls back
/// to the plain wrapping Text for that line.
fn build_lyric_words(
    line: &player::LyricLine,
    budget: usize,
) -> (slint::ModelRc<LyricWord>, i32) {
    let empty = || (std::rc::Rc::new(slint::VecModel::<LyricWord>::default()).into(), 1);
    let text_words: Vec<&str> = line.text.split_whitespace().collect();
    if line.words.len() != text_words.len() || line.words.is_empty() {
        return empty();
    }
    // Greedy word wrap: a word moves to row 1 once it no longer fits on row 0; the line
    // falls back to plain text if it needs a third row or one word alone is too wide.
    let mut rows: Vec<i32> = Vec::with_capacity(text_words.len());
    let mut row = 0;
    let mut used = 0usize;
    for w in &text_words {
        let len = w.chars().count();
        if len > budget {
            return empty();
        }
        if used > 0 && used + 1 + len > budget {
            if row == 0 {
                row = 1;
                used = 0;
            } else {
                return empty();
            }
        }
        rows.push(row);
        used += len + usize::from(used > 0);
    }
    let word_rows = if row == 1 { 2 } else { 1 };
    let words = text_words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let s = line.words[i];
            let e = line.ends.get(i).copied().unwrap_or(s + 300);
            let chars: Vec<char> = w.chars().collect();
            let n = chars.len();
            let letters = chars
                .iter()
                .enumerate()
                .map(|(ci, ch)| {
                    let frac = if n <= 1 { 1.0 } else { ci as f32 / (n - 1) as f32 };
                    let start = s + ((e.saturating_sub(s)) as f32 * frac * 0.7) as u32;
                    LyricLetter {
                        ch: ch.to_string().into(),
                        start_ms: start as i32,
                    }
                })
                .collect::<Vec<_>>();
            LyricWord {
                letters: std::rc::Rc::new(slint::VecModel::from(letters)).into(),
                row: rows[i],
            }
        })
        .collect::<Vec<_>>();
    (std::rc::Rc::new(slint::VecModel::from(words)).into(), word_rows)
}

/// Map fetched lyrics to Slint models, splitting word-mode rows for the current width.
fn lyric_models(lyrics: &player::Lyrics, duration_ms: u32, budget: usize) -> Vec<LyricLine> {
    let mut lines = Vec::with_capacity(lyrics.lines.len());
    for (i, line) in lyrics.lines.iter().enumerate() {
        let end = if i + 1 < lyrics.lines.len() {
            lyrics.lines[i + 1].start_ms
        } else {
            duration_ms.max(line.start_ms)
        };
        let (words, word_rows) = build_lyric_words(line, budget);
        lines.push(LyricLine {
            text: line.text.clone().into(),
            start_ms: line.start_ms as i32,
            end_ms: end as i32,
            words,
            word_rows,
        });
    }
    lines
}

/// Rebuild the lyric models from the last fetched lyrics, re-splitting word-mode rows
/// for the current window width. No-op when no lyrics are loaded.
fn rebuild_lyrics(ui: &MainWindow) {
    LAST_LYRICS.with(|cell| {
        let guard = cell.borrow();
        let Some((lyrics, duration_ms)) = guard.as_ref() else {
            return;
        };
        let budget = lyric_row_budget(ui);
        let lines = lyric_models(lyrics, *duration_ms, budget);
        let pb = ui.global::<Playback>();
        pb.set_lyrics_synced(lyrics.synced);
        pb.set_lyrics_note(slint::SharedString::default());
        pb.set_lyrics(std::rc::Rc::new(slint::VecModel::from(lines)).into());
    });
}

/// UI thread: show the new track and kick off its art and lyrics.
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
    pb.set_lyrics(std::rc::Rc::new(slint::VecModel::default()).into());
    pb.set_lyrics_synced(false);
    pb.set_lyrics_note("Loading lyrics…".into());
    // Drop the previous track's lyrics so a resize before the fetch lands cannot
    // resurrect them.
    LAST_LYRICS.with(|cell| *cell.borrow_mut() = None);
    ui.global::<Browse>().set_current_uri(uri.clone().into());

    // Kick off lyrics fetch (Spotify color-lyrics first, fallback to LRCLIB)
    let weak_lyrics = ui.as_weak();
    let uri_lyrics = uri.clone();
    let title_lyrics = title.clone();
    let artist_lyrics = artist.clone();
    let album_lyrics = album.clone();
    rt.spawn(async move {
        let lyrics = match PLAYER.get() {
            Some(p) => {
                p.lyrics(
                    &uri_lyrics,
                    &title_lyrics,
                    &artist_lyrics,
                    &album_lyrics,
                    duration_ms,
                )
                .await
            }
            None => None,
        };
        let _ = weak_lyrics.upgrade_in_event_loop(move |ui| {
            if TRACK_GEN.load(Ordering::Relaxed) != seq {
                return;
            }
            let pb = ui.global::<Playback>();
            match lyrics {
                Some(l) if !l.lines.is_empty() => {
                    LAST_LYRICS.with(|cell| *cell.borrow_mut() = Some((l, duration_ms)));
                    rebuild_lyrics(&ui);
                }
                _ => {
                    LAST_LYRICS.with(|cell| *cell.borrow_mut() = None);
                    pb.set_lyrics(std::rc::Rc::new(slint::VecModel::default()).into());
                    pb.set_lyrics_synced(false);
                    pb.set_lyrics_note("No lyrics available for this track.".into());
                }
            }
        });
    });

    let Some(url) = art_url else {
        pb.set_art(slint::Image::default());
        pb.set_bar_art(slint::Image::default());
        return;
    };
    let scale = ui.window().scale_factor();
    let (big_px, bar_px) = ((440.0 * scale).round() as u32, (56.0 * scale).round() as u32);
    let weak = ui.as_weak();
    rt.spawn(async move {
        let (big, small) = tokio::join!(art::load(&url, big_px), art::load(&url, bar_px));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if TRACK_GEN.load(Ordering::Relaxed) != seq {
                return;
            }
            let pb = ui.global::<Playback>();
            let accent = big.as_ref().or(small.as_ref()).map(|a| a.accent);
            if let Some((r, g, b)) = accent {
                let c = slint::Color::from_rgb_u8(r, g, b);
                pb.set_accent(c);
                ui.global::<Browse>().set_accent(c);
            }
            pb.set_art(big.map_or_else(slint::Image::default, |a| slint::Image::from_rgb8(a.buf)));
            pb.set_bar_art(small.map_or_else(slint::Image::default, |a| slint::Image::from_rgb8(a.buf)));
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
            Event::Playing { position_ms } => {
                playing = true;
                smtc_update(title.clone(), artist.clone(), true, art_url.clone());
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let pb = ui.global::<Playback>();
                    pb.set_playing(true);
                    pb.set_position_ms(position_ms as i32);
                });
            }
            Event::Paused { position_ms } => {
                playing = false;
                smtc_update(title.clone(), artist.clone(), false, art_url.clone());
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let pb = ui.global::<Playback>();
                    pb.set_playing(false);
                    pb.set_position_ms(position_ms as i32);
                });
            }
            Event::Stopped => {
                playing = false;
                let _ = ui.upgrade_in_event_loop(|ui| ui.global::<Playback>().set_playing(false));
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

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
            windows::core::w!("Cadence"),
        );
    }

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

    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    // CADENCE_SMOKE: inactive window, snapshot, auto-quit, no network.
    // CADENCE_OFFLINE: inactive window, no network, runs until closed (for measuring idle memory).
    let (smoke, offline) = (flag("CADENCE_SMOKE"), flag("CADENCE_OFFLINE"));

    let mut backend = slint::BackendSelector::new();
    if smoke || offline {
        // Create the window without activating it so tests never steal focus.
        backend = backend.with_winit_window_attributes_hook(|attrs| attrs.with_active(false));
    }
    backend.select()?;

    let ui = MainWindow::new()?;
    ui.on_play_pause_key(|| with_player(|p| p.play_pause()));
    // Resize / fullscreen toggle: re-split the karaoke word-rows for the new width.
    let weak_lyrics = ui.as_weak();
    ui.on_relayout_lyrics(move || {
        if let Some(ui) = weak_lyrics.upgrade() {
            rebuild_lyrics(&ui);
        }
    });

    let pb = ui.global::<Playback>();
    pb.on_play_pause(|| with_player(|p| p.play_pause()));
    pb.on_next(|| with_player(|p| p.next()));
    pb.on_prev(|| with_player(|p| p.prev()));
    pb.on_seek(|ms| with_player(|p| p.seek(ms.max(0) as u32)));
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

    init_smtc(ui.as_weak(), 0);

    if smoke {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(1500), move || {
            if let Some(ui) = weak.upgrade() {
                eprintln!("smoke: hwnd = {:?}", hwnd(ui.window()));
                if let Err(e) = save_snapshot(ui.window()) {
                    eprintln!("smoke: snapshot failed: {e}");
                    SMOKE_FAILED.store(true, Ordering::Relaxed);
                }
            }
            let _ = slint::quit_event_loop();
        });
    } else if !offline {
        rt.spawn(startup(ui.as_weak()));
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

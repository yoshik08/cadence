//! Podcasts: episode playback with resume, progress saving, mark played, and playback speed
//! (local time-stretching in the audio sink, episodes only).

use crate::db;
use crate::{MainWindow, Playback};
use slint::ComponentHandle;
use std::sync::Mutex;

/// (uri, duration) of the episode playing now, if any.
static CURRENT: Mutex<Option<(String, u32)>> = Mutex::new(None);

/// Plays an episode from where it was left off (local progress, else from the start).
pub fn play_episode(uri: &str, duration_ms: u64) {
    crate::queue::forget_restore();
    let resume = db::episode_progress(uri).filter(|p| !p.2).map_or(0, |p| p.0.max(0) as u32);
    // Within the last 30 s counts as finished: start over.
    let resume = if duration_ms > 0 && resume as u64 + 30_000 >= duration_ms { 0 } else { resume };
    crate::with_player(|p| p.load(uri, librespot_connect::PlayingTrack::Index(0), resume, true));
}

/// Track change: remember the episode (or none) and apply the speed.
pub fn on_track(ui: &MainWindow, uri: &str, duration_ms: u32) {
    let episode = uri.starts_with("spotify:episode:");
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = episode.then(|| (uri.to_string(), duration_ms));
    let pb = ui.global::<Playback>();
    pb.set_is_episode(episode);
    let speed = if episode { crate::prefs::get("podcast_speed").as_f64().unwrap_or(1.0) as f32 } else { 1.0 };
    apply_speed(ui, speed);
}

/// Position updates (pause, seek, periodic): saves progress, marks played near the end.
pub fn on_position(position_ms: u32) {
    let cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some((uri, dur)) = cur {
        let played = dur > 0 && position_ms + 30_000 >= dur;
        std::thread::spawn(move || db::set_episode_progress(&uri, position_ms as i64, dur as i64, played));
    }
}

thread_local! {
    /// Saves the episode position every 15 s. Runs only while an episode is playing: with music
    /// or nothing playing, no timer exists.
    static SAVER: slint::Timer = slint::Timer::default();
}

/// Playback started or stopped (UI thread).
pub fn set_saving(ui: &MainWindow, playing: bool) {
    let on = playing && ui.global::<Playback>().get_is_episode();
    SAVER.with(|t| {
        if !on {
            return t.stop();
        }
        if t.running() {
            return;
        }
        let weak = ui.as_weak();
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(15), move || {
            if let Some(ui) = weak.upgrade() {
                on_position(ui.global::<Playback>().get_position_ms().max(0) as u32);
            }
        });
    });
}

fn apply_speed(ui: &MainWindow, speed: f32) {
    let speed = speed.clamp(0.5, 2.0);
    crate::audio::set_speed(speed);
    crate::lyrics_view::set_rate(speed);
    ui.global::<Playback>().set_speed(speed);
}

/// Speed menu: remembered for podcasts; music always plays at 1×.
pub fn set_speed(ui: &MainWindow, speed: f32) {
    crate::prefs::set("podcast_speed", serde_json::json!(speed));
    if ui.global::<Playback>().get_is_episode() {
        apply_speed(ui, speed);
    }
}

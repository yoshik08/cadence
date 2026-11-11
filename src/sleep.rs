//! Sleep timer: presets, end of track, custom. The volume fades over the last 15 seconds, then
//! playback pauses (the device stays connected) and the volume is restored. A manual pause
//! doesn't cancel it; skipping cancels "end of this track". Not kept across restarts.

use crate::{Dialog, MainWindow, Playback};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::time::{Duration, Instant};

const FADE: Duration = Duration::from_secs(15);

enum Mode {
    Off,
    At(Instant),
    EndOfTrack { uri: String },
}

struct State {
    mode: Mode,
    timer: slint::Timer,
    /// Volume before the fade started.
    saved_volume: Option<f32>,
}

thread_local! {
    static S: RefCell<State> = RefCell::new(State { mode: Mode::Off, timer: slint::Timer::default(), saved_volume: None });
}

fn label(left: Duration) -> String {
    let s = left.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

pub fn choose(ui: &MainWindow, what: &str) {
    match what {
        "off" => cancel(ui),
        "track" => {
            let uri = ui.global::<crate::Browse>().get_current_uri().to_string();
            if uri.is_empty() {
                return crate::menu::toast_text(ui, "Nothing is playing.");
            }
            set(ui, Mode::EndOfTrack { uri });
        }
        "custom" => {
            crate::dialogs::confirm(ui, "Sleep timer", "", "Start", false, |ui| {
                let mins: u64 = ui.global::<Dialog>().get_field1().trim().parse().unwrap_or(0);
                if (1..=600).contains(&mins) {
                    set(ui, Mode::At(Instant::now() + Duration::from_secs(mins * 60)));
                } else {
                    crate::menu::toast_text(ui, "Enter 1 to 600 minutes.");
                }
            });
            let d = ui.global::<Dialog>();
            d.set_field1_label("Minutes".into());
            d.set_field1("20".into());
        }
        m => {
            let mins: u64 = m.parse().unwrap_or(0);
            if mins > 0 {
                set(ui, Mode::At(Instant::now() + Duration::from_secs(mins * 60)));
            }
        }
    }
}

fn set(ui: &MainWindow, mode: Mode) {
    restore_volume(ui);
    let msg = match &mode {
        Mode::EndOfTrack { .. } => "Sleep timer: end of this track".to_string(),
        Mode::At(t) => format!("Sleep timer: {}", label(t.saturating_duration_since(Instant::now()))),
        Mode::Off => String::new(),
    };
    S.with(|s| s.borrow_mut().mode = mode);
    crate::menu::toast_text(ui, &msg);
    start_tick(ui);
}

pub fn cancel(ui: &MainWindow) {
    restore_volume(ui);
    S.with(|s| {
        let mut s = s.borrow_mut();
        s.mode = Mode::Off;
        s.timer.stop();
    });
    ui.global::<Playback>().set_sleep_label("".into());
}

fn restore_volume(ui: &MainWindow) {
    if let Some(v) = S.with(|s| s.borrow_mut().saved_volume.take()) {
        ui.global::<Playback>().set_volume(v);
        crate::with_player(|p| p.set_volume(v));
    }
}

fn start_tick(ui: &MainWindow) {
    let w = ui.as_weak();
    S.with(|s| {
        s.borrow().timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
            if let Some(ui) = w.upgrade() {
                tick(&ui);
            }
        })
    });
    tick(ui);
}

/// Time left (None = off; end-of-track uses the playback position).
fn left(ui: &MainWindow) -> Option<Duration> {
    S.with(|s| match &s.borrow().mode {
        Mode::Off => None,
        Mode::At(t) => Some(t.saturating_duration_since(Instant::now())),
        Mode::EndOfTrack { .. } => {
            let pb = ui.global::<Playback>();
            Some(Duration::from_millis((pb.get_duration_ms() - pb.get_position_ms()).max(0) as u64))
        }
    })
}

fn tick(ui: &MainWindow) {
    let Some(left) = left(ui) else {
        ui.global::<Playback>().set_sleep_label("".into());
        return;
    };
    let pb = ui.global::<Playback>();
    let eot = S.with(|s| matches!(s.borrow().mode, Mode::EndOfTrack { .. }));
    pb.set_sleep_label(if eot { "end of track".into() } else { label(left).into() });
    // Only fade while actually playing (a manual pause keeps the timer and the volume).
    if !pb.get_playing() {
        return;
    }
    if left <= FADE {
        let saved = S.with(|s| *s.borrow_mut().saved_volume.get_or_insert(pb.get_volume()));
        let v = saved * (left.as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0);
        crate::with_player(|p| p.set_volume(v));
    }
    if left <= Duration::from_millis(300) {
        crate::with_player(|p| p.pause());
        crate::logln!("sleep: timer ended, paused");
        let w = ui.as_weak();
        // Restore the volume once the pause has landed.
        slint::Timer::single_shot(Duration::from_millis(800), move || {
            if let Some(ui) = w.upgrade() {
                cancel(&ui);
                crate::menu::toast_text(&ui, "Sleep timer ended");
            }
        });
        S.with(|s| {
            let mut s = s.borrow_mut();
            s.mode = Mode::Off;
            s.timer.stop();
        });
    }
}

/// Track changed: a skip away from the "end of this track" track cancels the timer (natural
/// end lands at 0 left, which already paused).
pub fn on_track(ui: &MainWindow, uri: &str) {
    let cancel_it = S.with(|s| matches!(&s.borrow().mode, Mode::EndOfTrack { uri: u } if u != uri));
    if cancel_it {
        cancel(ui);
        crate::menu::toast_text(ui, "Sleep timer cancelled (track skipped)");
    }
}

//! Mini player window (always on top, remembers its position) and the media-key overlay.
//! Both are separate Slint windows, created on first use and kept (hidden) afterwards.

use crate::{MainWindow, MiniWindow, OsdWindow, Playback};
use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Image};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The next window created opens without activating and without a taskbar button (overlay).
pub static OVERLAY_NEXT: AtomicBool = AtomicBool::new(false);

thread_local! {
    static MINI: RefCell<Option<(MiniWindow, slint::Timer)>> = const { RefCell::new(None) };
    static OSD: RefCell<Option<(OsdWindow, slint::Timer)>> = const { RefCell::new(None) };
}

fn main_ui() -> Option<MainWindow> {
    crate::MAIN.with(|m| m.borrow().as_ref().and_then(|w| w.upgrade()))
}

fn create(ui: &MainWindow) -> Option<(MiniWindow, slint::Timer)> {
    let mini = MiniWindow::new().ok()?;
    mini.on_play_pause(crate::play_pause_cmd);
    mini.on_next(|| crate::with_player(|p| p.next()));
    mini.on_prev(|| crate::with_player(|p| p.prev()));
    mini.on_seek(|ms| crate::with_player(|p| p.seek(ms.max(0) as u32)));
    mini.on_set_volume(|v| {
        crate::with_player(|p| p.set_volume(v));
        if let Some(ui) = main_ui() {
            ui.global::<Playback>().set_volume(v);
        }
    });
    let w = mini.as_weak();
    mini.on_drag(move || {
        if let Some(m) = w.upgrade() {
            let _ = m.window().with_winit_window(|win| win.drag_window());
        }
    });
    let w = mini.as_weak();
    mini.on_close_mini(move || {
        if let Some(m) = w.upgrade() {
            save_position(&m);
            let _ = m.hide();
        }
        stop_timer();
    });
    let w = mini.as_weak();
    mini.on_expand(move || {
        if let Some(m) = w.upgrade() {
            save_position(&m);
            let _ = m.hide();
        }
        stop_timer();
        crate::show_main();
    });
    let x = crate::prefs::get("mini_x").as_i64();
    let y = crate::prefs::get("mini_y").as_i64();
    if let (Some(x), Some(y)) = (x, y) {
        mini.window().set_position(slint::PhysicalPosition::new(x as i32, y as i32));
    }
    sync_all(&mini, ui);
    // Started when the mini player is shown, stopped when it is hidden (see `run_timer`).
    Some((mini, slint::Timer::default()))
}

/// The seek bar follows playback only while the mini player is showing; hidden, nothing ticks.
fn run_timer(m: &MiniWindow, timer: &slint::Timer, on: bool) {
    if !on {
        return timer.stop();
    }
    let w = m.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
        if let (Some(m), Some(ui)) = (w.upgrade(), main_ui()) {
            let pb = ui.global::<Playback>();
            m.set_position_ms(pb.get_position_ms());
            m.set_duration_ms(pb.get_duration_ms());
            m.set_playing(pb.get_playing());
        }
    });
}

/// The mini player was hidden from one of its own buttons.
fn stop_timer() {
    MINI.with(|c| {
        if let Ok(c) = c.try_borrow()
            && let Some((_, timer)) = c.as_ref()
        {
            timer.stop();
        }
    });
}

fn save_position(m: &MiniWindow) {
    let p = m.window().position();
    crate::prefs::set("mini_x", serde_json::json!(p.x));
    crate::prefs::set("mini_y", serde_json::json!(p.y));
}

fn sync_all(m: &MiniWindow, ui: &MainWindow) {
    let pb = ui.global::<Playback>();
    m.set_track_title(pb.get_title());
    m.set_artist(pb.get_artist());
    m.set_art_fresh(pb.get_art_fresh());
    m.set_art(pb.get_bar_art());
    m.set_playing(pb.get_playing());
    m.set_volume(pb.get_volume());
    m.set_accent(pb.get_accent());
    m.set_position_ms(pb.get_position_ms());
    m.set_duration_ms(pb.get_duration_ms());
}

pub fn toggle(ui: &MainWindow) {
    MINI.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = create(ui);
        }
        if let Some((m, timer)) = c.as_ref() {
            if m.window().is_visible() {
                save_position(m);
                let _ = m.hide();
                run_timer(m, timer, false);
            } else {
                sync_all(m, ui);
                let _ = m.show();
                run_timer(m, timer, true);
            }
        }
    });
}

/// Track / state changes from the main window.
pub fn sync(ui: &MainWindow) {
    MINI.with(|c| {
        if let Some((m, _)) = c.borrow().as_ref()
            && m.window().is_visible()
        {
            sync_all(m, ui);
        }
    });
}

/// Media-key / hotkey overlay for 2 s (only while Cadence is in the background).
pub fn osd(ui: &MainWindow, line1: &str) {
    if !crate::prefs::bool_or("osd", false) || ui.window().is_visible() && is_foreground(ui) {
        return;
    }
    let pb = ui.global::<Playback>();
    let (title, artist, art): (String, String, Image) = (pb.get_title().into(), pb.get_artist().into(), pb.get_bar_art());
    OSD.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            OVERLAY_NEXT.store(true, Ordering::Relaxed);
            let Ok(w) = OsdWindow::new() else { return };
            *c = Some((w, slint::Timer::default()));
        }
        let Some((w, timer)) = c.as_ref() else { return };
        w.set_line1(line1.into());
        w.set_line2(title.into());
        w.set_line3(artist.into());
        w.set_art(art);
        // Bottom centre of the primary screen.
        let (sw, sh) = unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
            (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        };
        let scale = ui.window().scale_factor();
        let (ww, wh) = ((360.0 * scale) as i32, (84.0 * scale) as i32);
        w.window().set_position(slint::PhysicalPosition::new((sw - ww) / 2, sh - wh - (120.0 * scale) as i32));
        OVERLAY_NEXT.store(true, Ordering::Relaxed);
        let _ = w.show();
        let weak = w.as_weak();
        timer.start(slint::TimerMode::SingleShot, Duration::from_millis(2000), move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    });
}

fn is_foreground(ui: &MainWindow) -> bool {
    let Some(h) = crate::hwnd(ui.window()) else { return false };
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize == h }
}

pub fn main_has_focus(ui: &MainWindow) -> bool {
    is_foreground(ui)
}

/// Test runs: shows the mini player and, a moment later, writes a snapshot of it and logs the
/// size of the cover it was given against the size of its slot.
pub fn test_shot(ui: &MainWindow, path: String) {
    toggle(ui);
    let scale = ui.window().scale_factor();
    slint::Timer::single_shot(Duration::from_millis(1200), move || {
        MINI.with(|c| {
            if let Some((m, _)) = c.borrow().as_ref() {
                let slot = (m.get_cover_size() * scale).round();
                crate::logln!("mini: cover is {} px wide for a {slot} px slot (scale {scale})", m.get_art().size().width);
                if let Err(e) = crate::save_snapshot(m.window(), &path) {
                    crate::logln!("mini: snapshot failed: {e}");
                }
            }
        });
    });
}

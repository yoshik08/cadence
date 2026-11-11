//! Taskbar thumbnail toolbar: previous, play/pause, next under the window preview.
//!
//! The buttons can only be added once the taskbar button exists, which Windows signals with the
//! registered "TaskbarButtonCreated" message (sent again when Explorer restarts). The window
//! procedure belongs to winit, so a comctl32 subclass catches that message and the
//! `WM_COMMAND`/`THBN_CLICKED` clicks. Icons are drawn at runtime (white glyphs, anti-aliased).

use std::cell::{Cell, RefCell};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::Shell::{
    DefSubclassProc, ITaskbarList3, SetWindowSubclass, TaskbarList, THB_FLAGS, THB_ICON, THB_TOOLTIP, THBF_ENABLED,
    THBN_CLICKED, THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, RegisterWindowMessageW, SM_CXSMICON, WM_COMMAND,
};
use windows::core::w;

#[derive(Clone, Copy)]
pub enum Cmd {
    Prev,
    PlayPause,
    Next,
}

const ID_PREV: u32 = 1;
const ID_PLAY: u32 = 2;
const ID_NEXT: u32 = 3;

#[derive(Clone, Copy)]
enum Glyph {
    Prev,
    Play,
    Pause,
    Next,
}

struct Bar {
    hwnd: HWND,
    list: ITaskbarList3,
    icons: [HICON; 4],
    created_msg: u32,
    on_cmd: Box<dyn Fn(Cmd)>,
}

thread_local! {
    static BAR: RefCell<Option<Bar>> = const { RefCell::new(None) };
    static PLAYING: Cell<bool> = const { Cell::new(false) };
}

/// Coverage of a glyph at (x, y) in a 0..1 square.
fn inside(g: Glyph, x: f32, y: f32) -> bool {
    // Right-pointing triangle with its left edge at x0, tip at x1.
    let tri_r = |x0: f32, x1: f32| x >= x0 && x <= x1 && (y - 0.5).abs() <= 0.36 * (x1 - x) / (x1 - x0);
    let tri_l = |x0: f32, x1: f32| x >= x0 && x <= x1 && (y - 0.5).abs() <= 0.36 * (x - x0) / (x1 - x0);
    let bar = |x0: f32, x1: f32| x >= x0 && x <= x1 && (0.14..=0.86).contains(&y);
    match g {
        Glyph::Play => tri_r(0.26, 0.84),
        Glyph::Pause => bar(0.24, 0.42) || bar(0.58, 0.76),
        Glyph::Next => tri_r(0.16, 0.7) || bar(0.72, 0.84),
        Glyph::Prev => bar(0.16, 0.28) || tri_l(0.3, 0.84),
    }
}

unsafe fn make_icon(g: Glyph, px: i32) -> HICON {
    let n = px.max(16) as usize;
    let mut bgra = vec![0u8; n * n * 4];
    const SS: usize = 4;
    for py in 0..n {
        for px_ in 0..n {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px_ as f32 + (sx as f32 + 0.5) / SS as f32) / n as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / n as f32;
                    hits += inside(g, x, y) as u32;
                }
            }
            let a = (hits * 255 / (SS * SS) as u32) as u8;
            // Premultiplied white.
            bgra[(py * n + px_) * 4..][..4].copy_from_slice(&[a, a, a, a]);
        }
    }
    unsafe {
        let color = CreateBitmap(n as i32, n as i32, 1, 32, Some(bgra.as_ptr().cast()));
        let mask_bits = vec![0u8; n.div_ceil(16) * 2 * n];
        let mask = CreateBitmap(n as i32, n as i32, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn button(id: u32, icon: HICON, tip: &str) -> THUMBBUTTON {
    let mut b = THUMBBUTTON { dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS, iId: id, hIcon: icon, dwFlags: THBF_ENABLED, ..Default::default() };
    for (d, c) in b.szTip.iter_mut().zip(tip.encode_utf16().take(259)) {
        *d = c;
    }
    b
}

fn buttons(bar: &Bar) -> [THUMBBUTTON; 3] {
    let playing = PLAYING.get();
    [
        button(ID_PREV, bar.icons[0], "Previous"),
        if playing { button(ID_PLAY, bar.icons[2], "Pause") } else { button(ID_PLAY, bar.icons[1], "Play") },
        button(ID_NEXT, bar.icons[3], "Next"),
    ]
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_COMMAND && ((wp.0 >> 16) & 0xffff) as u32 == THBN_CLICKED {
        let cmd = match (wp.0 & 0xffff) as u32 {
            ID_PREV => Some(Cmd::Prev),
            ID_PLAY => Some(Cmd::PlayPause),
            ID_NEXT => Some(Cmd::Next),
            _ => None,
        };
        if let Some(cmd) = cmd {
            BAR.with(|b| {
                if let Ok(b) = b.try_borrow()
                    && let Some(bar) = b.as_ref()
                {
                    (bar.on_cmd)(cmd);
                }
            });
            return LRESULT(0);
        }
    }
    let created = BAR.with(|b| b.try_borrow().ok().and_then(|b| b.as_ref().map(|bar| bar.created_msg)));
    if created == Some(msg) {
        // Explorer (re)created the taskbar button: the buttons have to be added again.
        BAR.with(|b| {
            if let Ok(b) = b.try_borrow()
                && let Some(bar) = b.as_ref()
            {
                unsafe {
                    let _ = bar.list.HrInit();
                    let _ = bar.list.ThumbBarAddButtons(bar.hwnd, &buttons(bar));
                }
            }
        });
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

/// UI thread, once the HWND exists. Clicks run `on_cmd` on the UI thread.
pub fn install(hwnd: isize, on_cmd: impl Fn(Cmd) + 'static) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let hwnd = HWND(hwnd as *mut _);
        let list: ITaskbarList3 = match CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) {
            Ok(l) => l,
            Err(e) => {
                crate::logln!("thumbbar: no ITaskbarList3: {e}");
                return;
            }
        };
        let _ = list.HrInit();
        let px = GetSystemMetrics(SM_CXSMICON);
        let icons = [Glyph::Prev, Glyph::Play, Glyph::Pause, Glyph::Next].map(|g| make_icon(g, px));
        let created_msg = RegisterWindowMessageW(w!("TaskbarButtonCreated"));
        let bar = Bar { hwnd, list, icons, created_msg, on_cmd: Box::new(on_cmd) };
        // The taskbar button usually exists already (the window is shown before this runs);
        // if not, the TaskbarButtonCreated handler adds the buttons.
        let added = bar.list.ThumbBarAddButtons(hwnd, &buttons(&bar));
        BAR.with(|b| *b.borrow_mut() = Some(bar));
        let _ = SetWindowSubclass(hwnd, Some(proc), 0xCADE, 0);
        crate::logln!("thumbbar: installed (buttons added now: {})", added.is_ok());
    }
}

/// UI thread: swap the play/pause icon.
pub fn set_playing(playing: bool) {
    if PLAYING.replace(playing) == playing {
        return;
    }
    BAR.with(|b| {
        if let Some(bar) = b.borrow().as_ref() {
            unsafe {
                let _ = bar.list.ThumbBarUpdateButtons(bar.hwnd, &buttons(bar)[1..2]);
            }
        }
    });
}

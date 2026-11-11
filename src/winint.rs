//! Windows integration on a hidden window with its own thread: tray icon (playback controls,
//! audio output switcher), global hotkeys, pause on lock, plus start-with-Windows, native file
//! dialogs and toast notifications.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    PlayPause,
    Next,
    Prev,
    Like,
    CopyNowPlaying,
    Show,
    Mini,
    Quit,
    /// Output device id ("" = system default).
    Device(String),
    Locked,
}

#[derive(Default, Clone)]
struct TrayState {
    title: String,
    playing: bool,
    device: String,
}

static STATE: Mutex<Option<TrayState>> = Mutex::new(None);
static HWND_ID: OnceLock<isize> = OnceLock::new();
static ON_CMD: OnceLock<Box<dyn Fn(Cmd) + Send + Sync>> = OnceLock::new();
static DEVICES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

const WM_TRAY: u32 = WM_APP + 1;
const WM_HOTKEYS: u32 = WM_APP + 2;
const WM_TIP: u32 = WM_APP + 3;
const ID_PLAY: usize = 1;
const ID_NEXT: usize = 2;
const ID_PREV: usize = 3;
const ID_SHOW: usize = 4;
const ID_MINI: usize = 5;
const ID_QUIT: usize = 6;
const ID_DEFAULT_DEV: usize = 100;
const ID_DEV: usize = 101;
const HOTKEYS: [(i32, u32, Cmd); 5] = [
    (1, 0x20, Cmd::PlayPause),
    (2, 0x27, Cmd::Next),
    (3, 0x25, Cmd::Prev),
    (4, 0x4C, Cmd::Like),
    (5, 0x43, Cmd::CopyNowPlaying),
];

fn send(cmd: Cmd) {
    if let Some(f) = ON_CMD.get() {
        f(cmd);
    }
}

fn wide(s: &str, out: &mut [u16]) {
    let max = out.len() - 1;
    for (d, c) in out.iter_mut().zip(s.encode_utf16().take(max).chain(Some(0))) {
        *d = c;
    }
}

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    unsafe {
        let mut nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = crate::taskbar::icon_handle(GetSystemMetrics(SM_CXSMICON));
        let title = STATE.lock().ok().and_then(|s| s.as_ref().map(|s| s.title.clone())).unwrap_or_default();
        let tip = if title.is_empty() { "Cadence".to_string() } else { format!("Cadence · {title}") };
        wide(&tip, &mut nid.szTip);
        nid
    }
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let st = STATE.lock().ok().and_then(|s| s.clone()).unwrap_or_default();
        let Ok(menu) = CreatePopupMenu() else { return };
        if !st.title.is_empty() {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &HSTRING::from(st.title.chars().take(60).collect::<String>()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        let _ = AppendMenuW(menu, MF_STRING, ID_PLAY, &HSTRING::from(if st.playing { "Pause" } else { "Play" }));
        let _ = AppendMenuW(menu, MF_STRING, ID_NEXT, w!("Next"));
        let _ = AppendMenuW(menu, MF_STRING, ID_PREV, w!("Previous"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        if let Ok(sub) = CreatePopupMenu() {
            let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(sub, MF_STRING | check(st.device.is_empty()), ID_DEFAULT_DEV, w!("System default"));
            let devs = crate::audio::devices();
            for (i, (id, name)) in devs.iter().enumerate() {
                let _ = AppendMenuW(sub, MF_STRING | check(st.device == *id), ID_DEV + i, &HSTRING::from(name.as_str()));
            }
            *DEVICES.lock().unwrap_or_else(|e| e.into_inner()) = devs;
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Audio output"));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_SHOW, w!("Show Cadence"));
        let _ = AppendMenuW(menu, MF_STRING, ID_MINI, w!("Mini player"));
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
        let id = cmd.0 as usize;
        match id {
            ID_PLAY => send(Cmd::PlayPause),
            ID_NEXT => send(Cmd::Next),
            ID_PREV => send(Cmd::Prev),
            ID_SHOW => send(Cmd::Show),
            ID_MINI => send(Cmd::Mini),
            ID_QUIT => send(Cmd::Quit),
            ID_DEFAULT_DEV => send(Cmd::Device(String::new())),
            n if n >= ID_DEV => {
                if let Some((dev, _)) = DEVICES.lock().ok().and_then(|d| d.get(n - ID_DEV).cloned()) {
                    send(Cmd::Device(dev));
                }
            }
            _ => {}
        }
    }
}

static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                match (lp.0 & 0xffff) as u32 {
                    WM_LBUTTONUP => send(Cmd::Show),
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                if let Some((_, _, cmd)) = HOTKEYS.iter().find(|(id, _, _)| *id as usize == wp.0) {
                    send(cmd.clone());
                }
                LRESULT(0)
            }
            WM_HOTKEYS => {
                for (id, vk, _) in HOTKEYS {
                    let _ = UnregisterHotKey(Some(hwnd), id);
                    if wp.0 == 1 && RegisterHotKey(Some(hwnd), id, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, vk).is_err() {
                        crate::logln!("hotkeys: Ctrl+Alt+{vk:#x} is taken by another app");
                    }
                }
                LRESULT(0)
            }
            WM_TIP => {
                let nid = tray_data(hwnd);
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                LRESULT(0)
            }
            // WM_WTSSESSION_CHANGE / WTS_SESSION_LOCK
            0x02B1 if wp.0 == 7 => {
                send(Cmd::Locked);
                LRESULT(0)
            }
            WM_DESTROY => {
                let nid = tray_data(hwnd);
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if Some(&m) == TASKBAR_CREATED.get() => {
                // Explorer restarted: add the icon again.
                let nid = tray_data(hwnd);
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Starts the tray icon, hotkey and lock-notification window (once).
pub fn start(hotkeys: bool, on_cmd: impl Fn(Cmd) + Send + Sync + 'static) {
    let _ = ON_CMD.set(Box::new(on_cmd));
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(TrayState::default());
    let _ = std::thread::Builder::new().name("cadence-tray".into()).spawn(move || unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("CadenceTray");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        let Ok(hwnd) = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("Cadence tray"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(inst.into()), None) else {
            crate::logln!("tray: cannot create window");
            return;
        };
        let _ = HWND_ID.set(hwnd.0 as isize);
        let nid = tray_data(hwnd);
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            crate::logln!("tray: icon not added");
        }
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        let _ = PostMessageW(Some(hwnd), WM_HOTKEYS, WPARAM(hotkeys as usize), LPARAM(0));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

fn post(msg: u32, wp: usize) {
    if let Some(h) = HWND_ID.get() {
        unsafe {
            let _ = PostMessageW(Some(HWND(*h as *mut _)), msg, WPARAM(wp), LPARAM(0));
        }
    }
}

pub fn set_hotkeys(on: bool) {
    post(WM_HOTKEYS, on as usize);
}

/// Keeps the tray tooltip and menu current.
pub fn set_now_playing(title: &str, playing: bool) {
    if let Ok(mut s) = STATE.lock()
        && let Some(s) = s.as_mut()
    {
        let changed = s.title != title;
        s.title = title.to_string();
        s.playing = playing;
        if changed {
            post(WM_TIP, 0);
        }
    }
}

pub fn set_device_label(id: &str) {
    if let Ok(mut s) = STATE.lock()
        && let Some(s) = s.as_mut()
    {
        s.device = id.to_string();
    }
}

pub fn remove_tray() {
    if let Some(h) = HWND_ID.get() {
        unsafe {
            let _ = PostMessageW(Some(HWND(*h as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            let nid = tray_data(HWND(*h as *mut _));
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Start with Windows.

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

/// "off" | "normal" | "minimized".
pub fn set_run_at_login(mode: &str) {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    unsafe {
        if mode == "off" {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Cadence"));
            return;
        }
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let cmd = if mode == "minimized" { format!("\"{exe}\" --minimized") } else { format!("\"{exe}\"") };
        let wide: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Cadence"), REG_SZ.0, Some(wide.as_ptr().cast()), (wide.len() * 2) as u32);
    }
}

// ---------------------------------------------------------------------------------------------
// Native file dialogs. They run on their own thread (a modal dialog pumping messages on the UI
// thread would re-enter Slint); the result comes back on the UI thread.

static OWNER: OnceLock<isize> = OnceLock::new();

pub fn set_owner(hwnd: isize) {
    let _ = OWNER.set(hwnd);
}

fn dialog_thread(f: impl FnOnce(HWND) -> Option<PathBuf> + Send + 'static, done: impl FnOnce(PathBuf) + Send + 'static) {
    std::thread::spawn(move || unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        let owner = HWND(OWNER.get().copied().unwrap_or(0) as *mut _);
        if let Some(p) = f(owner) {
            let _ = slint::invoke_from_event_loop(move || done(p));
        }
    });
}

unsafe fn run_dialog(owner: HWND, save: bool, title: &str, default_name: &str, filters: &[(&str, &str)], folder: bool) -> Option<PathBuf> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, SIGDN_FILESYSPATH};
    use windows::core::Interface;
    unsafe {
        let dlg: IFileDialog = if save {
            CoCreateInstance::<_, IFileSaveDialog>(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?.cast().ok()?
        } else {
            CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?.cast().ok()?
        };
        let _ = dlg.SetTitle(&HSTRING::from(title));
        let mut opts = dlg.GetOptions().ok()? | FOS_FORCEFILESYSTEM;
        if folder {
            opts |= FOS_PICKFOLDERS;
        }
        if save {
            opts |= FOS_OVERWRITEPROMPT;
        }
        let _ = dlg.SetOptions(opts);
        let names: Vec<(HSTRING, HSTRING)> = filters.iter().map(|(n, p)| (HSTRING::from(*n), HSTRING::from(*p))).collect();
        let specs: Vec<COMDLG_FILTERSPEC> = names.iter().map(|(n, p)| COMDLG_FILTERSPEC { pszName: PCWSTR(n.as_ptr()), pszSpec: PCWSTR(p.as_ptr()) }).collect();
        if !specs.is_empty() && !folder {
            let _ = dlg.SetFileTypes(&specs);
        }
        if !default_name.is_empty() {
            let _ = dlg.SetFileName(&HSTRING::from(default_name));
            if let Some(ext) = default_name.rsplit('.').next() {
                let _ = dlg.SetDefaultExtension(&HSTRING::from(ext));
            }
        }
        let owner = (!owner.0.is_null()).then_some(owner);
        dlg.Show(owner).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn pick_file(title: &str, filters: &[(&str, &str)], done: impl FnOnce(PathBuf) + Send + 'static) {
    let (title, filters): (String, Vec<(String, String)>) = (title.into(), filters.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect());
    dialog_thread(
        move |o| unsafe {
            let f: Vec<(&str, &str)> = filters.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            run_dialog(o, false, &title, "", &f, false)
        },
        done,
    );
}

pub fn save_file(title: &str, name: &str, filters: &[(&str, &str)], done: impl FnOnce(PathBuf) + Send + 'static) {
    let (title, name, filters): (String, String, Vec<(String, String)>) = (title.into(), name.into(), filters.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect());
    dialog_thread(
        move |o| unsafe {
            let f: Vec<(&str, &str)> = filters.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            run_dialog(o, true, &title, &name, &f, false)
        },
        done,
    );
}

pub fn pick_folder(title: &str, done: impl FnOnce(PathBuf) + Send + 'static) {
    let title = title.to_string();
    dialog_thread(move |o| unsafe { run_dialog(o, false, &title, "", &[], true) }, done);
}

/// Opens a folder or URL with the shell.
pub fn shell_open(target: &str) {
    let _ = std::process::Command::new("explorer").arg(target).spawn();
}

// ---------------------------------------------------------------------------------------------
// Toast notifications (needs the Start menu shortcut with the app ID, see taskbar.rs).

pub fn toast(title: &str, body: &str, image: Option<&std::path::Path>) {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let img = image.map_or(String::new(), |p| format!("<image placement=\"appLogoOverride\" src=\"file:///{}\"/>", esc(&p.display().to_string().replace('\\', "/"))));
    let xml = format!(
        "<toast duration=\"short\"><visual><binding template=\"ToastGeneric\">{img}<text>{}</text><text>{}</text></binding></visual><audio silent=\"true\"/></toast>",
        esc(title),
        esc(body)
    );
    let r = (|| -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        // Replace the previous track's toast instead of stacking them.
        let _ = toast.SetTag(&HSTRING::from("track"));
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("Cadence"))?;
        notifier.Show(&toast)
    })();
    if let Err(e) = r {
        crate::logln!("toast: {e}");
    }
}

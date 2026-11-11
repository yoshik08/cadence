//! Windows taskbar identity. Cadence runs unpackaged, so Windows only knows its name and icon
//! when they are registered:
//! - the process AppUserModelID "Cadence", registered under
//!   `HKCU\Software\Classes\AppUserModelId\Cadence` with a DisplayName and IconUri, which the
//!   taskbar, the media flyout (SMTC) and notifications read;
//! - a Start menu shortcut `Cadence.lnk` carrying the same ID (System.AppUserModel.ID), name and
//!   icon. The media flyout resolves an unpackaged app's name and icon through such a shortcut;
//!   without it the flyout said "Unknown app";
//! - the same ID plus relaunch command / name / icon on the window itself, so a pinned taskbar
//!   button groups with the running window and launches Cadence again.

use windows::Win32::Foundation::{HWND, PROPERTYKEY};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_AppUserModel_ID, PKEY_AppUserModel_RelaunchCommand, PKEY_AppUserModel_RelaunchDisplayNameResource,
    PKEY_AppUserModel_RelaunchIconResource,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

const APP_ID: &str = "Cadence";

fn exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

/// The icon as a file the shell can read (IconUri needs a path, not a resource).
fn icon_file() -> Option<String> {
    let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("cadence");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("cadence.ico");
    let data: &[u8] = include_bytes!("../assets/cadence.ico");
    if std::fs::read(&path).ok().as_deref() != Some(data) {
        std::fs::write(&path, data).ok()?;
    }
    Some(path.display().to_string())
}

fn set_reg(name: PCWSTR, value: &str) {
    let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Classes\\AppUserModelId\\Cadence"),
            name,
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        );
    }
}

/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Cadence.lnk`, created when missing or
/// when its target exe no longer exists (Cadence moved). Only touched by Cadence itself.
fn ensure_shortcut() {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;
    let Some(appdata) = std::env::var_os("APPDATA") else { return };
    let dir = std::path::PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs");
    let path = dir.join("Cadence.lnk");
    let exe = exe();
    if exe.is_empty() {
        return;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else { return };
        let Ok(file) = link.cast::<IPersistFile>() else { return };
        let lnk = HSTRING::from(path.as_os_str());
        if path.exists() && file.Load(&lnk, STGM_READ).is_ok() {
            let mut buf = [0u16; 260];
            if link.GetPath(&mut buf, std::ptr::null_mut(), 0).is_ok() {
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                if std::path::Path::new(&String::from_utf16_lossy(&buf[..end])).exists() {
                    return;
                }
            }
        }
        let _ = link.SetPath(&HSTRING::from(exe.as_str()));
        let _ = link.SetIconLocation(&HSTRING::from(exe.as_str()), 0);
        let _ = link.SetDescription(w!("Cadence"));
        if let Ok(store) = link.cast::<IPropertyStore>() {
            let _ = store.SetValue(&PKEY_AppUserModel_ID, &prop_str(APP_ID));
            let _ = store.Commit();
        }
        let _ = std::fs::create_dir_all(&dir);
        match file.Save(&lnk, true) {
            Ok(()) => crate::logln!("taskbar: Start menu shortcut written"),
            Err(e) => crate::logln!("taskbar: cannot write Start menu shortcut: {e}"),
        }
    }
}

/// Call first thing in main(), before any window exists.
pub fn register_app_id() {
    ensure_shortcut();
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID));
    }
    set_reg(w!("DisplayName"), "Cadence");
    if let Some(icon) = icon_file() {
        set_reg(w!("IconUri"), &icon);
    }
}

/// A VT_LPWSTR PROPVARIANT. The string is leaked on purpose: this runs once per launch and the
/// property store copies the value.
fn prop_str(s: &str) -> PROPVARIANT {
    let wide: &'static mut [u16] = Vec::leak(s.encode_utf16().chain(Some(0)).collect());
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                Anonymous: PROPVARIANT_0_0_0 { pwszVal: PWSTR(wide.as_mut_ptr()) },
                ..Default::default()
            }),
        },
    }
}

/// The exe's icon resource at exactly `px` pixels (the .ico entry made for that size; a single
/// image stretched by Windows is what makes small icons blurry).
pub fn icon_handle(px: i32) -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{HICON, IMAGE_ICON, LR_SHARED, LoadImageW};
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        LoadImageW(Some(inst.into()), PCWSTR(1 as _), IMAGE_ICON, px, px, LR_SHARED).map_or(HICON::default(), |h| HICON(h.0))
    }
}

/// Title bar / Alt+Tab / taskbar icons of a window, each at the size Windows asks for.
pub fn set_window_icons(hwnd: isize) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, ICON_BIG, ICON_SMALL, SM_CXICON, SM_CXSMICON, SendMessageW, WM_SETICON};
    unsafe {
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let icon = icon_handle(GetSystemMetrics(metric));
            if !icon.is_invalid() {
                SendMessageW(HWND(hwnd as *mut _), WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}

/// Tags the main window (call once the HWND exists).
pub fn set_window_identity(hwnd: isize) {
    set_window_icons(hwnd);
    let exe = exe();
    let props: [(PROPERTYKEY, String); 4] = [
        (PKEY_AppUserModel_ID, APP_ID.to_string()),
        (PKEY_AppUserModel_RelaunchCommand, format!("\"{exe}\"")),
        (PKEY_AppUserModel_RelaunchDisplayNameResource, "Cadence".to_string()),
        (PKEY_AppUserModel_RelaunchIconResource, format!("{exe},0")),
    ];
    unsafe {
        let Ok(store) = SHGetPropertyStoreForWindow::<IPropertyStore>(HWND(hwnd as *mut _)) else { return };
        // The ID goes last: the shell reads the relaunch values when the ID is set.
        for (key, value) in props.iter().rev() {
            let v = prop_str(value);
            let _ = store.SetValue(key, &v);
        }
        let _ = store.Commit();
    }
}

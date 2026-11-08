//! Windows taskbar identity. Cadence runs unpackaged, so Windows only knows its name and icon
//! when they are registered:
//! - the process AppUserModelID "Cadence", registered under
//!   `HKCU\Software\Classes\AppUserModelId\Cadence` with a DisplayName and IconUri, which the
//!   taskbar, the media flyout (SMTC) and notifications read;
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
    let data: &[u8] = include_bytes!("../assets/icon.ico");
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

/// Call first thing in main(), before any window exists.
pub fn register_app_id() {
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

/// Tags the main window (call once the HWND exists).
pub fn set_window_identity(hwnd: isize) {
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

//! Settings > Updates and the Settings badge, on top of `update` (which holds the logic and tests).

use crate::{Browse, MainWindow, Settings, auth, logln, update};
use slint::ComponentHandle;
use std::sync::Mutex;
use std::time::Duration;

/// The newer release found by this launch's check.
static LATEST: Mutex<Option<update::Release>> = Mutex::new(None);

fn latest() -> Option<update::Release> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn dir() -> std::path::PathBuf {
    update::update_dir(&auth::data_dir())
}

fn exe() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()
}

/// Once per launch, a few seconds after start, on the runtime. Silent on any failure.
pub fn check_once(ui: slint::Weak<MainWindow>) {
    crate::runtime().spawn(async move {
        // A failed apply from the last start is reported once.
        if let Some(e) = update::take_error(&dir()) {
            let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Settings>().set_update_status(format!("The last update didn't install: {e} You're still on this version.").into()));
        }
        tokio::time::sleep(Duration::from_secs(8)).await;
        let Some(rel) = update::check(crate::http::client(), &update::api_url(), update::current()).await else {
            let _ = ui.upgrade_in_event_loop(|ui| {
                let st = ui.global::<Settings>();
                if !st.get_update_available() && st.get_update_status().starts_with("Checking") {
                    st.set_update_status("You're up to date.".into());
                }
            });
            return;
        };
        let staged = update::staged_version(&dir()) == Some(rel.version);
        let installable = exe().is_some_and(|e| update::can_install(&e));
        let v = rel.version;
        *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(rel);
        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.global::<Browse>().set_update_available(true);
            let st = ui.global::<Settings>();
            st.set_update_available(true);
            st.set_update_staged(staged);
            st.set_update_installable(installable);
            st.set_update_status(if staged {
                format!("Update available: v{v}. It installs the next time Cadence starts.")
            } else if installable {
                format!("Update available: v{v}")
            } else {
                format!("Update available: v{v}. Cadence can't write to its own folder, so download it from the release page.")
            }
            .into());
        });
    });
}

pub fn open_page() {
    let page = latest().map_or_else(|| update::RELEASES_PAGE.to_string(), |r| r.page);
    crate::winint::shell_open(&page);
}

/// "Update now" (`now`) or "Update on next restart".
pub fn start(ui: &MainWindow, now: bool) {
    let (Some(rel), Some(exe)) = (latest(), exe()) else { return };
    let st = ui.global::<Settings>();
    if st.get_update_busy() {
        return;
    }
    st.set_update_busy(true);
    st.set_update_progress(0.0);
    st.set_update_status(format!("Downloading v{}…", rel.version).into());
    let w = ui.as_weak();
    crate::runtime().spawn(async move {
        let dir = dir();
        let staged = if update::staged_version(&dir) == Some(rel.version) {
            Ok(())
        } else {
            let w2 = w.clone();
            let mut last = -1i32;
            update::download(crate::http::client(), &rel, &dir, move |p| {
                // ~100 UI updates per download, not one per chunk.
                let pct = (p * 100.0) as i32;
                if pct != last {
                    last = pct;
                    let _ = w2.upgrade_in_event_loop(move |ui| ui.global::<Settings>().set_update_progress(p));
                }
            })
            .await
            .map(|_| ())
        };
        let result = staged.and_then(|()| {
            if now {
                update::apply(&exe, &dir)?;
                update::relaunch(&exe)
            } else {
                Ok(())
            }
        });
        let v = rel.version;
        let _ = w.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<Settings>();
            st.set_update_busy(false);
            st.set_update_progress(-1.0);
            match result {
                Ok(()) if now => {
                    logln!("update: restarting into v{v}");
                    crate::queue::save();
                    let _ = slint::quit_event_loop();
                }
                Ok(()) => {
                    st.set_update_staged(true);
                    st.set_update_status(format!("v{v} is ready. It installs the next time Cadence starts.").into());
                }
                Err(e) => st.set_update_status(format!("Update failed: {e} You're still on this version.").into()),
            }
        });
    });
}

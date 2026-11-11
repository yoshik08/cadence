//! The Client ID card (ui/setup.slint): first launch without a valid Client ID, and Settings >
//! Change Client ID. The ID is validated and saved by `auth`; it is never logged.

use crate::{MainWindow, Setup, auth, logln, web};
use slint::ComponentHandle;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// First launch: Continue sends the saved ID here; `startup` waits on the other end.
static FIRST_RUN: Mutex<Option<mpsc::Sender<String>>> = Mutex::new(None);

const DASHBOARD: &str = "https://developer.spotify.com/dashboard";
pub const STEP_LIBRARY: &str = "Step 1 of 2: approve access to your library in the browser tab Cadence opened.";
pub const STEP_STREAMING: &str = "Step 2 of 2: approve streaming in the browser tab Cadence opened.";

pub fn init(ui: &MainWindow) {
    let s = ui.global::<Setup>();
    s.set_redirect_uri(web::REDIRECT.into());
    // Debug builds: CADENCE_TEST_SETUP=1 shows the card (snapshots with CADENCE_SMOKE).
    #[cfg(debug_assertions)]
    if std::env::var("CADENCE_TEST_SETUP").is_ok_and(|v| v == "1") {
        s.set_shown(true);
    }
    s.on_open_dashboard(|| crate::winint::shell_open(DASHBOARD));
    let w = ui.as_weak();
    s.on_copy_redirect(move || {
        if let Some(ui) = w.upgrade() {
            let ok = crate::clip::set_text(web::REDIRECT);
            ui.global::<Setup>().set_note(if ok { "Redirect URI copied." } else { "Couldn't copy. Select the address and copy it by hand." }.into());
        }
    });
    let w = ui.as_weak();
    s.on_paste(move || {
        if let (Some(ui), Some(t)) = (w.upgrade(), crate::clip::get_text()) {
            let s = ui.global::<Setup>();
            s.set_client_id(t.trim().into());
            s.set_error("".into());
        }
    });
    let w = ui.as_weak();
    s.on_cancel(move || {
        if let Some(ui) = w.upgrade() {
            close(&ui);
        }
    });
    let w = ui.as_weak();
    s.on_submit(move || {
        if let Some(ui) = w.upgrade() {
            submit(&ui);
        }
    });
}

pub fn close(ui: &MainWindow) {
    let s = ui.global::<Setup>();
    s.set_shown(false);
    s.set_changing(false);
    s.set_busy(false);
    s.set_step("".into());
    s.set_error("".into());
    s.set_note("".into());
}

fn submit(ui: &MainWindow) {
    let s = ui.global::<Setup>();
    if s.get_busy() {
        return;
    }
    let Some(id) = auth::valid_client_id(&s.get_client_id()) else {
        s.set_error(auth::CLIENT_ID_HINT.into());
        return;
    };
    s.set_error("".into());
    if s.get_changing() {
        change(ui, id);
        return;
    }
    match auth::save_client_id(&auth::data_dir(), &id) {
        Err(e) => s.set_error(e.into()),
        Ok(id) => {
            logln!("setup: Client ID saved");
            let tx = FIRST_RUN.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match tx {
                Some(tx) => {
                    s.set_busy(true);
                    s.set_step(STEP_LIBRARY.into());
                    let _ = tx.try_send(id);
                }
                None => close(ui),
            }
        }
    }
}

/// First launch: shows the card and waits until Continue saved a valid ID.
pub async fn wait_for_client_id(ui: &slint::Weak<MainWindow>) -> String {
    let (tx, mut rx) = mpsc::channel(1);
    *FIRST_RUN.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let _ = ui.upgrade_in_event_loop(|ui| {
        let s = ui.global::<Setup>();
        s.set_changing(false);
        s.set_shown(true);
    });
    logln!("setup: no Client ID yet, showing the setup card");
    // The sender lives in FIRST_RUN until `finish_first_run`, so this never ends with None.
    rx.recv().await.unwrap_or_default()
}

/// First launch: the Web API sign-in failed; the card stays and Continue tries again.
pub fn first_run_failed(ui: &slint::Weak<MainWindow>, msg: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let s = ui.global::<Setup>();
        s.set_busy(false);
        s.set_step("".into());
        s.set_error(msg.into());
    });
}

pub fn set_step(ui: &slint::Weak<MainWindow>, step: &'static str) {
    let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<Setup>().set_step(step.into()));
}

/// First launch done (or not needed): the card goes away for good.
pub fn finish_first_run(ui: &slint::Weak<MainWindow>) {
    if FIRST_RUN.lock().unwrap_or_else(|e| e.into_inner()).take().is_some() {
        logln!("setup: finished");
        let _ = ui.upgrade_in_event_loop(|ui| close(&ui));
    }
}

/// Settings > Change Client ID: opens the card in change mode.
pub fn open_change(ui: &MainWindow) {
    let s = ui.global::<Setup>();
    s.set_client_id("".into());
    s.set_error("".into());
    s.set_note("".into());
    s.set_step("".into());
    s.set_changing(true);
    s.set_shown(true);
}

/// Signs in with the new ID first (token to a side file). Only when that works are
/// client_id.txt and web_token.json replaced; then Cadence restarts on the new ID. A failed
/// sign-in changes nothing.
fn change(ui: &MainWindow, id: String) {
    let s = ui.global::<Setup>();
    s.set_busy(true);
    s.set_step("Approve access to your library in the browser tab Cadence opened.".into());
    let w = ui.as_weak();
    crate::runtime().spawn(async move {
        let dir = auth::data_dir();
        let side = dir.join("web_token.new.json");
        let r = web::Web::sign_in_to(&id, side.clone()).await;
        let r = match r {
            Err(e) => Err(if web::is_invalid_client(&e) { web::INVALID_CLIENT.to_string() } else { format!("Sign-in didn't finish: {e}") }),
            Ok(()) => auth::save_client_id(&dir, &id)
                .and_then(|_| std::fs::rename(&side, dir.join("web_token.json")).map_err(|e| format!("Couldn't save the new sign-in ({e}).")))
                .map(|_| ()),
        };
        let _ = std::fs::remove_file(&side);
        match &r {
            Ok(()) => logln!("setup: Client ID changed, restarting"),
            Err(_) => logln!("setup: Client ID change failed, nothing replaced"),
        }
        let _ = w.upgrade_in_event_loop(move |ui| match r {
            Ok(()) => crate::settings::restart(),
            Err(e) => {
                let s = ui.global::<Setup>();
                s.set_busy(false);
                s.set_step("".into());
                s.set_error(e.into());
            }
        });
    });
}

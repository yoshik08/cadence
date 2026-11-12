//! Settings page (ui/settings.slint) <-> `prefs`, and applying each setting. Settings that
//! librespot or the renderer read once at startup (quality, normalization, gapless, autoplay,
//! cache size, proxy, hardware acceleration) are marked "applies after a restart".

use crate::prefs;
use crate::{Browse, DeviceRow, MainWindow, Settings};
use serde_json::json;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

pub const EQ_PRESETS: [(&str, [f32; 6]); 6] = [
    ("flat", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("bass", [6.0, 4.0, 1.0, 0.0, 0.0, 0.0]),
    ("treble", [0.0, 0.0, 0.0, 1.0, 3.0, 5.0]),
    ("vocal", [-2.0, -1.0, 2.0, 4.0, 3.0, 0.0]),
    ("rock", [4.0, 2.0, -1.0, -1.0, 2.0, 4.0]),
    ("electronic", [5.0, 3.0, 0.0, -1.0, 2.0, 4.0]),
];

const RESTART_KEYS: [&str; 7] = ["quality", "normalize", "loudness", "gapless", "autoplay", "cache-gb", "gpu"];

pub fn eq_gains() -> [f32; 6] {
    let v = prefs::get("eq");
    let mut g = [0.0; 6];
    for (i, x) in g.iter_mut().enumerate() {
        *x = v[i].as_f64().unwrap_or(0.0) as f32;
    }
    g
}

/// At startup, before the player exists: audio processing and Windows settings.
pub fn apply_startup() {
    crate::dsp::set_eq(prefs::bool_or("eq_on", false), eq_gains());
    crate::dsp::set_mono(prefs::bool_or("mono", false));
    let dev = prefs::get("device").as_str().unwrap_or("").to_string();
    crate::audio::set_device((!dev.is_empty()).then_some(dev));
}

fn str_pref(key: &str, default: &str) -> String {
    prefs::get(key).as_str().unwrap_or(default).to_string()
}

/// Fills the page from prefs and live state.
pub fn refresh(ui: &MainWindow) {
    let st = ui.global::<Settings>();
    st.set_quality(str_pref("quality", "veryhigh").into());
    st.set_normalize(prefs::bool_or("normalize", false));
    st.set_loudness(str_pref("loudness", "normal").into());
    st.set_gapless(prefs::bool_or("gapless", true));
    st.set_autoplay(prefs::bool_or("autoplay", true));
    st.set_mono(prefs::bool_or("mono", false));
    st.set_explicit_filter(prefs::bool_or("explicit_filter", false));
    st.set_shuffle_style(str_pref("shuffle_style", "random").into());
    st.set_eq_on(prefs::bool_or("eq_on", false));
    st.set_eq(ModelRc::new(VecModel::from(eq_gains().to_vec())));
    st.set_eq_preset(str_pref("eq_preset", "flat").into());
    st.set_device(str_pref("device", "").into());
    st.set_zoom(prefs::u64_or("zoom", 100) as i32);
    st.set_compact(prefs::bool_or("compact", false));
    st.set_panel_on_play(prefs::bool_or("panel_on_play", false));
    st.set_notify(prefs::bool_or("notify", false));
    st.set_osd(prefs::bool_or("osd", false));
    st.set_on_login(str_pref("on_login", "off").into());
    st.set_close_to_tray(prefs::bool_or("close_to_tray", false));
    st.set_hotkeys(prefs::bool_or("hotkeys", true));
    st.set_bt_pause(prefs::bool_or("bt_pause", true));
    st.set_bt_resume(prefs::bool_or("bt_resume", false));
    st.set_lock_pause(prefs::bool_or("lock_pause", false));
    st.set_cache_gb(prefs::u64_or("cache_gb", 1) as i32);
    st.set_proxy(str_pref("proxy", "").into());
    st.set_gpu(prefs::bool_or("gpu", true));
    st.set_show_local(prefs::bool_or("show_local", false));
    st.set_local_folders(ModelRc::new(VecModel::from(crate::local::folders().into_iter().map(SharedString::from).collect::<Vec<_>>())));
    st.set_debug_log(prefs::bool_or("debug_log", false));
    st.set_version(env!("CARGO_PKG_VERSION").into());
    st.set_client_id_masked(crate::auth::read_client_id(&crate::auth::data_dir()).map(|id| crate::auth::mask_client_id(&id)).unwrap_or_default().into());
    refresh_devices(ui);
    refresh_info(ui);
}

fn refresh_devices(ui: &MainWindow) {
    let devs: Vec<DeviceRow> = crate::audio::devices().into_iter().map(|(id, name)| DeviceRow { id: id.into(), name: name.into() }).collect();
    ui.global::<Settings>().set_devices(ModelRc::new(VecModel::from(devs)));
}

fn dir_size(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p).map_or(0, |rd| {
        rd.flatten().map(|e| e.metadata().map_or(0, |m| if m.is_dir() { dir_size(&e.path()) } else { m.len() })).sum()
    })
}

fn refresh_info(ui: &MainWindow) {
    let st = ui.global::<Settings>();
    let (ws, private) = crate::mem::usage_mb();
    st.set_memory(format!("Working set {ws:.0} MB of the {:.0} MB budget (private {private:.0} MB).", crate::mem::CAP_MB).into());
    let audio = crate::cache::dir("audio");
    let mb = dir_size(&audio) as f64 / (1024.0 * 1024.0);
    st.set_cache_info(format!("{mb:.0} MB used in {}", audio.display()).into());
    let w = ui.as_weak();
    crate::runtime().spawn(async move {
        let info = match crate::web() {
            None => "Not signed in yet. Cadence signs in to Spotify when it starts.".to_string(),
            Some(web) => match web.days_left().await {
                Some(d) if d <= 0 => "Your sign-in has expired; Re-authorize to continue.".to_string(),
                Some(d) => format!("Signed in. Spotify asks you to sign in again in about {d} days (6 months after you first authorized)."),
                None => "Signed in.".to_string(),
            },
        };
        let _ = w.upgrade_in_event_loop(move |ui| ui.global::<Settings>().set_account_info(info.into()));
    });
}

pub fn init(ui: &MainWindow) {
    let st = ui.global::<Settings>();
    let w = ui.as_weak();
    st.on_changed(move |key| {
        if let Some(ui) = w.upgrade() {
            changed(&ui, &key);
        }
    });
    let w = ui.as_weak();
    st.on_action(move |a| {
        if let Some(ui) = w.upgrade() {
            action(&ui, &a);
        }
    });
    let w = ui.as_weak();
    st.on_eq_band(move |i, v| {
        if let Some(ui) = w.upgrade() {
            let mut g = eq_gains();
            if let Some(x) = g.get_mut(i as usize) {
                *x = v.round().clamp(-12.0, 12.0);
            }
            prefs::set("eq", json!(g));
            prefs::set("eq_preset", json!("custom"));
            crate::dsp::set_eq(prefs::bool_or("eq_on", false), g);
            let st = ui.global::<Settings>();
            st.set_eq(ModelRc::new(VecModel::from(g.to_vec())));
            st.set_eq_preset("custom".into());
        }
    });
}

fn changed(ui: &MainWindow, key: &str) {
    let st = ui.global::<Settings>();
    match key {
        "quality" => prefs::set("quality", json!(st.get_quality().as_str())),
        "normalize" => prefs::set("normalize", json!(st.get_normalize())),
        "loudness" => prefs::set("loudness", json!(st.get_loudness().as_str())),
        "gapless" => prefs::set("gapless", json!(st.get_gapless())),
        "autoplay" => prefs::set("autoplay", json!(st.get_autoplay())),
        "mono" => {
            prefs::set("mono", json!(st.get_mono()));
            crate::dsp::set_mono(st.get_mono());
        }
        "explicit" => {
            prefs::set("explicit_filter", json!(st.get_explicit_filter()));
            crate::browse::reapply_filters();
        }
        "shuffle-style" => prefs::set("shuffle_style", json!(st.get_shuffle_style().as_str())),
        "eq-on" => {
            prefs::set("eq_on", json!(st.get_eq_on()));
            crate::dsp::set_eq(st.get_eq_on(), eq_gains());
        }
        "device" => {
            let id = st.get_device().to_string();
            prefs::set("device", json!(id));
            crate::winint::set_device_label(&id);
            crate::audio::set_device((!id.is_empty()).then_some(id));
        }
        "compact" => {
            prefs::set("compact", json!(st.get_compact()));
            ui.global::<Browse>().set_compact(st.get_compact());
        }
        "panel-on-play" => prefs::set("panel_on_play", json!(st.get_panel_on_play())),
        "notify" => prefs::set("notify", json!(st.get_notify())),
        "osd" => prefs::set("osd", json!(st.get_osd())),
        "on-login" => {
            let m = st.get_on_login().to_string();
            prefs::set("on_login", json!(m));
            crate::winint::set_run_at_login(&m);
        }
        "close-to-tray" => prefs::set("close_to_tray", json!(st.get_close_to_tray())),
        "hotkeys" => {
            prefs::set("hotkeys", json!(st.get_hotkeys()));
            crate::winint::set_hotkeys(st.get_hotkeys());
        }
        "bt-pause" => prefs::set("bt_pause", json!(st.get_bt_pause())),
        "bt-resume" => prefs::set("bt_resume", json!(st.get_bt_resume())),
        "lock-pause" => prefs::set("lock_pause", json!(st.get_lock_pause())),
        "cache-gb" => prefs::set("cache_gb", json!(st.get_cache_gb())),
        "proxy" => {
            let p = st.get_proxy().trim().to_string();
            if !p.is_empty() && reqwest::Url::parse(&p).is_err() {
                st.set_note("That proxy address isn't a valid URL (example: http://host:8080).".into());
                return;
            }
            prefs::set("proxy", json!(p));
            st.set_restart_needed(true);
        }
        "gpu" => prefs::set("gpu", json!(st.get_gpu())),
        "show-local" => {
            prefs::set("show_local", json!(st.get_show_local()));
            ui.global::<Browse>().set_show_local(st.get_show_local());
            crate::browse::reload_sidebar(ui);
        }
        "debug-log" => prefs::set("debug_log", json!(st.get_debug_log())),
        _ => {}
    }
    if RESTART_KEYS.contains(&key) {
        st.set_restart_needed(true);
    }
}

fn action(ui: &MainWindow, a: &str) {
    let st = ui.global::<Settings>();
    match a {
        "restart" => restart(),
        "devices" => refresh_devices(ui),
        "memory" => refresh_info(ui),
        "zoom-in" | "zoom-out" | "zoom-reset" => zoom(ui, a),
        "cache-open" => crate::winint::shell_open(&crate::cache::dir("audio").display().to_string()),
        "cache-clear" => {
            crate::dialogs::confirm(ui, "Clear the cache?", "Deletes downloaded audio, cover art, lyrics and cached library pages. Your settings and sign-in stay.", "Clear", true, |ui| {
                for sub in ["audio", "art", "lyrics", "api"] {
                    let d = crate::cache::dir(sub);
                    if let Ok(rd) = std::fs::read_dir(&d) {
                        for e in rd.flatten() {
                            let _ = if e.path().is_dir() { std::fs::remove_dir_all(e.path()) } else { std::fs::remove_file(e.path()) };
                        }
                    }
                }
                refresh_info(ui);
                crate::menu::toast_text(ui, "Cache cleared");
            });
        }
        "log-open" => crate::winint::shell_open(&crate::cache::dir("..").display().to_string()),
        "update-now" => crate::updates_ui::start(ui, true),
        "update-later" => crate::updates_ui::start(ui, false),
        "update-page" => crate::updates_ui::open_page(),
        "change-client-id" => crate::setup::open_change(ui),
        "sign-out" => {
            crate::dialogs::confirm(ui, "Sign out of Spotify?", "Cadence removes its Spotify sign-in from this PC and quits. Your Client ID and settings stay; the next start asks you to approve again.", "Sign out", true, |_| {
                crate::queue::save();
                crate::auth::sign_out(&crate::auth::data_dir());
                let _ = slint::quit_event_loop();
            });
        }
        "account-apps" => crate::winint::shell_open("https://www.spotify.com/account/apps/"),
        "reauth" => {
            let Some(web) = crate::web() else {
                return st.set_note("Not signed in yet: restart Cadence to sign in, or use Change Client ID.".into());
            };
            st.set_note("Sign in to Spotify in your browser…".into());
            let w = ui.as_weak();
            crate::runtime().spawn(async move {
                let r = web.reauthorize().await;
                let _ = w.upgrade_in_event_loop(move |ui| {
                    let st = ui.global::<Settings>();
                    match r {
                        Ok(()) => {
                            st.set_note("Signed in again.".into());
                            refresh_info(&ui);
                        }
                        Err(e) => st.set_note(format!("Sign-in didn't finish: {e}").into()),
                    }
                });
            });
        }
        "local-add" => {
            let w = ui.as_weak();
            crate::winint::pick_folder("Add a music folder", move |p| {
                let mut list = crate::local::folders();
                let s = p.display().to_string();
                if !list.contains(&s) {
                    list.push(s);
                }
                crate::local::set_folders(&list);
                prefs::set("show_local", json!(true));
                if let Some(ui) = w.upgrade() {
                    ui.global::<Browse>().set_show_local(true);
                    refresh(&ui);
                    crate::browse::reload_sidebar(&ui);
                    crate::menu::toast_text(&ui, "Folder added; scanning…");
                }
            });
        }
        "local-rescan" => {
            crate::local::rescan();
            crate::menu::toast_text(ui, "Rescanning…");
        }
        x if x.starts_with("local-remove:") => {
            let f = &x["local-remove:".len()..];
            let list: Vec<String> = crate::local::folders().into_iter().filter(|p| p != f).collect();
            crate::local::set_folders(&list);
            refresh(ui);
        }
        x if x.starts_with("eq:") => {
            let name = &x[3..];
            if let Some((_, g)) = EQ_PRESETS.iter().find(|(n, _)| *n == name) {
                prefs::set("eq", json!(g));
                prefs::set("eq_preset", json!(name));
                crate::dsp::set_eq(prefs::bool_or("eq_on", false), *g);
                st.set_eq(ModelRc::new(VecModel::from(g.to_vec())));
                st.set_eq_preset(name.into());
            }
        }
        _ => {}
    }
}

/// Relaunches Cadence (settings read at startup).
pub fn restart() {
    if let Ok(exe) = std::env::current_exe() {
        crate::queue::save();
        let _ = std::process::Command::new(exe).spawn();
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    }
}

/// Zoom 70–130 % on top of the system scale.
pub fn zoom(ui: &MainWindow, how: &str) {
    let cur = prefs::u64_or("zoom", 100) as i32;
    let next = match how {
        "zoom-in" => (cur + 10).min(130),
        "zoom-out" => (cur - 10).max(70),
        _ => 100,
    };
    prefs::set("zoom", json!(next));
    ui.global::<Settings>().set_zoom(next);
    apply_zoom(ui);
}

pub fn apply_zoom(ui: &MainWindow) {
    use slint::winit_030::WinitWindowAccessor;
    let z = prefs::u64_or("zoom", 100) as f32 / 100.0;
    let base = ui.window().with_winit_window(|w| w.scale_factor() as f32).unwrap_or(1.0);
    ui.window().dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: base * z });
    crate::recheck_now_art(ui);
}

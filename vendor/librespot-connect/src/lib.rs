#![warn(missing_docs)]
#![doc=include_str!("../README.md")]

#[macro_use]
extern crate log;

use librespot_core as core;
use librespot_playback as playback;
use librespot_protocol as protocol;

mod context_resolver;
mod model;
mod shuffle_vec;
mod spirc;
mod state;

pub use model::*;
pub use spirc::*;
pub use state::*;

/// Cadence patch: one entry of the play queue.
#[derive(Clone, Debug, Default)]
pub struct QueueItem {
    /// Track or episode uri.
    pub uri: String,
    /// Connect uid (stable while the item is queued).
    pub uid: String,
    /// In the user queue (as opposed to the playing context).
    pub queued: bool,
    /// Comes from the autoplay context (similar songs after the context ended).
    pub autoplay: bool,
}

/// Cadence patch: where the autoplay request / result / failure lines go, besides `debug!`.
static AUTOPLAY_LOG: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Cadence patch: sends autoplay log lines to `f` as well (set once, at startup).
pub fn set_autoplay_log(f: fn(&str)) {
    let _ = AUTOPLAY_LOG.set(f);
}

pub(crate) fn autoplay_log(msg: &str) {
    debug!("{msg}");
    if let Some(f) = AUTOPLAY_LOG.get() {
        f(msg)
    }
}

/// Cadence patch: the Connect play queue as this device sees it.
#[derive(Clone, Debug, Default)]
pub struct QueueSnapshot {
    /// The playing track.
    pub current: Option<QueueItem>,
    /// Upcoming: user queue first, then context.
    pub next: Vec<QueueItem>,
    /// Recently played, oldest first.
    pub prev: Vec<QueueItem>,
}

//! Local listening stats: Cadence records its own plays (a play counts after 30 s, or half of a
//! shorter track). Paused time doesn't count. Private session stops recording. Nothing is
//! imported from or sent to Spotify.

use std::sync::Mutex;
use std::time::Instant;

struct Current {
    uri: String,
    title: String,
    artist: String,
    album: String,
    duration_ms: u32,
    listened_ms: u64,
    since: Option<Instant>,
}

static CUR: Mutex<Option<Current>> = Mutex::new(None);

fn finish(c: Current) {
    let listened = c.listened_ms + c.since.map_or(0, |s| s.elapsed().as_millis() as u64);
    let enough = listened >= 30_000 || (c.duration_ms > 0 && listened * 2 >= c.duration_ms as u64);
    if !enough || c.uri.is_empty() || crate::prefs::bool_or("private_session", false) {
        return;
    }
    crate::runtime().spawn(async move {
        let meta = if c.uri.starts_with("spotify:track:") { crate::queue::player_meta(&c.uri).await } else { None };
        let (artist_uri, album_uri) = meta.map_or((String::new(), String::new()), |m| (m.artist_uri, m.album_uri));
        let _ = tokio::task::spawn_blocking(move || {
            crate::db::record_play(&crate::db::Play {
                uri: &c.uri,
                title: &c.title,
                artist: &c.artist,
                artist_uri: &artist_uri,
                album: &c.album,
                album_uri: &album_uri,
                ms: listened as i64,
            })
        })
        .await;
    });
}

pub fn on_track(uri: &str, title: &str, artist: &str, album: &str, duration_ms: u32) {
    let prev = CUR.lock().unwrap_or_else(|e| e.into_inner()).replace(Current {
        uri: uri.into(),
        title: title.into(),
        artist: artist.into(),
        album: album.into(),
        duration_ms,
        listened_ms: 0,
        since: None,
    });
    if let Some(p) = prev {
        finish(p);
    }
}

pub fn on_playing(playing: bool) {
    let mut g = CUR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        match (playing, c.since) {
            (true, None) => c.since = Some(Instant::now()),
            (false, Some(s)) => {
                c.listened_ms += s.elapsed().as_millis() as u64;
                c.since = None;
            }
            _ => {}
        }
    }
}

/// Exit: count the track in progress.
pub fn flush() {
    if let Some(c) = CUR.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let listened = c.listened_ms + c.since.map_or(0, |s| s.elapsed().as_millis() as u64);
        if (listened >= 30_000 || (c.duration_ms > 0 && listened * 2 >= c.duration_ms as u64)) && !crate::prefs::bool_or("private_session", false) {
            crate::db::record_play(&crate::db::Play { uri: &c.uri, title: &c.title, artist: &c.artist, artist_uri: "", album: &c.album, album_uri: "", ms: listened as i64 });
        }
    }
}

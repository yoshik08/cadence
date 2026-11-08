#![allow(dead_code)]

use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::{
    authentication::Credentials,
    cache::Cache,
    config::{DeviceType, SessionConfig},
    session::Session,
    SpotifyUri,
};
use serde_json::Value;
use librespot_metadata::{artist::ArtistRole, audio::UniqueFields};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, PlayerConfig},
    mixer::{self, Mixer, MixerConfig},
    player::{Player as LsPlayer, PlayerEvent},
};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU16, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

pub enum Event {
    Track {
        uri: String,
        title: String,
        artist: String,
        album: String,
        art_url: Option<String>,
        duration_ms: u32,
    },
    Playing { position_ms: u32 },
    Paused { position_ms: u32 },
    Stopped,
    Volume(f32 /*0..1*/),
    Shuffle(bool),
    Repeat(bool),
    Error(String),
}

pub struct Player {
    spirc: Spirc,
    session: Session,
    mixer: Arc<dyn Mixer>,
    tx: UnboundedSender<Event>,
    // Last known shuffle/repeat/volume: Spirc resets shuffle/repeat on every load and ignores
    // volume commands while this device is not the active Connect device, so we re-apply them.
    shuffle: Arc<AtomicBool>,
    repeat: Arc<AtomicBool>,
    volume: Arc<AtomicU16>,
    closing: Arc<AtomicBool>,
    spirc_task: Mutex<Option<JoinHandle<()>>>,
    _event_task: JoinHandle<()>,
}

const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Player>();
};

impl Player {
    pub async fn start(
        creds: Credentials,
        cache: Cache,
    ) -> Result<(Player, UnboundedReceiver<Event>), String> {
        let initial_volume = cache.volume().unwrap_or(u16::MAX / 2);

        // Default client id on Windows is the keymaster id, which matches the librespot-oauth token.
        let session_config = SessionConfig {
            device_id: device_id(),
            ..Default::default()
        };
        let session = Session::new(session_config, Some(cache));

        let mixer = mixer::find(None).ok_or("no mixer")?(MixerConfig::default())
            .map_err(|e| e.to_string())?;
        let backend = audio_backend::find(None).ok_or("no audio backend")?;
        let player_config = PlayerConfig {
            gapless: true,
            normalisation: true,
            ..Default::default()
        };
        let player = LsPlayer::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || backend(None, AudioFormat::F32),
        );
        let player_events = player.get_player_event_channel();

        let connect_config = ConnectConfig {
            name: "Cadence".into(),
            device_type: DeviceType::Computer,
            initial_volume,
            ..Default::default()
        };
        // Spirc::new connects the session itself with store_credentials = true, so the reusable
        // credentials end up in the Cache (credentials.json) after the first successful login.
        let (spirc, spirc_future) =
            Spirc::new(connect_config, session.clone(), creds, player, mixer.clone())
                .await
                .map_err(|e| e.to_string())?;

        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(Event::Volume(initial_volume as f32 / u16::MAX as f32));

        let closing = Arc::new(AtomicBool::new(false));
        let spirc_task = {
            let (tx, closing) = (tx.clone(), closing.clone());
            tokio::spawn(async move {
                spirc_future.await;
                if !closing.load(Ordering::Relaxed) {
                    let _ = tx.send(Event::Error("Spotify connection closed".into()));
                }
            })
        };

        let shuffle = Arc::new(AtomicBool::new(false));
        let repeat = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU16::new(initial_volume));
        let event_task = tokio::spawn(map_events(
            player_events,
            tx.clone(),
            shuffle.clone(),
            repeat.clone(),
            volume.clone(),
        ));

        let player = Player {
            spirc,
            session,
            mixer,
            tx,
            shuffle,
            repeat,
            volume,
            closing,
            spirc_task: Mutex::new(Some(spirc_task)),
            _event_task: event_task,
        };
        Ok((player, rx))
    }

    /// Playlist/album/collection context, or a bare `spotify:track:..` uri (use index 0).
    pub fn play(&self, context_uri: &str, index: usize) {
        let options = LoadRequestOptions {
            start_playing: true,
            seek_to: 0,
            context_options: Some(LoadContextOptions::Options(Options {
                shuffle: self.shuffle.load(Ordering::Relaxed),
                repeat: self.repeat.load(Ordering::Relaxed),
                repeat_track: false,
            })),
            playing_track: Some(PlayingTrack::Index(index as u32)),
        };
        let request = if context_uri.starts_with("spotify:track:") {
            LoadRequest::from_tracks(vec![context_uri.to_owned()], options)
        } else {
            LoadRequest::from_context_uri(context_uri.to_owned(), options)
        };
        // Commands are processed in order and everything but activate/transfer is ignored while
        // this device is not active, so: activate, resync volume, then load.
        self.report(self.spirc.activate());
        self.report(self.spirc.set_volume(self.volume.load(Ordering::Relaxed)));
        self.report(self.spirc.load(request));
    }

    pub fn play_pause(&self) {
        self.report(self.spirc.play_pause());
    }

    pub fn next(&self) {
        self.report(self.spirc.next());
    }

    pub fn prev(&self) {
        self.report(self.spirc.prev());
    }

    pub fn seek(&self, ms: u32) {
        self.report(self.spirc.set_position_ms(ms));
    }

    pub fn set_volume(&self, v: f32) {
        let v = (v.clamp(0.0, 1.0) * u16::MAX as f32).round() as u16;
        self.volume.store(v, Ordering::Relaxed);
        // Direct mixer write so the slider works before this device is active; Spirc then syncs
        // Connect state, cache and event when it is.
        self.mixer.set_volume(v);
        self.report(self.spirc.set_volume(v));
    }

    pub fn set_shuffle(&self, on: bool) {
        self.shuffle.store(on, Ordering::Relaxed);
        self.report(self.spirc.shuffle(on));
    }

    pub fn set_repeat(&self, on: bool) {
        self.repeat.store(on, Ordering::Relaxed);
        self.report(self.spirc.repeat(on));
        if !on {
            self.report(self.spirc.repeat_track(false));
        }
    }

    /// Pauses, disconnects the Connect device and waits (max 2 s) for the Spirc task to end.
    pub async fn shutdown(&self) {
        self.closing.store(true, Ordering::Relaxed);
        let _ = self.spirc.shutdown();
        let task = self.spirc_task.lock().ok().and_then(|mut t| t.take());
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }

    fn report(&self, r: Result<(), librespot_core::Error>) {
        if let Err(e) = r {
            let _ = self.tx.send(Event::Error(e.to_string()));
        }
    }
}

pub struct Lyrics {
    pub synced: bool,
    pub lines: Vec<(u32 /*start ms*/, String)>,
}

impl Player {
    /// Exact lyrics pipeline matching yoshik.xyz / lyrics:
    /// 1. Spotify internal color-lyrics via librespot session
    /// 2. Clean title: remove (feat. ...), [feat. ...], (with ...), remasters, bonus tracks, suffixes
    /// 3. LRCMUX API (https://api.lrcmux.dev/get with User-Agent: yoshik.xyz/lyrics)
    /// 4. LRCLIB API (get + search fallback)
    /// 5. Retry with raw title if different
    pub async fn lyrics(
        &self,
        uri: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_ms: u32,
    ) -> Option<Lyrics> {
        // 1. Spotify internal lyrics (Hermes spclient)
        if let Some(l) = self.spotify_lyrics(uri).await {
            if l.synced && !l.lines.is_empty() {
                return Some(l);
            }
        }

        let cleaned = clean_title(title);

        // 2. LRCMUX with cleaned title (primary yoshik.xyz lyrics provider)
        if let Some(l) = lrcmux(&cleaned, artist, duration_ms).await {
            return Some(l);
        }

        // 3. LRCLIB with cleaned title
        if let Some(l) = lrclib(&cleaned, artist, album, duration_ms).await {
            if l.synced {
                return Some(l);
            }
        }

        // 4. Retry with raw title if different
        if cleaned != title {
            if let Some(l) = lrcmux(title, artist, duration_ms).await {
                return Some(l);
            }
            if let Some(l) = lrclib(title, artist, album, duration_ms).await {
                return Some(l);
            }
        }

        // Fallback: unsynced Spotify lyrics if any
        self.spotify_lyrics(uri).await
    }

    async fn spotify_lyrics(&self, uri: &str) -> Option<Lyrics> {
        let SpotifyUri::Track { id } = SpotifyUri::from_uri(uri).ok()? else {
            return None;
        };
        let body = tokio::time::timeout(
            Duration::from_secs(10),
            self.session.spclient().get_lyrics(&id),
        )
        .await
        .ok()?
        .ok()?;
        parse_spotify_lyrics(&body)
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("yoshik.xyz/lyrics")
            .build()
            .unwrap_or_default()
    })
}

/// Clean track titles matching cleanTitle() in yoshik.xyz/lyrics/api/lyrics.js
fn clean_title(raw: &str) -> String {
    let mut s = raw.to_string();
    loop {
        let prev = s.clone();
        // Strip trailing brackets containing feat/ft/with/bonus/remaster
        if let Some(start) = s.rfind(|c| c == '(' || c == '[') {
            let close = if s.as_bytes()[start] == b'(' { ')' } else { ']' };
            if let Some(end) = s[start..].find(close) {
                let inner = s[start + 1..start + end].trim().to_lowercase();
                if inner.starts_with("feat")
                    || inner.starts_with("ft.")
                    || inner.starts_with("ft ")
                    || inner.starts_with("featuring")
                    || inner.starts_with("with ")
                    || inner.contains("remaster")
                    || inner.contains("bonus")
                {
                    s = format!("{}{}", s[..start].trim_end(), &s[start + end + 1..]);
                }
            }
        }
        // Strip trailing "- bonus", "- remaster...", "- radio edit", "- single version", etc.
        if let Some(idx) = s.rfind(" - ") {
            let suffix = s[idx + 3..].trim().to_lowercase();
            if suffix.contains("remaster")
                || suffix.starts_with("bonus")
                || suffix.starts_with("radio edit")
                || suffix.starts_with("single version")
                || suffix.starts_with("single edit")
                || suffix.starts_with("explicit")
            {
                s.truncate(idx);
            }
        }
        s = s.trim().to_string();
        if s == prev {
            break;
        }
    }
    if s.is_empty() {
        raw.to_string()
    } else {
        s
    }
}

/// Query LRCMUX (https://api.lrcmux.dev/get) - the exact provider from yoshik.xyz/lyrics
async fn lrcmux(title: &str, artist: &str, duration_ms: u32) -> Option<Lyrics> {
    let primary_artist = artist.split(',').next().unwrap_or(artist).trim();
    let secs = (duration_ms as f64 / 1000.0).round() as u64;
    let secs_s = secs.to_string();
    let r = http()
        .get("https://api.lrcmux.dev/get")
        .header("User-Agent", "yoshik.xyz/lyrics")
        .query(&[
            ("artist", primary_artist),
            ("title", title),
            ("duration", secs_s.as_str()),
            ("level", "line"),
            ("sources", "!kugou"),
        ])
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let body = r.text().await.ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let lines_arr = v.get("lines")?.as_array()?;
    if lines_arr.is_empty() {
        return None;
    }
    let mut lines = Vec::with_capacity(lines_arr.len());
    for item in lines_arr {
        let text = item.get("text").and_then(Value::as_str).unwrap_or("").trim();
        let start = item.get("start").and_then(Value::as_u64).unwrap_or(0) as u32;
        push_line(&mut lines, start, text);
    }
    (!lines.is_empty()).then_some(Lyrics {
        synced: true,
        lines,
    })
}

async fn lrclib(title: &str, artist: &str, album: &str, duration_ms: u32) -> Option<Lyrics> {
    let primary_artist = artist.split(',').next().unwrap_or(artist).trim();
    let secs = (duration_ms as f64 / 1000.0).round();
    let secs_s = (secs as u64).to_string();
    let r = http()
        .get("https://lrclib.net/api/get")
        .header("User-Agent", "yoshik.xyz/lyrics")
        .query(&[
            ("track_name", title),
            ("artist_name", primary_artist),
            ("album_name", album),
            ("duration", secs_s.as_str()),
        ])
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    if r.status().is_success() {
        if let Some(res) = lrclib_item(&serde_json::from_str(&r.text().await.ok()?).ok()?) {
            return Some(res);
        }
    }
    let search_q = format!("{title} {primary_artist}");
    let r = http()
        .get("https://lrclib.net/api/search")
        .header("User-Agent", "yoshik.xyz/lyrics")
        .query(&[("q", search_q.as_str())])
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let v: Value = serde_json::from_str(&r.text().await.ok()?).ok()?;
    v.as_array()?
        .iter()
        .filter_map(|i| {
            let diff = (i.get("duration")?.as_f64()? - secs).abs();
            (diff <= 4.0).then_some(())?;
            lrclib_item(i).map(|l| (l, diff))
        })
        .min_by(|a, b| {
            (!a.0.synced, a.1)
                .partial_cmp(&(!b.0.synced, b.1))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(l, _)| l)
}

/// One LRCLIB track object: synced lyrics if present, else plain; instrumental gives None.
fn lrclib_item(v: &Value) -> Option<Lyrics> {
    if v.get("instrumental").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if let Some(lrc) = v.get("syncedLyrics").and_then(Value::as_str) {
        let lines = parse_lrc(lrc);
        if !lines.is_empty() {
            return Some(Lyrics { synced: true, lines });
        }
    }
    let mut lines = Vec::new();
    for l in v.get("plainLyrics")?.as_str()?.lines() {
        push_line(&mut lines, 0, l);
    }
    (!lines.is_empty()).then_some(Lyrics {
        synced: false,
        lines,
    })
}

/// `[mm:ss.xx] text`, several timestamps per line allowed; non-time tags like `[ar:..]` are skipped.
fn parse_lrc(lrc: &str) -> Vec<(u32, String)> {
    let mut lines = Vec::new();
    for raw in lrc.lines() {
        let mut rest = raw.trim_start();
        let mut times = Vec::new();
        while let Some(tag) = rest.strip_prefix('[') {
            let Some((inner, after)) = tag.split_once(']') else {
                break;
            };
            let Some(t) = lrc_time(inner) else { break };
            times.push(t);
            rest = after;
        }
        for t in times {
            push_line(&mut lines, t, rest);
        }
    }
    lines.sort_by_key(|l| l.0);
    lines
}

fn lrc_time(s: &str) -> Option<u32> {
    let (m, rest) = s.split_once(':')?;
    let (sec, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let frac: String = frac.chars().take(3).collect();
    let ms = if frac.is_empty() {
        0
    } else {
        frac.parse::<u32>().ok()? * 10u32.pow(3 - frac.len() as u32)
    };
    Some(m.parse::<u32>().ok()? * 60_000 + sec.parse::<u32>().ok()? * 1000 + ms)
}

/// `{"lyrics":{"syncType":"LINE_SYNCED"|"UNSYNCED","lines":[{"startTimeMs":"1230","words":".."}]}}`
fn parse_spotify_lyrics(body: &[u8]) -> Option<Lyrics> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let lyrics = v.get("lyrics")?;
    let synced = lyrics.get("syncType").and_then(Value::as_str) == Some("LINE_SYNCED");
    let mut lines = Vec::new();
    for l in lyrics.get("lines")?.as_array()? {
        let start = match l.get("startTimeMs") {
            Some(Value::String(s)) if synced => s.parse().unwrap_or(0),
            Some(Value::Number(n)) if synced => n.as_u64().unwrap_or(0) as u32,
            _ => 0,
        };
        push_line(&mut lines, start, l.get("words").and_then(Value::as_str).unwrap_or(""));
    }
    (!lines.is_empty()).then_some(Lyrics { synced, lines })
}

fn push_line(lines: &mut Vec<(u32, String)>, start_ms: u32, text: &str) {
    let text = unescape(text.trim());
    if text.is_empty() {
        if let Some(last) = lines.last() {
            if !last.1.is_empty() {
                lines.push((start_ms, String::new()));
            }
        }
        return;
    }
    if text == "♪" {
        return;
    }
    for sub in text.split('\n') {
        let sub = sub.trim();
        if !sub.is_empty() && sub != "♪" {
            lines.push((start_ms, sub.to_string()));
        }
    }
}

async fn map_events(
    mut rx: librespot_playback::player::PlayerEventChannel,
    tx: UnboundedSender<Event>,
    shuffle: Arc<AtomicBool>,
    repeat: Arc<AtomicBool>,
    volume: Arc<AtomicU16>,
) {
    let mut request_id: Option<u64> = None;
    let mut playing = false;
    while let Some(ev) = rx.recv().await {
        // Same rule as Spirc: drop events that belong to a previous play request.
        match ev {
            PlayerEvent::PlayRequestIdChanged { play_request_id } => {
                request_id = Some(play_request_id);
                continue;
            }
            _ => {
                if let Some(id) = ev.get_play_request_id() {
                    if Some(id) != request_id {
                        continue;
                    }
                }
            }
        }
        let out = match ev {
            PlayerEvent::TrackChanged { audio_item } => {
                let (artist, album) = match &audio_item.unique_fields {
                    UniqueFields::Track { artists, album, .. } => {
                        (join_artists(artists), album.clone())
                    }
                    UniqueFields::Local { artists, album, .. } => (
                        artists.clone().unwrap_or_default(),
                        album.clone().unwrap_or_default(),
                    ),
                    UniqueFields::Episode { show_name, .. } => (show_name.clone(), String::new()),
                };
                // Cover closest to 640 px; covers come sorted largest first, so first() is the fallback.
                let art_url = audio_item
                    .covers
                    .iter()
                    .filter(|c| c.width > 0)
                    .min_by_key(|c| (c.width - 640).abs())
                    .or(audio_item.covers.first())
                    .map(|c| c.url.clone());
                Some(Event::Track {
                    uri: audio_item.uri.clone(),
                    title: unescape(&audio_item.name),
                    artist: unescape(&artist),
                    album: unescape(&album),
                    art_url,
                    duration_ms: audio_item.duration_ms,
                })
            }
            PlayerEvent::Playing { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. } => {
                playing = true;
                Some(Event::Playing { position_ms })
            }
            PlayerEvent::Paused { position_ms, .. } => {
                playing = false;
                Some(Event::Paused { position_ms })
            }
            PlayerEvent::Seeked { position_ms, .. } => Some(if playing {
                Event::Playing { position_ms }
            } else {
                Event::Paused { position_ms }
            }),
            PlayerEvent::Stopped { .. } => {
                playing = false;
                Some(Event::Stopped)
            }
            PlayerEvent::Unavailable { .. } => Some(Event::Error("Track unavailable".into())),
            PlayerEvent::VolumeChanged { volume: v } => {
                volume.store(v, Ordering::Relaxed);
                Some(Event::Volume(v as f32 / u16::MAX as f32))
            }
            PlayerEvent::ShuffleChanged { shuffle: on } => {
                shuffle.store(on, Ordering::Relaxed);
                Some(Event::Shuffle(on))
            }
            PlayerEvent::RepeatChanged { context, track } => {
                repeat.store(context, Ordering::Relaxed);
                Some(Event::Repeat(context || track))
            }
            _ => None,
        };
        if let Some(out) = out {
            if tx.send(out).is_err() {
                break;
            }
        }
    }
}

fn join_artists(artists: &[librespot_metadata::artist::ArtistWithRole]) -> String {
    let names = |all: bool| {
        artists
            .iter()
            .filter(|a| {
                all || matches!(
                    a.role,
                    ArtistRole::ARTIST_ROLE_MAIN_ARTIST | ArtistRole::ARTIST_ROLE_FEATURED_ARTIST
                )
            })
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let main = names(false);
    if main.is_empty() { names(true) } else { main }
}

/// Spotify metadata sometimes carries HTML entities ("Future &amp; Young Thug"); decode the common
/// named ones and numeric `&#NNN;` / `&#xHH;`, leave anything else untouched.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest.find(';').filter(|&end| end <= 10).and_then(|end| {
            let ent = &rest[1..end];
            let ch = match ent {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                _ => {
                    let num = ent.strip_prefix('#')?;
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((ch, end + 1))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Stable per-machine device id (40 hex chars, like librespot's own) so Spotify sees one
/// "Cadence" device across launches instead of a new random one each time.
fn device_id() -> String {
    let seed = format!(
        "cadence:{}",
        std::env::var("COMPUTERNAME").unwrap_or_default()
    );
    let mut id = String::new();
    for salt in 1u64..=3 {
        let mut h = 0xcbf29ce484222325u64 ^ salt.wrapping_mul(0x9e3779b97f4a7c15);
        for b in seed.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        id.push_str(&format!("{h:016x}"));
    }
    id.truncate(40);
    id
}

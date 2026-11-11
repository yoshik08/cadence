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
    config::PlayerConfig,
    mixer::{self, Mixer, MixerConfig},
    player::{Player as LsPlayer, PlayerEvent},
};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering},
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
        explicit: bool,
    },
    Playing { position_ms: u32 },
    Paused { position_ms: u32 },
    Stopped,
    Volume(f32 /*0..1*/),
    Shuffle(bool),
    /// 0 off, 1 all (context), 2 one (track).
    Repeat(u8),
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
    repeat: Arc<AtomicU8>,
    volume: Arc<AtomicU16>,
    /// The context the current playback was started from (a track uri for a single track).
    context: Mutex<String>,
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
        // Settings read once at startup (Settings marks them "applies after a restart").
        let proxy = crate::prefs::get("proxy").as_str().and_then(|p| reqwest::Url::parse(p).ok());
        let session_config = SessionConfig {
            device_id: device_id(),
            autoplay: Some(crate::prefs::bool_or("autoplay", true)),
            proxy,
            ..Default::default()
        };
        let session = Session::new(session_config, Some(cache));

        let mixer = mixer::find(None).ok_or("no mixer")?(MixerConfig::default())
            .map_err(|e| e.to_string())?;
        let bitrate = match crate::prefs::get("quality").as_str() {
            Some("low") => librespot_playback::config::Bitrate::Bitrate96,
            Some("normal") => librespot_playback::config::Bitrate::Bitrate160,
            _ => librespot_playback::config::Bitrate::Bitrate320,
        };
        // Spotify's levels: Quiet ≈ -19 LUFS, Normal ≈ -14, Loud ≈ -11 (librespot's default is Normal).
        let pregain = match crate::prefs::get("loudness").as_str() {
            Some("quiet") => -5.0,
            Some("loud") => 3.0,
            _ => 0.0,
        };
        let player_config = PlayerConfig {
            bitrate,
            gapless: crate::prefs::bool_or("gapless", true),
            normalisation: crate::prefs::bool_or("normalize", false),
            normalisation_pregain_db: pregain,
            ..Default::default()
        };
        let player = LsPlayer::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            // Cadence's WASAPI sink follows the Windows default device (audio.rs).
            // CADENCE_RODIO=1 falls back to librespot's rodio backend (pinned to the launch device).
            move || match librespot_playback::audio_backend::find(None) {
                Some(rodio) if std::env::var("CADENCE_RODIO").is_ok_and(|v| v == "1") => {
                    rodio(None, librespot_playback::config::AudioFormat::F32)
                }
                _ => crate::audio::sink(),
            },
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

        // Remembered across launches; Spirc gets them with every load.
        let shuffle = Arc::new(AtomicBool::new(crate::prefs::bool_or("shuffle", false)));
        let repeat = Arc::new(AtomicU8::new(crate::prefs::u64_or("repeat", 0).min(2) as u8));
        let _ = tx.send(Event::Shuffle(shuffle.load(Ordering::Relaxed)));
        let _ = tx.send(Event::Repeat(repeat.load(Ordering::Relaxed)));
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
            context: Mutex::new(String::new()),
            closing,
            spirc_task: Mutex::new(Some(spirc_task)),
            _event_task: event_task,
        };
        Ok((player, rx))
    }

    /// Playlist/album/collection context, or a bare `spotify:track:..` uri (use index 0).
    pub fn play(&self, context_uri: &str, index: usize) {
        self.load(context_uri, PlayingTrack::Index(index as u32), 0, true);
    }

    /// Starts `context_uri` with shuffle on from a random one of its `count` tracks.
    pub fn play_shuffled(&self, context_uri: &str, count: usize) {
        self.set_shuffle(true);
        let start = if count > 1 { random_below(count) } else { 0 };
        self.load(context_uri, PlayingTrack::Index(start as u32), 0, true);
    }

    pub fn context(&self) -> String {
        self.context.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Loads a context at a track and position (session restore passes `start_playing: false`).
    pub fn load(&self, context_uri: &str, track: PlayingTrack, seek_ms: u32, start_playing: bool) {
        if let Ok(mut c) = self.context.lock() {
            *c = context_uri.to_string();
        }
        let repeat = self.repeat.load(Ordering::Relaxed);
        let options = LoadRequestOptions {
            start_playing,
            seek_to: seek_ms,
            context_options: Some(LoadContextOptions::Options(Options {
                shuffle: self.shuffle.load(Ordering::Relaxed),
                repeat: repeat >= 1,
                repeat_track: repeat == 2,
            })),
            playing_track: Some(track),
        };
        let single = ["spotify:track:", "spotify:episode:", "spotify:local:"].iter().any(|p| context_uri.starts_with(p));
        let request = if single {
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

    /// Artist name and portrait from librespot metadata (fallback without the Web API).
    pub async fn artist_meta(&self, artist_uri: &str) -> Option<(String, String)> {
        use librespot_metadata::{Artist, Metadata};
        let id = SpotifyUri::from_uri(artist_uri).ok()?;
        let a = tokio::time::timeout(Duration::from_secs(8), Artist::get(&self.session, &id)).await.ok()?.ok()?;
        let img = a.portraits.0.iter().chain(a.portrait_group.0.iter()).max_by_key(|i| i.width).map(|i| format!("https://i.scdn.co/image/{}", i.id)).unwrap_or_default();
        Some((a.name.clone(), img))
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn pause(&self) {
        self.report(self.spirc.pause());
    }

    /// Plays an explicit list of uris (sorted / filtered lists, local files, popular tracks).
    /// `shuffle`: Some(false) when the list is already in Cadence's own shuffle order.
    pub fn play_list(&self, uris: &[String], index: usize, shuffle: Option<bool>) {
        if uris.is_empty() {
            return;
        }
        let uris: Vec<String> = uris.iter().take(1000).cloned().collect();
        let index = index.min(uris.len() - 1);
        let repeat = self.repeat.load(Ordering::Relaxed);
        let options = LoadRequestOptions {
            start_playing: true,
            seek_to: 0,
            context_options: Some(LoadContextOptions::Options(Options {
                shuffle: shuffle.unwrap_or(self.shuffle.load(Ordering::Relaxed)),
                repeat: repeat >= 1,
                repeat_track: repeat == 2,
            })),
            playing_track: Some(PlayingTrack::Index(index as u32)),
        };
        self.report(self.spirc.activate());
        self.report(self.spirc.set_volume(self.volume.load(Ordering::Relaxed)));
        self.report(self.spirc.load(LoadRequest::from_tracks(uris, options)));
    }

    /// Popular tracks from librespot's artist metadata (internal API: may fail at any time).
    pub async fn artist_top_tracks(&self, artist_uri: &str) -> Option<Vec<String>> {
        use librespot_metadata::{Artist, Metadata};
        let id = SpotifyUri::from_uri(artist_uri).ok()?;
        let a = tokio::time::timeout(Duration::from_secs(8), Artist::get(&self.session, &id)).await.ok()?.ok()?;
        let country = self.session.country();
        let top = a.top_tracks.0.iter().find(|t| t.country == country).or_else(|| a.top_tracks.0.first())?;
        let uris: Vec<String> = top.tracks.0.iter().filter_map(|t| t.to_uri().ok()).collect();
        (!uris.is_empty()).then_some(uris)
    }

    pub fn seek(&self, ms: u32) {
        crate::dsp::flush();
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
        crate::prefs::set("shuffle", on.into());
        self.report(self.spirc.shuffle(on));
    }

    /// 0 off, 1 all, 2 one.
    pub fn set_repeat(&self, mode: u8) {
        let mode = mode.min(2);
        self.repeat.store(mode, Ordering::Relaxed);
        crate::prefs::set("repeat", mode.into());
        self.report(self.spirc.repeat(mode >= 1));
        self.report(self.spirc.repeat_track(mode == 2));
    }

    // Queue (librespot-connect patch, vendor/librespot-connect/CADENCE_PATCH.md). These only
    // apply while Cadence is the active Connect device.

    pub fn queue_insert(&self, uri: &str, position: Option<usize>) {
        self.report(self.spirc.queue_insert(uri.to_string(), position));
    }

    pub fn queue_remove(&self, uid: &str) {
        self.report(self.spirc.queue_remove(uid.to_string()));
    }

    pub fn queue_move(&self, uid: &str, position: usize) {
        self.report(self.spirc.queue_move(uid.to_string(), position));
    }

    pub fn queue_clear(&self) {
        self.report(self.spirc.queue_clear());
    }

    pub async fn queue_snapshot(&self) -> Option<librespot_connect::QueueSnapshot> {
        tokio::time::timeout(Duration::from_secs(3), self.spirc.queue_snapshot()).await.ok().flatten()
    }

    /// Title, artists, album, duration and cover for a track or episode (librespot metadata).
    pub async fn item_meta(&self, uri: &str) -> Option<ItemMeta> {
        use librespot_metadata::{Episode, Metadata, Track};
        let id = SpotifyUri::from_uri(uri).ok()?;
        let cover = |imgs: &[librespot_metadata::image::Image]| {
            imgs.iter()
                .min_by_key(|i| (i.width - 300).abs())
                .map(|i| format!("https://i.scdn.co/image/{}", i.id))
        };
        let timeout = Duration::from_secs(8);
        match id {
            SpotifyUri::Track { .. } => {
                let t = tokio::time::timeout(timeout, Track::get(&self.session, &id)).await.ok()?.ok()?;
                Some(ItemMeta {
                    title: unescape(&t.name),
                    artist: t.artists.0.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "),
                    album: t.album.name.clone(),
                    album_uri: t.album.id.to_uri().unwrap_or_default(),
                    artist_uri: t.artists.0.first().and_then(|a| a.id.to_uri().ok()).unwrap_or_default(),
                    duration_ms: t.duration.max(0) as u32,
                    art_url: cover(&t.album.covers.0),
                    explicit: t.is_explicit,
                })
            }
            SpotifyUri::Episode { .. } => {
                let e = tokio::time::timeout(timeout, Episode::get(&self.session, &id)).await.ok()?.ok()?;
                Some(ItemMeta {
                    title: unescape(&e.name),
                    artist: e.show_name.clone(),
                    duration_ms: e.duration.max(0) as u32,
                    art_url: cover(&e.covers.0),
                    explicit: e.is_explicit,
                    ..Default::default()
                })
            }
            _ => None,
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

#[derive(Clone, Debug, Default)]
pub struct ItemMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_uri: String,
    pub artist_uri: String,
    pub duration_ms: u32,
    pub art_url: Option<String>,
    pub explicit: bool,
}

/// Uniform in 0..n (n > 0), from the OS RNG.
pub fn random_below(n: usize) -> usize {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    (u64::from_le_bytes(b) % n as u64) as usize
}

pub struct LyricLine {
    pub start_ms: u32,
    pub text: String,
    /// Per-word start times (ms), aligned 1:1 with `text` split on whitespace; empty if unknown.
    pub words: Vec<u32>,
    /// Per-word end times (ms), same alignment as `words`.
    pub ends: Vec<u32>,
}

pub struct Lyrics {
    pub synced: bool,
    /// True when at least one line carries per-word timings.
    pub word_sync: bool,
    pub lines: Vec<LyricLine>,
}

impl Lyrics {
    /// Compact JSON for the disk cache.
    pub fn to_json(&self) -> String {
        let lines: Vec<Value> = self
            .lines
            .iter()
            .map(|l| serde_json::json!({ "t": l.start_ms, "x": l.text, "w": l.words, "e": l.ends }))
            .collect();
        serde_json::json!({ "synced": self.synced, "word": self.word_sync, "lines": lines }).to_string()
    }

    pub fn from_json(data: &[u8]) -> Option<Lyrics> {
        let v: Value = serde_json::from_slice(data).ok()?;
        let nums = |x: &Value| -> Vec<u32> {
            x.as_array().map_or(Vec::new(), |a| a.iter().filter_map(|n| n.as_u64().map(|n| n as u32)).collect())
        };
        let lines = v
            .get("lines")?
            .as_array()?
            .iter()
            .map(|l| LyricLine {
                start_ms: l["t"].as_u64().unwrap_or(0) as u32,
                text: l["x"].as_str().unwrap_or("").to_string(),
                words: nums(&l["w"]),
                ends: nums(&l["e"]),
            })
            .collect();
        Some(Lyrics { synced: v["synced"].as_bool()?, word_sync: v["word"].as_bool().unwrap_or(false), lines })
    }
}

impl Player {
    /// Lyrics pipeline ported from yoshik.xyz/api/lyrics.js (its line and word modes combined):
    /// 1. Spotify internal color-lyrics via the librespot session. The site has no equivalent;
    ///    it is kept because it is the most accurate source and is not sync logic.
    /// 2. Word race: LRCMUX word-level + NetEase YRC, queried in parallel; first result with
    ///    per-word timings wins.
    /// 3. Line race: LRCMUX -> LRCLIB -> NetEase LRC, queried in parallel; first valid wins,
    ///    LRCLIB plain lyrics last.
    /// 4. Retry with the raw title when the cleaned title differs.
    /// 5. Fallback: unsynced Spotify lyrics if any.
    /// No syncing anywhere: no calibrate(), no offset estimation, no confidence/MAD candidate
    /// selection. Provider timings are used exactly as returned — Cadence playback is local and
    /// the librespot position is exact, so there is nothing to correct for.
    pub async fn lyrics(
        &self,
        uri: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_ms: u32,
    ) -> Option<Lyrics> {
        // 1. Spotify internal lyrics (Hermes spclient). Kept for the unsynced fallback below
        // instead of being fetched a second time.
        let spotify = self.spotify_lyrics(uri).await;
        lyrics_pipeline(spotify, title, artist, album, duration_ms).await
    }

    /// The track's ISRC from Spotify metadata (Apple Music lookup by ISRC is exact).
    #[cfg_attr(not(feature = "karaoke"), allow(dead_code))]
    pub async fn isrc(&self, uri: &str) -> Option<String> {
        use librespot_metadata::{Metadata, Track};
        let uri = SpotifyUri::from_uri(uri).ok()?;
        let track = tokio::time::timeout(Duration::from_secs(8), Track::get(&self.session, &uri))
            .await
            .ok()?
            .ok()?;
        track
            .external_ids
            .iter()
            .find(|e| e.external_type.eq_ignore_ascii_case("isrc"))
            .map(|e| e.id.clone())
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

/// Steps 2-5 of the pipeline (everything after Spotify's own lyrics, which the caller passes in).
pub async fn lyrics_pipeline(
    spotify: Option<Lyrics>,
    title: &str,
    artist: &str,
    album: &str,
    duration_ms: u32,
) -> Option<Lyrics> {
    if spotify.as_ref().is_some_and(|l| l.synced && !l.lines.is_empty()) {
        return spotify;
    }
    let cleaned = clean_title(title);
    // 2. Word race, then 3. line race — cleaned title first, raw title as retry.
    let mut query = cleaned.as_str();
    loop {
        if let Some(l) = fetch_word(query, artist, duration_ms).await {
            return Some(l);
        }
        if let Some(l) = fetch_line(query, artist, album, duration_ms).await {
            return Some(l);
        }
        if query == title {
            break;
        }
        query = title;
    }

    // Fallback: unsynced Spotify lyrics if any
    spotify
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

/// Word race: LRCMUX word-level + NetEase YRC in parallel; first with per-word timings wins.
/// No calibration: provider timings are used exactly as returned.
async fn fetch_word(title: &str, artist: &str, duration_ms: u32) -> Option<Lyrics> {
    let (a, b) = tokio::join!(
        lrcmux(title, artist, duration_ms, true),
        netease(title, artist, duration_ms)
    );
    if let Some(l) = a.filter(|l| l.word_sync) {
        return Some(l);
    }
    if let Some(l) = b.filter(|l| l.word_sync) {
        return Some(l);
    }
    None
}

/// Line race: LRCMUX -> LRCLIB -> NetEase LRC in parallel; first valid wins, LRCLIB plain last.
async fn fetch_line(
    title: &str,
    artist: &str,
    album: &str,
    duration_ms: u32,
) -> Option<Lyrics> {
    let (a, b, c) = tokio::join!(
        lrcmux(title, artist, duration_ms, false),
        lrclib(title, artist, album, duration_ms),
        netease(title, artist, duration_ms)
    );
    if let Some(l) = a {
        return Some(l);
    }
    let plain = match b {
        Some(l) if l.synced => return Some(l),
        other => other,
    };
    if let Some(l) = c {
        return Some(l);
    }
    plain
}

/// Last line must start within the track (+2 s) or the result is a wrong-track match.
fn length_ok(lines: &[LyricLine], duration_ms: u32) -> bool {
    duration_ms == 0
        || lines
            .last()
            .map(|l| l.start_ms)
            .unwrap_or(0)
            <= duration_ms + 2000
}

/// Port of norm() in yoshik.xyz/api/lyrics.js: lowercase, strip diacritics, dashes to spaces,
/// drop punctuation, collapse whitespace. Used only for match verification, never for timing.
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ('\u{0300}'..='\u{036f}').contains(&ch) {
            continue;
        }
        let base = strip_diacritic(ch);
        let lower = base.to_lowercase().next().unwrap_or(base);
        if lower == '-' || lower == '_' || lower == '–' || lower == '—' || lower == '/' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else if lower.is_alphanumeric() || lower == ' ' {
            if lower == ' ' {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(lower);
                prev_space = false;
            }
        }
    }
    out.trim_end().to_string()
}

/// Precomposed Latin-1 accented letters to their base letter (NFD decomposition, std-only).
fn strip_diacritic(ch: char) -> char {
    match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        'ñ' => 'n',
        'ç' => 'c',
        'ø' => 'o',
        _ => ch,
    }
}

/// Port of firstSigWord() in yoshik.xyz/api/lyrics.js: first word that is not the/a/an.
fn first_sig_word(s: &str) -> String {
    let normed = norm(s);
    for w in normed.split_whitespace() {
        if !matches!(w, "the" | "a" | "an") {
            return w.to_string();
        }
    }
    normed.split_whitespace().next().map(|w| w.to_string()).unwrap_or_default()
}

/// Port of cleanTitle() in yoshik.xyz/api/lyrics.js.
fn clean_title(raw: &str) -> String {
    let mut s = raw.to_string();
    loop {
        let prev = s.clone();
        // Strip any bracket group whose contents start with feat/ft/featuring/with,
        // or mention bonus/remaster.
        s = strip_bracket_group(&s);
        // Strip a trailing " - <suffix>" where the suffix is a bonus/remaster/radio/single/explicit tag.
        if let Some(idx) = s.rfind(" - ") {
            let suffix = s[idx + 3..].trim().to_lowercase();
            let mut parts = suffix.splitn(2, ' ');
            let year_remaster = matches!(parts.next(), Some(y) if y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()))
                && parts.next().map(|r| r.contains("remaster")).unwrap_or(false);
            let is_tag = suffix.starts_with("bonus")
                || suffix.contains("remaster")
                || suffix.starts_with("radio edit")
                || suffix.starts_with("radio version")
                || suffix.starts_with("single version")
                || suffix.starts_with("single edit")
                || suffix.starts_with("explicit")
                || year_remaster;
            if is_tag {
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

/// Remove the first bracket group (parens or square) whose inner text starts with
/// feat/ft/featuring/with or mentions bonus/remaster; returns the string unchanged if none.
fn strip_bracket_group(s: &str) -> String {
    for (i, ch) in s.char_indices() {
        let close = match ch {
            '(' => ')',
            '[' => ']',
            _ => continue,
        };
        let rest = &s[i + ch.len_utf8()..];
        if let Some(rel) = rest.find(close) {
            let inner = rest[..rel].trim().to_lowercase();
            let hit = inner.starts_with("feat")
                || inner.starts_with("ft.")
                || inner.starts_with("ft ")
                || inner.starts_with("featuring")
                || inner.starts_with("with ")
                || inner.contains("remaster")
                || inner.starts_with("bonus");
            if hit {
                let mut out = String::with_capacity(s.len());
                out.push_str(s[..i].trim_end());
                out.push_str(&rest[rel + close.len_utf8()..]);
                return out;
            }
        }
    }
    s.to_string()
}

/// Query LRCMUX (https://api.lrcmux.dev/get) - the provider from yoshik.xyz/lyrics.
/// `word` requests word-level timings. Applies the site's Fix B (match verification:
/// artist fuzzy match + first significant word) and Fix C (length sanity check).
async fn lrcmux(title: &str, artist: &str, duration_ms: u32, word: bool) -> Option<Lyrics> {
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
            ("level", if word { "word" } else { "line" }),
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
    let track = v.get("track")?;
    let lines_arr = v.get("lines")?.as_array()?;
    if lines_arr.is_empty() {
        return None;
    }

    // Fix B: verify match.
    let req_artist = norm(primary_artist);
    let track_artist = norm(track.get("artist").and_then(Value::as_str).unwrap_or(""));
    if req_artist.is_empty() || track_artist.is_empty() {
        return None;
    }
    if !track_artist.contains(&req_artist) && !req_artist.contains(&track_artist) {
        return None;
    }
    let sig_req = first_sig_word(title);
    let sig_track = first_sig_word(track.get("title").and_then(Value::as_str).unwrap_or(""));
    if sig_req.is_empty() || sig_track.is_empty() || sig_req != sig_track {
        return None;
    }

    let mut lines = Vec::with_capacity(lines_arr.len());
    let mut word_sync = false;
    for item in lines_arr {
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let start = item.get("start").and_then(Value::as_u64).unwrap_or(0) as u32;
        let (words, ends) = match item.get("words").and_then(Value::as_array) {
            Some(arr) => {
                let provider_words: Vec<(String, u32, u32)> = arr
                    .iter()
                    .filter_map(|w| {
                        Some((
                            w.get("text")?.as_str()?.to_string(),
                            w.get("start")?.as_u64()? as u32,
                            w.get("end")?.as_u64()? as u32,
                        ))
                    })
                    .collect();
                align_word_count(text, &provider_words)
            }
            None => (Vec::new(), Vec::new()),
        };
        if !words.is_empty() {
            word_sync = true;
        }
        push_word_line(&mut lines, start, text, words, ends);
    }
    if lines.is_empty() {
        return None;
    }
    // Fix C: length sanity check.
    if !length_ok(&lines, duration_ms) {
        return None;
    }
    Some(Lyrics {
        synced: true,
        word_sync,
        lines,
    })
}

const NETEASE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Port of fetchNetease() in yoshik.xyz/api/lyrics.js: search, best match by fuzzy
/// title/artist + closest duration (<= 3000 ms), then YRC word lyrics, else LRC lines.
async fn netease(title: &str, artist: &str, duration_ms: u32) -> Option<Lyrics> {
    let primary_artist = artist.split(',').next().unwrap_or(artist).trim();
    let clean_t = title.replace(['\u{2019}', '\u{2018}'], "'");
    let q = format!("{} {}", clean_t, primary_artist);
    let sr = http()
        .get("https://music.163.com/api/cloudsearch/pc")
        .header("User-Agent", NETEASE_UA)
        .header("Referer", "https://music.163.com")
        .query(&[("s", q.as_str()), ("type", "1"), ("limit", "10")])
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    if !sr.status().is_success() {
        return None;
    }
    let sd: Value = serde_json::from_str(&sr.text().await.ok()?).ok()?;
    let songs = sd.get("result")?.get("songs")?.as_array()?;
    if songs.is_empty() {
        return None;
    }

    let title_n = norm(&clean_t);
    let artist_n = norm(primary_artist);
    let mut best_id: Option<u64> = None;
    let mut best_score = u64::MAX;
    for s in songs {
        let s_title = norm(s.get("name").and_then(Value::as_str).unwrap_or(""));
        let s_artist = norm(
            s.get("ar")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        if s_title.is_empty() || s_artist.is_empty() {
            continue;
        }
        // Title and artist must fuzzy match.
        if !s_title.contains(&title_n) && !title_n.contains(&s_title) {
            continue;
        }
        if !s_artist.contains(&artist_n) && !artist_n.contains(&s_artist) {
            continue;
        }
        let s_dur = s
            .get("dt")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| s.get("duration").and_then(Value::as_u64).unwrap_or(0));
        let score = if duration_ms > 0 {
            (s_dur as i64 - duration_ms as i64).unsigned_abs()
        } else {
            0
        };
        if score < best_score {
            best_score = score;
            best_id = s.get("id").and_then(Value::as_u64);
        }
    }
    let best_id = best_id?;
    if duration_ms > 0 && best_score > 3000 {
        return None;
    }

    let lr = http()
        .get("https://music.163.com/api/song/lyric")
        .header("User-Agent", NETEASE_UA)
        .header("Referer", "https://music.163.com")
        .query(&[
            ("id", best_id.to_string().as_str()),
            ("lv", "-1"),
            ("kv", "-1"),
            ("tv", "-1"),
            ("yv", "-1"),
        ])
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    if !lr.status().is_success() {
        return None;
    }
    let ld: Value = serde_json::from_str(&lr.text().await.ok()?).ok()?;

    // Word-level: YRC.
    if let Some(yrc) = ld
        .get("yrc")
        .and_then(|y| y.get("lyric"))
        .and_then(Value::as_str)
    {
        let lines = parse_yrc(yrc);
        if !lines.is_empty() && length_ok(&lines, duration_ms) {
            let word_sync = lines.iter().any(|l| !l.words.is_empty());
            return Some(Lyrics {
                synced: true,
                word_sync,
                lines,
            });
        }
    }
    // Line-level fallback: LRC.
    if let Some(lrc) = ld
        .get("lrc")
        .and_then(|y| y.get("lyric"))
        .and_then(Value::as_str)
    {
        let lines = parse_lrc(lrc);
        if !lines.is_empty() && length_ok(&lines, duration_ms) {
            return Some(Lyrics {
                synced: true,
                word_sync: false,
                lines,
            });
        }
    }
    None
}

/// Port of alignWordCount() in yoshik.xyz/api/_karaoke-calib.js.
/// Parsing helper only: it aligns provider word timings to the line's words; it never shifts times.
fn align_word_count(
    text: &str,
    provider_words: &[(String, u32, u32)],
) -> (Vec<u32>, Vec<u32>) {
    let empty = (Vec::new(), Vec::new());
    let text_words: Vec<&str> = text.split_whitespace().collect();
    if text_words.is_empty() || provider_words.is_empty() {
        return empty;
    }
    // Drop empty / punctuation-only tokens.
    let filtered: Vec<(String, u32, u32)> = provider_words
        .iter()
        .filter(|(t, _, _)| !t.is_empty() && t.chars().any(|c| c.is_alphanumeric()))
        .cloned()
        .collect();
    if filtered.is_empty() {
        return empty;
    }
    if filtered.len() == text_words.len() {
        return (
            filtered.iter().map(|w| w.1).collect(),
            filtered.iter().map(|w| w.2).collect(),
        );
    }
    // Try merging: when consecutive tokens should be part of the same word
    // (e.g. "don" + "'t" -> "don't"), merge into the previous one.
    let mut merged: Vec<(String, u32, u32)> = vec![filtered[0].clone()];
    for cur in &filtered[1..] {
        let target_idx = merged.len() - 1;
        let merged_text = format!("{}{}", merged[target_idx].0, cur.0);
        if target_idx < text_words.len()
            && norm(&merged_text) == norm(text_words[target_idx])
        {
            merged[target_idx].0 = merged_text;
            merged[target_idx].2 = cur.2;
        } else {
            merged.push(cur.clone());
        }
    }
    if merged.len() == text_words.len() {
        return (
            merged.iter().map(|w| w.1).collect(),
            merged.iter().map(|w| w.2).collect(),
        );
    }
    empty
}

/// Port of yrcEnds() in yoshik.xyz/api/_karaoke-calib.js: end = start + dur per chunk.
fn yrc_ends(chunks: &[(u32, u32, String)]) -> Vec<u32> {
    chunks.iter().map(|c| c.0 + c.1).collect()
}

fn is_credit_line(text: &str) -> bool {
    if text.starts_with('{') || text.starts_with('（') {
        return true;
    }
    const PREFIXES: &[&str] = &[
        "制作", "作词", "作曲", "编曲", "混音", "母带", "录音", "和声", "出品", "监制",
    ];
    if PREFIXES.iter().any(|p| text.starts_with(p)) {
        return true;
    }
    let lower = text.to_lowercase();
    ["produced", "written", "composed", "arranged"]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// Port of parseYRC() in yoshik.xyz/api/lyrics.js.
/// Lines: `[lineStart,lineDur](chunkStart,chunkDur,0)text...`
/// Syllable chunks without spaces merge into one word; uses the first chunk's start.
fn parse_yrc(yrc: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in yrc.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Some(rest) = raw.strip_prefix('[') else {
            continue;
        };
        let Some((hdr, after_hdr)) = rest.split_once(']') else {
            continue;
        };
        let mut hp = hdr.split(',');
        let line_start: u32 = match hp.next().and_then(|x| x.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        // Chunks: (start,dur,0)text
        let mut chunks: Vec<(u32, u32, String)> = Vec::new();
        let mut s = after_hdr;
        while let Some(open) = s.find('(') {
            let s2 = &s[open + 1..];
            let Some((nums, after)) = s2.split_once(')') else {
                break;
            };
            let mut np = nums.split(',');
            let (Some(cs), Some(cd)) = (
                np.next().and_then(|x| x.parse::<u32>().ok()),
                np.next().and_then(|x| x.parse::<u32>().ok()),
            ) else {
                break;
            };
            let text_end = after.find('(').unwrap_or(after.len());
            let text = &after[..text_end];
            if !text.is_empty() {
                chunks.push((cs, cd, text.to_string()));
            }
            s = &after[text_end..];
        }
        if chunks.is_empty() {
            continue;
        }
        let full_text: String = chunks
            .iter()
            .map(|c| c.2.clone())
            .collect::<String>()
            .trim()
            .to_string();
        if full_text.is_empty() || is_credit_line(&full_text) {
            continue;
        }
        let ends = yrc_ends(&chunks);
        // Merge chunks into words at whitespace boundaries.
        let mut words: Vec<(String, u32, u32)> = Vec::new();
        let mut cur = (chunks[0].2.clone(), chunks[0].0, ends[0]);
        for (i, ch) in chunks.iter().enumerate().skip(1) {
            if cur.0.ends_with(' ') || ch.2.starts_with(' ') {
                words.push((cur.0.trim().to_string(), cur.1, cur.2));
                cur = (ch.2.trim_start().to_string(), ch.0, ends[i]);
            } else {
                cur.0.push_str(&ch.2);
                cur.2 = ends[i];
            }
        }
        if !cur.0.trim().is_empty() {
            words.push((cur.0.trim().to_string(), cur.1, cur.2));
        }
        let words: Vec<(String, u32, u32)> =
            words.into_iter().filter(|w| !w.0.is_empty()).collect();
        if words.is_empty() {
            continue;
        }
        let text = words
            .iter()
            .map(|w| w.0.clone())
            .collect::<Vec<_>>()
            .join(" ");
        let text_words: Vec<&str> = text.split_whitespace().collect();
        let starts: Vec<u32> = words.iter().map(|w| w.1).collect();
        let mut ends_out: Vec<u32> = words.iter().map(|w| w.2).collect();
        for i in 0..ends_out.len() {
            if ends_out[i] <= starts[i] {
                ends_out[i] = if i + 1 < ends_out.len() {
                    starts[i + 1]
                } else {
                    starts[i] + 300
                };
            }
        }
        let (w, e) = if starts.len() == text_words.len() {
            (starts, ends_out)
        } else {
            (vec![], vec![])
        };
        lines.push(LyricLine {
            start_ms: line_start,
            text,
            words: w,
            ends: e,
        });
    }
    lines
}

const LRCLIB_EXCLUDE: &[&str] = &[
    "music video",
    "japanese",
    "jp ver",
    "remix",
    "live",
    "instrumental",
    "acoustic",
];

/// LRCLIB, ported from fetchLrclib() in yoshik.xyz/api/lyrics.js: exact `/api/get` first,
/// then `/api/search` with the exclude-keyword filter (Fix D) and duration within 3 s.
/// Synced lyrics preferred; plain lyrics are the last resort. A 429 just yields nothing.
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
    if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
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
    if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    if !r.status().is_success() {
        return None;
    }
    let v: Value = serde_json::from_str(&r.text().await.ok()?).ok()?;
    let req_title_lower = title.to_lowercase();
    let mut best_synced: Option<(Lyrics, f64)> = None;
    let mut best_plain: Option<(Lyrics, f64)> = None;
    for x in v.as_array()? {
        let t_name = x
            .get("trackName")
            .and_then(Value::as_str)
            .unwrap_or_else(|| x.get("name").and_then(Value::as_str).unwrap_or(""))
            .to_lowercase();
        // Fix D: skip excluded keywords unless the requested title contains that word.
        if LRCLIB_EXCLUDE
            .iter()
            .any(|kw| t_name.contains(kw) && !req_title_lower.contains(kw))
        {
            continue;
        }
        let diff = if duration_ms > 0 {
            (x.get("duration").and_then(Value::as_f64).unwrap_or(0.0) - secs).abs()
        } else {
            0.0
        };
        if duration_ms > 0 && diff > 3.0 {
            continue;
        }
        if let Some(lrc) = x.get("syncedLyrics").and_then(Value::as_str) {
            let lines = parse_lrc(lrc);
            if !lines.is_empty()
                && length_ok(&lines, duration_ms)
                && best_synced.as_ref().map(|(_, d)| diff < *d).unwrap_or(true)
            {
                best_synced = Some((
                    Lyrics {
                        synced: true,
                        word_sync: false,
                        lines,
                    },
                    diff,
                ));
            }
        }
        if let Some(plain) = x.get("plainLyrics").and_then(Value::as_str) {
            if best_plain.as_ref().map(|(_, d)| diff < *d).unwrap_or(true) {
                let mut lines = Vec::new();
                for l in plain.lines() {
                    push_line(&mut lines, 0, l);
                }
                if !lines.is_empty() {
                    best_plain = Some((
                        Lyrics {
                            synced: false,
                            word_sync: false,
                            lines,
                        },
                        diff,
                    ));
                }
            }
        }
    }
    if let Some((l, _)) = best_synced {
        return Some(l);
    }
    if let Some((l, _)) = best_plain {
        return Some(l);
    }
    None
}

/// One LRCLIB track object: synced lyrics if present, else plain; instrumental gives None.
fn lrclib_item(v: &Value) -> Option<Lyrics> {
    if v.get("instrumental").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if let Some(lrc) = v.get("syncedLyrics").and_then(Value::as_str) {
        let lines = parse_lrc(lrc);
        if !lines.is_empty() {
            return Some(Lyrics {
                synced: true,
                word_sync: false,
                lines,
            });
        }
    }
    let mut lines = Vec::new();
    for l in v.get("plainLyrics")?.as_str()?.lines() {
        push_line(&mut lines, 0, l);
    }
    (!lines.is_empty()).then_some(Lyrics {
        synced: false,
        word_sync: false,
        lines,
    })
}

/// `[mm:ss.xx] text`, several timestamps per line allowed; `[offset:...]` shifts all times
/// (port of parseLRC() in yoshik.xyz/api/lyrics.js). Non-time tags like `[ar:..]` are skipped.
fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut offset: i64 = 0;
    if let Some(pos) = lrc.find("[offset:") {
        let rest = &lrc[pos + 8..];
        if let Some(end) = rest.find(']') {
            offset = rest[..end].parse().unwrap_or(0);
        }
    }
    let mut lines = Vec::new();
    for raw in lrc.lines() {
        let mut rest = raw.trim_start();
        let mut times = Vec::new();
        while let Some(tag) = rest.strip_prefix('[') {
            let Some((inner, after)) = tag.split_once(']') else {
                break;
            };
            let Some(t) = lrc_time(inner) else {
                break;
            };
            times.push((t as i64 - offset).max(0) as u32);
            rest = after;
        }
        for t in times {
            push_line(&mut lines, t, rest);
        }
    }
    lines.sort_by_key(|l| l.start_ms);
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
    (!lines.is_empty()).then_some(Lyrics {
        synced,
        word_sync: false,
        lines,
    })
}

fn push_line(lines: &mut Vec<LyricLine>, start_ms: u32, text: &str) {
    let text = unescape(text.trim());
    if text.is_empty() {
        if let Some(last) = lines.last() {
            if !last.text.is_empty() {
                lines.push(LyricLine {
                    start_ms,
                    text: String::new(),
                    words: Vec::new(),
                    ends: Vec::new(),
                });
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
            lines.push(LyricLine {
                start_ms,
                text: sub.to_string(),
                words: Vec::new(),
                ends: Vec::new(),
            });
        }
    }
}

fn push_word_line(
    lines: &mut Vec<LyricLine>,
    start_ms: u32,
    text: &str,
    words: Vec<u32>,
    ends: Vec<u32>,
) {
    let text = unescape(text.trim());
    if text.is_empty() || text == "♪" {
        return;
    }
    // Word timings belong to the whole provider line: keep them only when it stays one line.
    let split = text.contains('\n');
    for sub in text.split('\n') {
        let sub = sub.trim();
        if !sub.is_empty() && sub != "♪" {
            lines.push(LyricLine {
                start_ms,
                text: sub.to_string(),
                words: if split { Vec::new() } else { words.clone() },
                ends: if split { Vec::new() } else { ends.clone() },
            });
        }
    }
}

async fn map_events(
    mut rx: librespot_playback::player::PlayerEventChannel,
    tx: UnboundedSender<Event>,
    shuffle: Arc<AtomicBool>,
    repeat: Arc<AtomicU8>,
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
                    explicit: audio_item.is_explicit,
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
                let mode = if track { 2 } else { context as u8 };
                repeat.store(mode, Ordering::Relaxed);
                Some(Event::Repeat(mode))
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

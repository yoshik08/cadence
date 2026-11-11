<p align="center"><img src="assets/icon-128.png" width="96" height="96" alt="Cadence icon"></p>

# Cadence — Lightweight Spotify Client for Windows (Pure Rust)

[![Release](https://img.shields.io/github/v/release/yoshik08/cadence)](https://github.com/yoshik08/cadence/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/Platform-Windows-0078D6.svg)](https://github.com/yoshik08/cadence/releases)
[![Made with Rust](https://img.shields.io/badge/Made%20with-Rust-orange.svg)](https://www.rust-lang.org/)

**Cadence is a lightweight Spotify desktop client for Windows, written entirely in Rust.** No Electron, no Chromium, no webview — just a native Slint UI rendered with OpenGL, streaming through [librespot](https://github.com/librespot-org/librespot), and library browsing through the Spotify Web API (a small built-in client).

It plays your Spotify library at 320 kbps with gapless playback and loudness normalisation, acts as a Spotify Connect device, and stays around **25 MB of RAM while playing** — roughly 10× lighter than the official Spotify desktop app.

## Download

Grab `cadence.exe` from the [latest release](https://github.com/yoshik08/cadence/releases) (~12 MB, no installer, no dependencies). Windows 10 or 11, Spotify Premium required.

## Why Cadence

|  | Cadence | Official Spotify app |
| --- | --- | --- |
| RAM while playing | ~55–70 MB | ~300–500 MB |
| Download size | ~12 MB | ~100 MB+ |
| UI stack | Native Slint (OpenGL, vsync) | Electron / Chromium |
| Startup | Under 1 s | Several seconds |
| Playback | 320 kbps, gapless, normalisation | 320 kbps |
| Spotify Connect device | Yes | Yes |
| Lyrics | Yes (synced) | Yes |

## Features

**Playback**
- Spotify Connect device named "Cadence": pick it from any Spotify app, or play from Cadence itself
- Queue with Play next and Add to queue, drag to reorder, remove, clear; resumes where you left off after a restart
- Shuffle (Random or Fewer repeats) and repeat off / all / one, remembered
- Audio follows the Windows default device, so Bluetooth headphones connected later just work; or pin a device
- 96 / 160 / 320 kbps, gapless, optional volume normalization (Quiet / Normal / Loud), mono, 6-band equalizer
- Podcasts: show pages, follow, resume, mark played, 0.5x to 2x speed without pitch change
- Local files (MP3, FLAC, WAV, AAC, M4A, Ogg) from folders you choose, played through the same queue
- Sleep timer with fade-out, including "end of this track"

**Library and browsing**
- Home with Recently played, your playlists, Liked Songs, albums, new releases from artists you follow, podcasts and your most played; reorder or hide sections
- Your Library with tabs, search and sort; sidebar with filter, pinned playlists and Cadence-local folders
- Playlist pages: edit name, description and cover, public / private, collaborative, delete, sortable columns (remembered), duplicate finder, add songs from search, CSV export
- Album, artist (popular tracks, discography filters, follow, your liked songs) and podcast pages
- Search across songs, artists, albums, playlists, podcasts and episodes with a top result, Show all and search history; paste a Spotify link anywhere (Ctrl+V) to open it
- Right-click menus everywhere; back / forward with Alt+arrows or the mouse side buttons

**Windows**
- Media keys, the Windows media flyout (shows Cadence with its icon), taskbar thumbnail buttons
- Tray icon with playback controls and an audio output switcher, close to tray, start with Windows
- Global hotkeys: Ctrl+Alt+Space / Right / Left / L (like) / C (copy "Artist - Title")
- Mini player (always on top), optional track-change notifications and media-key overlay, pause on lock or when Bluetooth disconnects

**Lyrics**
- Lyrics page in the yoshik.xyz design with word-synced lines, plus a Now Playing side panel with synced lyrics
- Per-song lyrics timing offset (±0.1 s steps), saved per track

**Yours, on this PC**
- Listening stats (top songs, artists, albums, listening time) counted from Cadence's own plays; private session pauses recording
- A small local Cadence profile (name, picture, bio)

## Requirements

- Windows 10 or 11
- Rust stable (edition 2024, so a recent toolchain) — only if building from source
- Visual Studio 2022 Build Tools with the "Desktop development with C++" workload — only if building from source
- Spotify Premium (librespot only streams for Premium accounts, and the app owner needs it too, see Limits)

No OpenSSL, cmake or NASM is needed: TLS is the Windows native stack (schannel).

## Spotify developer app

Cadence uses your own Spotify app for the Web API (PKCE, no client secret).

1. Open [https://developer.spotify.com/dashboard](https://developer.spotify.com/dashboard) and log in.
2. Create app. Pick any name and description.
3. Under "Which API/SDKs are you planning to use?" select Web API.
4. Add the redirect URI exactly as `http://127.0.0.1:8888/callback` and save.
5. Open the app's settings and copy the Client ID.
6. Give it to Cadence in one of two ways:
   - set the `SPOTIFY_CLIENT_ID` environment variable, or
   - put it on the first line of `%APPDATA%\cadence\client_id.txt`.

Anyone else who should use the app has to be added under User Management in the dashboard (see Limits).

## First launch

Cadence opens your browser twice, once each for:

1. the Web API (your library, playlists, search), using your client ID, and
2. streaming, using librespot's own sign-in. Spotify treats streaming as a separate grant, so this is a second approval.

Both are one-time. Spotify ends the Web API sign-in 6 months after you first approve it; Cadence warns two weeks ahead and Settings > Re-authorize renews it. Updating from 1.3 asks for the Web API approval once more: Cadence now requests every permission it uses in that one step. The results are cached in `%APPDATA%\cadence`: `web_token.json` and `librespot\credentials.json`. To sign out, close Cadence and delete those two files. If the client ID is missing, the library stays empty and the page says so; streaming still works, for example from another Spotify app through Connect.

## Build and run

```
cargo build --release
target\release\cadence.exe
```

Keep `Cargo.lock`. `cargo update` pulls a `vergen` release that breaks `librespot-core`'s build script; if that happens, restore it with `cargo update -p vergen --precise 9.0.6`.

For development, `cargo build --features smoke` adds a PNG snapshot writer. Two environment variables help with testing, and neither signs in or touches the network:

- `CADENCE_SMOKE=1` opens an inactive window, writes a snapshot to `CADENCE_SNAPSHOT` (default `smoke.png`) after about 1.5 s, and exits.
- `CADENCE_OFFLINE=1` opens an inactive window and keeps running until closed, for measuring idle memory.
- `CADENCE_MEM=1` logs the working set every 5 s to `%LOCALAPPDATA%\cadence\cadence.log`.
- `CADENCE_RODIO=1` uses librespot's own audio output instead of Cadence's (it doesn't follow device changes).
- `CADENCE_DEMO=1` shows the lyrics page for one track with a simulated clock and no Spotify session; `scripts/demo.sh` drives it and writes snapshots.

Karaoke mode (Apple Music syllable timings) is a personal-build feature: `cargo build --release --features karaoke` with an Apple Music media-user-token in `APPLE_MEDIA_USER_TOKEN` or `%APPDATA%\cadence\apple_media_token.txt`. Release builds leave it out.

Two librespot crates are vendored with small, marked patches (`vendor/*/CADENCE_PATCH.md`): queue editing for the local client, and local files at any sample rate in more formats.

Your library index, listening stats, search history, local files and podcast progress live in `%APPDATA%\cadence\cadence.db` (SQLite). Caches live in `%LOCALAPPDATA%\cadence\cache` (`audio`, `art`, `lyrics`, `api`), each size-capped and pruned oldest-first at startup.

## Memory

v1.3 renders on the GPU (OpenGL via Slint femtovg, vsync on), which trades some memory for frames paced to the display. Measured on 2026-11-08, release build, 1500x900 window, 180 Hz display: lyrics page closed 53 MB private working set and about 1% of a core; lyrics page open about 66 MB and 10%; karaoke (personal build) about 64 MB and 42% at a steady 180 fps. Flat over time. Most of the baseline is the OpenGL driver.

To measure real playback, start a track with the library open, then run:

```
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\measure.ps1 -Minutes 10
```

It attaches to the running app. In Task Manager, read the "Memory (active private working set)" column, not "Working set".

Playback, release build, 10 min with the library open and a playlist playing (9 tracks, 100% DPI, 1200×780 window, measured with scripts\measure.ps1 on 2026-11-08): private working set min 17.4 MB, avg 25.2 MB, max 28.1 MB, final 23.3 MB; peak commit 31.5 MB; peak 21 threads. Flat over time, no growth across track changes.

## Limits

- New Spotify apps start in Development Mode. They are capped to a small allowlist of users until extended quota mode is approved. The cap was 25 users, and Spotify cut it to 5 for apps created after February 2026. The app owner needs Premium.
- Development Mode also hides the track lists of playlists you don't own (the header shows and Play still works), and search returns at most 10 results per type. Spotify removed artist top tracks, recommendations, new releases, editorial playlists and audio features for new apps; Cadence gets popular tracks through librespot when it can, and otherwise says so and shows your own top tracks for that artist.
- Not available to apps: Lossless, downloads, Jam, Canvas, friends activity, user search and editing your Spotify profile. Playlist folders and the profile in Cadence are local to your PC and labelled that way.

## Legal

librespot is an unofficial client library. Using it is against Spotify's Terms of Service, so use Cadence at your own risk. Cadence is not affiliated with or endorsed by Spotify.

---

Icon by [Icons8](https://icons8.com).

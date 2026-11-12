<p align="center"><img src="assets/icon-128.png" width="96" height="96" alt="Cadence icon"></p>

# Cadence â€” Lightweight Spotify Client for Windows (Pure Rust)

[![Release](https://img.shields.io/github/v/release/yoshik08/cadence)](https://github.com/yoshik08/cadence/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/Platform-Windows-0078D6.svg)](https://github.com/yoshik08/cadence/releases)
[![Made with Rust](https://img.shields.io/badge/Made%20with-Rust-orange.svg)](https://www.rust-lang.org/)

**Cadence is a lightweight Spotify desktop client for Windows, written entirely in Rust.** No Electron, no Chromium, no webview â€” just a native Slint UI rendered with OpenGL, streaming through [librespot](https://github.com/librespot-org/librespot), and library browsing through the Spotify Web API (a small built-in client).

It plays your Spotify library at 320 kbps with gapless playback and loudness normalisation, acts as a Spotify Connect device, and stays around **25 MB of RAM while playing** â€” roughly 10Ã— lighter than the official Spotify desktop app.

## Download

Grab `cadence.exe` from the [latest release](https://github.com/yoshik08/cadence/releases) (~22 MB, no installer, no dependencies). Later versions install from Settings > Updates. Windows 10 or 11, Spotify Premium required.

## Why Cadence

|  | Cadence | Official Spotify app |
| --- | --- | --- |
| RAM while playing | ~55â€“70 MB | ~300â€“500 MB |
| Download size | ~22 MB | ~100 MB+ |
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
- Home with Recently played, your playlists, Liked Songs, albums, new releases from artists you follow, podcasts; reorder or hide sections
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

## Requirements

- Windows 10 or 11
- Rust stable (edition 2024, so a recent toolchain) â€” only if building from source
- Visual Studio 2022 Build Tools with the "Desktop development with C++" workload â€” only if building from source
- Spotify Premium (librespot only streams for Premium accounts, and the app owner needs it too, see Limits)

No OpenSSL, cmake or NASM is needed: TLS is the Windows native stack (schannel).

## Spotify developer app

Cadence uses your own Spotify app for your library (PKCE, no client secret). The first time you start Cadence it shows a setup card that walks you through it:

How to get your Client ID (takes about 2 minutes). Your Spotify account needs Premium to create an app.

1. Click Open Spotify Dashboard and log in.
2. Click Create app (top right).
3. App name: anything, for example Cadence.
4. App description: anything, for example My music player. Required.
5. Website: leave blank.
6. Redirect URIs: click Copy below, paste it in the field, then click the Add button next to the field. It must show in the list before you save. After saving, check it still reads 127.0.0.1 and not localhost.
   Redirect URI: `http://127.0.0.1:8888/callback`
7. Under Which API/SDKs are you planning to use, tick Web API.
8. Tick the box agreeing to Spotify's Developer Terms of Service and Design Guidelines, then click Save.
9. Open your new app, go to Settings, and copy the Client ID.
10. Paste it above and click Continue.

Cadence opens your browser twice, once for your library and once for streaming. Approve both. Each is one-time.

Anyone else who should use the app has to be added under User Management in the dashboard (see Limits). To use a different app later, Settings > Account > Change Client ID; your current sign-in stays until the new one succeeds.

## First launch

After you paste your Client ID and click Continue, Cadence opens your browser twice:

1. Step 1 of 2: your library, playlists and search (the Web API), using your Client ID.
2. Step 2 of 2: streaming, using librespot's own sign-in. Spotify treats streaming as a separate grant, so this is a second approval.

Both are one-time. Spotify ends the Web API sign-in 6 months after you first approve it; Cadence warns two weeks ahead and Settings > Re-authorize renews it, keeping your Client ID.

## Updating

Settings > Updates shows your version and, when a newer release exists, Update now (downloads, installs and restarts) or Update on next restart. Cadence checks once per launch and never downloads anything until you click. Updates replace only the program file: your Client ID, both sign-ins, settings and caches are kept, so you never sign in again because of an update. Replacing `cadence.exe` by hand keeps them too.

Updating from 1.4 asks for nothing: 1.5 uses the same permissions. (Updating from 1.3 or older asks for the library approval once, because 1.4 added permissions.)

## Signing out

Settings > Account > Sign out removes Cadence's two sign-ins from this PC and quits; the next start asks you to approve again. Your Client ID and settings stay.

## Build and run

```
cargo build --release
target\release\cadence.exe
```

Keep `Cargo.lock`. `cargo update` pulls a `vergen` release that breaks `librespot-core`'s build script; if that happens, restore it with `cargo update -p vergen --precise 9.0.6`.

For development, `cargo build --features smoke` adds a PNG snapshot writer. Two environment variables help with testing, and neither signs in or touches the network:

- `CADENCE_SMOKE=1` opens an inactive window, writes a snapshot to `CADENCE_SNAPSHOT` (default `smoke.png`) after about 1.5 s, and exits.
- `CADENCE_OFFLINE=1` opens an inactive window and keeps running until closed, for measuring idle memory.
- `CADENCE_MEM=1` logs the working set every 5 s to `cadence.log` (Settings > Open log folder).
- `CADENCE_RODIO=1` uses librespot's own audio output instead of Cadence's (it doesn't follow device changes).
- `CADENCE_DEMO=1` shows the lyrics page for one track with a simulated clock and no Spotify session; `scripts/demo.sh` drives it and writes snapshots.

Karaoke mode (Apple Music syllable timings) is a personal-build feature: `cargo build --release --features karaoke` with an Apple Music media-user-token (see `src/apple.rs`). Release builds leave it out.

Two librespot crates are vendored with small, marked patches (`vendor/*/CADENCE_PATCH.md`): queue editing for the local client, and local files at any sample rate in more formats.

Your library index, search history, local files and podcast progress live in a local SQLite database next to your settings. Caches (`audio`, `art`, `lyrics`, `api`) are each size-capped and pruned oldest-first at startup; Settings > Storage shows and clears them.

## Memory

v1.3 renders on the GPU (OpenGL via Slint femtovg, vsync on), which trades some memory for frames paced to the display. Measured on 2026-11-08, release build, 1500x900 window, 180 Hz display: lyrics page closed 53 MB private working set and about 1% of a core; lyrics page open about 66 MB and 10%; karaoke (personal build) about 64 MB and 42% at a steady 180 fps. Flat over time. Most of the baseline is the OpenGL driver.

To measure real playback, start a track with the library open, then run:

```
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\measure.ps1 -Minutes 10
```

It attaches to the running app. In Task Manager, read the "Memory (active private working set)" column, not "Working set".

Playback, release build, 10 min with the library open and a playlist playing (9 tracks, 100% DPI, 1200Ã—780 window, measured with scripts\measure.ps1 on 2026-11-08): private working set min 17.4 MB, avg 25.2 MB, max 28.1 MB, final 23.3 MB; peak commit 31.5 MB; peak 21 threads. Flat over time, no growth across track changes.

## Limits

- New Spotify apps start in Development Mode. They are capped to a small allowlist of users until extended quota mode is approved. The cap was 25 users, and Spotify cut it to 5 for apps created after February 2026. The app owner needs Premium.
- Development Mode also hides the track lists of playlists you don't own (the header shows and Play still works), and search returns at most 10 results per type. Spotify removed artist top tracks, recommendations, new releases, editorial playlists and audio features for new apps; Cadence gets popular tracks through librespot when it can, and otherwise says so and shows your own top tracks for that artist.
- Not available to apps: Lossless, downloads, Jam, Canvas, friends activity, user search and editing your Spotify profile. Playlist folders in Cadence are local to your PC and labelled that way.

## Legal

librespot is an unofficial client library. Using it is against Spotify's Terms of Service, so use Cadence at your own risk. Cadence is not affiliated with or endorsed by Spotify.

---

Icon by [Icons8](https://icons8.com).


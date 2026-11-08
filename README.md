# Cadence

A small Spotify desktop client for Windows, written entirely in Rust. The UI is Slint on its software renderer (no GPU stack, no webview), streaming is [librespot](https://github.com/librespot-org/librespot), and library browsing uses the Web API through [rspotify](https://github.com/ramsayleung/rspotify). The target is under 100 MB of private working set while playing with the library open.

## Requirements

- Windows 10 or 11
- Rust stable (edition 2024, so a recent toolchain)
- Visual Studio 2022 Build Tools with the "Desktop development with C++" workload
- Spotify Premium (librespot only streams for Premium accounts, and the app owner needs it too, see Limits)

No OpenSSL, cmake or NASM is needed: TLS is the Windows native stack (schannel).

## Spotify developer app

Cadence uses your own Spotify app for the Web API (PKCE, no client secret).

1. Open <https://developer.spotify.com/dashboard> and log in.
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

Both are one-time. The results are cached in `%APPDATA%\cadence`: `web_token.json` and `librespot\credentials.json`. To sign out, close Cadence and delete those two files. If the client ID is missing, the library stays empty and the page says so; streaming still works, for example from another Spotify app through Connect.

## Build and run

```
cargo build --release
target\release\cadence.exe
```

Keep `Cargo.lock`. `cargo update` pulls a `vergen` release that breaks `librespot-core`'s build script; if that happens, restore it with `cargo update -p vergen --precise 9.0.6`.

For development, `cargo build --features smoke` adds a PNG snapshot writer. Two environment variables help with testing, and neither signs in or touches the network:

- `CADENCE_SMOKE=1` opens an inactive window, writes a snapshot to `CADENCE_SNAPSHOT` (default `smoke.png`) after about 1.5 s, and exits.
- `CADENCE_OFFLINE=1` opens an inactive window and keeps running until closed, for measuring idle memory.

## Features

- Spotify Connect device named "Cadence": pick it from any Spotify app, or play from Cadence itself
- Gapless playback and loudness normalisation
- Media keys and the Windows media overlay (System Media Transport Controls)
- Space plays and pauses, Esc closes the full player
- Home (recently played, your playlists), search, playlist, album and artist pages, Liked Songs
- The accent colour follows the current album art

## Limits

- New Spotify apps start in Development Mode. They are capped to a small allowlist of users until extended quota mode is approved. The cap was 25 users, and Spotify cut it to 5 for apps created after February 2026. The app owner needs Premium.
- Development Mode also hides the track lists of playlists you don't own (the header shows and Play still works), search returns at most 10 results per type, and there is no artist top-tracks endpoint, so artist pages list albums instead.

## Memory

Idle, offline release build (`CADENCE_OFFLINE=1`): 6.4 MB private working set, about 8 MB commit, about 8 threads.

The software renderer's framebuffer costs 4 bytes per window pixel: 3.6 MB at 1200x780 and 100% scaling, about 8 MB for a maximised 1080p window, about 32 MB for a maximised 4K one.

To measure real playback, start a track with the library open, then run:

```
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\measure.ps1 -Minutes 10
```

It attaches to the running app. In Task Manager, read the "Memory (active private working set)" column, not "Working set".

Playback, release build, 10 min with the library open and a playlist playing (9 tracks, 100% DPI, 1200×780 window, measured with scripts\measure.ps1 on 2026-11-08): private working set min 17.4 MB, avg 25.2 MB, max 28.1 MB, final 23.3 MB; peak commit 31.5 MB; peak 21 threads. Flat over time, no growth across track changes.

## Legal

librespot is an unofficial client library. Using it is against Spotify's Terms of Service, so use Cadence at your own risk. Cadence is not affiliated with or endorsed by Spotify.

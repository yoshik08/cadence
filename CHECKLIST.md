# Cadence implementation checklist

Tracks the "Cadence Master Implementation Prompt" (Nov 10, 2026). Updated for v1.4 (2026-11-11).

Status: **[x]** built and verified by running it; **[~]** built, compiles and unit-tested where
possible, but not verified end to end (reason given); **[ ]** not done.

The main reason for [~]: every Web API write and most Web API reads need a Spotify sign-in with
the new 16-scope consent, which only the account owner can approve in a browser. Streaming
(librespot) features were verified live with isolated test runs. Windows integrations that need a
focused, interactive window (tray, hotkeys, dialogs) were not exercised, so tests never take focus.

Memory check: release build, `CADENCE_MEM=1` or `scripts/measure.ps1`.

## Stage 1: known bugs

- [~] Bug 1 daily re-auth: rspotify replaced by `src/web.rs` (keeps the refresh token, serialized refresh, atomic file, 6-month warning, invalid_grant re-sign-in, logs). Unit-tested. Needs one browser sign-in to confirm live.
- [~] Bug 2 Bluetooth: WASAPI sink follows the default device (`src/audio.rs`). Playback verified; the Bluetooth switch needs real headphones.
- [~] Bug 3 media flyout name: Start menu shortcut with the app ID, property verified; the flyout itself needs a look.
- [~] Bug 4 thumbnail buttons: added (verified the call succeeds); clicking needs a person.

## Features

- [x] 3.1 Queue: Play next / Add to queue layers, panel, drag reorder, remove, clear, durations, persistence + resume (verified live).
- [x] 3.2 Shuffle (Random / Fewer repeats, unit-tested) and repeat off / all / one, persisted; Shuffle buttons on album, playlist, artist, Liked Songs, local files. Artist URIs play as context (verified).
- [~] 3.3 Library page (tabs, search, sort, create playlist), sidebar filter, pins, Cadence-local folders (SQLite). UI verified signed out; data needs sign-in.
- [~] 3.4 Playlist page: header, Play / Shuffle, ⋯ (add to queue, edit details + cover upload, public / private, collaborative, invite = copy link, delete / remove with confirm, add to other playlist, move to folder, pin, duplicates, CSV, copy link / URI), sortable remembered columns, Add songs panel with duplicate confirm. Duplicate plan unit-tested. Needs sign-in.
- [~] 3.5 Context menus on tracks, episodes, albums, artists, playlists, folders, queue rows, the bar; keyboard inside menus. Menu rendering verified; Web API actions need sign-in. Opening a menu from the keyboard (rows aren't focusable) is not done.
- [~] 3.6 Search: mixed results, Top result, Show all with paging, history (SQLite), paste-a-link (field and Ctrl+V anywhere), arrow keys / Enter / Escape, states. Needs sign-in for results.
- [~] 3.7 Album page: header, Save, explicit badges, per-track hearts, Now Playing + lyrics panel. Needs sign-in.
- [x] 3.8 Artist page: Popular via librespot artist metadata (verified live, works signed out), honest local fallback, name / photo fallback from librespot, Follow, discography with filters and Load more, "You liked N songs". Discography and follow need sign-in.
- [~] 3.9 Home: greeting, Recently played, Your playlists, Liked Songs, albums, new releases from followed artists, podcasts, Most played (local), skeletons, hover play, art-tinted background, reorder / hide. Made For You: not reachable without unofficial hub APIs, left out. Needs sign-in.
- [x] 3.10 Podcasts: episodes play through librespot (verified), speed 0.5–2× via WSOLA time-stretch (verified: 2× doubles progress), resume, progress saving, mark played (SQLite). Show pages / follow need sign-in.
- [x] 3.11 Router: back / forward (buttons, Alt+arrows, mouse side buttons), history, scroll restoration, link handling. Verified page switching; mouse buttons not exercised.
- [~] 3.12 Settings: everything listed in the prompt that can work (quality, normalize + level, gapless, autoplay, mono, EQ with presets, explicit filter, shuffle style, output device, zoom, compact, panel on play, notifications, overlay, start with Windows, close to tray, hotkeys, Bluetooth / lock pause, cache size / clear / open, proxy, hardware acceleration, local files, private session, language note, re-authorize, account link, memory readout, debug log). Page verified; restart-bound settings need a restart.
- [~] 3.13 Windows: tray with controls + audio output switcher, close to tray, start with Windows (Run key), native file dialogs, global hotkeys, mini player, media-key overlay. Not exercised (would take focus).
- [x] 3.14 Lyrics: per-song offset (±0.1 s, SQLite) on the lyrics page and side panel; Now Playing + lyrics panel on artist and album pages (verified).
- [~] 3.15 Sleep timer: presets, end of track, custom, 15 s fade, pause (not disconnect), restore volume, skip cancels end-of-track. Logic reviewed, not timed live.
- [~] 3.16 QoL: pause on Bluetooth disconnect / resume on reconnect, pause on lock, resume after restart (verified), copy now playing, CSV export.
- [x] 3.17 Local files: scan, tags, SQLite index, change watching, missing detection, playback through librespot (patched for any sample rate, WAV / AAC / M4A / Ogg); verified with a generated WAV. Embedded artwork code not exercised (test file had none).
- [~] 3.18 Local stats: plays recorded (30 s or half), top songs / artists / albums, time, ranges, clear, private session. Recording logic unit-tested via db test.
- [x] 3.19 Local Cadence profile, clearly labelled (page verified).

## Measurements (v1.4 release, signed-out test run, queue panel open, playing)

| Time | Working set | Private |
|---|---|---|
| 9 s | 36 MB | 246 MB |
| 18–63 s | 17–47 MB | ~231 MB |

Under the 100 MB working-set cap. Private bytes stay high because of the NVIDIA OpenGL driver
(software renderer: Settings > Hardware acceleration off).

## Known gaps

- Web API features above are untested until signed in.
- Keyboard-opened context menus; Made For You shelf.
- `CADENCE_TEST_*` hooks: QUEUE, NEXT, RESUME, MENU, ROUTES (offline / inactive), PLAY=local / show uri.

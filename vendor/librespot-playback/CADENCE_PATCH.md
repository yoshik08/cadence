# Cadence patch on librespot-playback 0.8.0

- Local files: `register_local_files(map)` lets the app supply `spotify:local:` uri -> path at
  runtime (Cadence scans and watches its own folders); `local_uri_for(path)` exposes the uri scheme.
- Extra formats for local files: WAV, AAC/M4A, Ogg Vorbis (symphonia features wav, pcm, aac, isomp4).
- Local files at any sample rate or channel count play: converted to stereo and resampled to
  44.1 kHz (streaming Catmull-Rom) instead of being rejected.
- `bytes_per_second` no longer divides by zero for files shorter than 1 s.

Patch code is marked "Cadence patch".

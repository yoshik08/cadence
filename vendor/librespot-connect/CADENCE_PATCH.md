# Cadence patch on librespot-connect 0.8.0

Unmodified crates.io 0.8.0 plus queue editing for the local client (Spirc 0.8.0 only accepts
queue changes from remote Connect clients):

- `Spirc::queue_insert(uri, position)`, `queue_remove(uid)`, `queue_move(uid, position)`,
  `queue_clear()`, `queue_snapshot()`; types `QueueItem`, `QueueSnapshot` in `lib.rs`.
- `ConnectState::cadence_*` helpers at the end of `src/state/tracks.rs`, built on the crate's own
  `add_to_queue` / `next_tracks` handling, so other Connect clients see the same queue.

All patch code is marked "Cadence patch". Drop this folder once a librespot release exposes
`add_to_queue` (it is on the dev branch after 0.8.0).

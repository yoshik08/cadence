//! On-disk store (`%APPDATA%\cadence\cadence.db`, SQLite). Everything that could grow with the
//! library lives here and is queried on demand instead of being held in RAM: the library index,
//! listening stats, search history, local files, episode progress, lyrics offsets, sidebar pins
//! and Cadence-local playlist folders. The page cache is capped at 512 KB.

use rusqlite::{Connection, OptionalExtension, params};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn conn() -> MutexGuard<'static, Connection> {
    static C: OnceLock<Mutex<Connection>> = OnceLock::new();
    C.get_or_init(|| {
        let path = crate::auth::data_dir().join("cadence.db");
        let c = Connection::open(&path).unwrap_or_else(|e| {
            crate::logln!("db: cannot open {}: {e}; using memory", path.display());
            Connection::open_in_memory().expect("in-memory sqlite")
        });
        let _ = c.execute_batch(SCHEMA);
        Mutex::new(c)
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = -512;
PRAGMA temp_store = FILE;
CREATE TABLE IF NOT EXISTS library (
    uri TEXT PRIMARY KEY, kind TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
    subtitle TEXT NOT NULL DEFAULT '', artist TEXT NOT NULL DEFAULT '', artist_uri TEXT NOT NULL DEFAULT '',
    album TEXT NOT NULL DEFAULT '', album_uri TEXT NOT NULL DEFAULT '', added_at TEXT NOT NULL DEFAULT '',
    duration_ms INTEGER NOT NULL DEFAULT 0, explicit INTEGER NOT NULL DEFAULT 0, image TEXT NOT NULL DEFAULT '',
    owner_id TEXT NOT NULL DEFAULT '', collaborative INTEGER NOT NULL DEFAULT 0, public INTEGER NOT NULL DEFAULT 0,
    total INTEGER NOT NULL DEFAULT 0, position INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS library_kind ON library(kind, position);
CREATE INDEX IF NOT EXISTS library_artist ON library(artist_uri);
CREATE TABLE IF NOT EXISTS sync_state (kind TEXT PRIMARY KEY, synced_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS plays (
    id INTEGER PRIMARY KEY, uri TEXT NOT NULL, title TEXT NOT NULL, artist TEXT NOT NULL,
    artist_uri TEXT NOT NULL DEFAULT '', album TEXT NOT NULL, album_uri TEXT NOT NULL DEFAULT '',
    ms INTEGER NOT NULL, at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS plays_at ON plays(at);
CREATE TABLE IF NOT EXISTS search_history (query TEXT PRIMARY KEY, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS local_tracks (
    path TEXT PRIMARY KEY, uri TEXT NOT NULL, title TEXT NOT NULL, artist TEXT NOT NULL,
    album TEXT NOT NULL, duration_ms INTEGER NOT NULL, mtime INTEGER NOT NULL, size INTEGER NOT NULL,
    missing INTEGER NOT NULL DEFAULT 0, error TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS episode_progress (
    uri TEXT PRIMARY KEY, position_ms INTEGER NOT NULL, duration_ms INTEGER NOT NULL,
    played INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS lyrics_offset (uri TEXT PRIMARY KEY, ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pins (uri TEXT PRIMARY KEY, position INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS folders (id INTEGER PRIMARY KEY, name TEXT NOT NULL, position INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS folder_items (folder_id INTEGER NOT NULL, uri TEXT PRIMARY KEY, position INTEGER NOT NULL);
";

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------------------------
// Library index.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LibItem {
    pub uri: String,
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    pub artist_uri: String,
    pub album: String,
    pub album_uri: String,
    pub added_at: String,
    pub duration_ms: i64,
    pub explicit: bool,
    pub image: String,
    pub owner_id: String,
    pub collaborative: bool,
    pub public: bool,
    pub total: i64,
}

/// Replaces every row of `kind` with `items` (in order) in one transaction.
pub fn library_replace(kind: &str, items: &[LibItem]) {
    let mut c = conn();
    let Ok(tx) = c.transaction() else { return };
    let _ = tx.execute("DELETE FROM library WHERE kind = ?1", params![kind]);
    {
        let Ok(mut st) = tx.prepare(
            "INSERT OR REPLACE INTO library (uri, kind, title, subtitle, artist, artist_uri, album, album_uri, added_at,
             duration_ms, explicit, image, owner_id, collaborative, public, total, position)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        ) else {
            return;
        };
        for (i, it) in items.iter().enumerate() {
            let _ = st.execute(params![
                it.uri, kind, it.title, it.subtitle, it.artist, it.artist_uri, it.album, it.album_uri, it.added_at,
                it.duration_ms, it.explicit, it.image, it.owner_id, it.collaborative, it.public, it.total, i as i64
            ]);
        }
    }
    let _ = tx.execute("INSERT OR REPLACE INTO sync_state (kind, synced_at) VALUES (?1, ?2)", params![kind, now()]);
    let _ = tx.commit();
}

/// Inserts or removes one row after a like / save / follow made in Cadence.
pub fn library_set(kind: &str, item: Option<&LibItem>, uri: &str) {
    let c = conn();
    match item {
        Some(it) => {
            let _ = c.execute(
                "INSERT OR REPLACE INTO library (uri, kind, title, subtitle, artist, artist_uri, album, album_uri, added_at,
                 duration_ms, explicit, image, owner_id, collaborative, public, total, position)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,-1)",
                params![it.uri, kind, it.title, it.subtitle, it.artist, it.artist_uri, it.album, it.album_uri, it.added_at,
                    it.duration_ms, it.explicit, it.image, it.owner_id, it.collaborative, it.public, it.total],
            );
        }
        None => {
            let _ = c.execute("DELETE FROM library WHERE uri = ?1 AND kind = ?2", params![uri, kind]);
        }
    }
}

pub fn synced_at(kind: &str) -> Option<i64> {
    conn().query_row("SELECT synced_at FROM sync_state WHERE kind = ?1", params![kind], |r| r.get(0)).optional().ok().flatten()
}

fn lib_row(r: &rusqlite::Row) -> rusqlite::Result<LibItem> {
    Ok(LibItem {
        uri: r.get(0)?,
        kind: r.get(1)?,
        title: r.get(2)?,
        subtitle: r.get(3)?,
        artist: r.get(4)?,
        artist_uri: r.get(5)?,
        album: r.get(6)?,
        album_uri: r.get(7)?,
        added_at: r.get(8)?,
        duration_ms: r.get(9)?,
        explicit: r.get(10)?,
        image: r.get(11)?,
        owner_id: r.get(12)?,
        collaborative: r.get(13)?,
        public: r.get(14)?,
        total: r.get(15)?,
    })
}

const LIB_COLS: &str = "uri, kind, title, subtitle, artist, artist_uri, album, album_uri, added_at, duration_ms, explicit, image, owner_id, collaborative, public, total";

/// Rows of `kind`, filtered by `filter` (title/artist contains), ordered by `sort`
/// ("recent" = library order, "alpha", "creator"), at most `limit`.
pub fn library_list(kind: &str, filter: &str, sort: &str, limit: usize) -> Vec<LibItem> {
    let order = match sort {
        "alpha" => "title COLLATE NOCASE",
        "creator" => "subtitle COLLATE NOCASE, title COLLATE NOCASE",
        _ => "position",
    };
    let sql = format!(
        "SELECT {LIB_COLS} FROM library WHERE kind = ?1 AND (?2 = '' OR title LIKE ?3 OR artist LIKE ?3 OR subtitle LIKE ?3)
         ORDER BY {order} LIMIT ?4"
    );
    let like = format!("%{}%", filter.replace('%', "\\%"));
    let c = conn();
    let Ok(mut st) = c.prepare(&sql) else { return Vec::new() };
    st.query_map(params![kind, filter, like, limit as i64], lib_row).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub fn library_get(uri: &str) -> Option<LibItem> {
    let c = conn();
    c.query_row(&format!("SELECT {LIB_COLS} FROM library WHERE uri = ?1"), params![uri], lib_row).optional().ok().flatten()
}

pub fn library_contains(uri: &str) -> bool {
    conn().query_row("SELECT 1 FROM library WHERE uri = ?1", params![uri], |_| Ok(())).optional().ok().flatten().is_some()
}

/// Liked tracks of one artist (for "You liked N songs").
pub fn liked_by_artist(artist_uri: &str) -> Vec<LibItem> {
    let c = conn();
    let Ok(mut st) = c.prepare(&format!("SELECT {LIB_COLS} FROM library WHERE kind = 'track' AND artist_uri = ?1 ORDER BY position")) else {
        return Vec::new();
    };
    st.query_map(params![artist_uri], lib_row).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub fn library_count(kind: &str) -> i64 {
    conn().query_row("SELECT COUNT(*) FROM library WHERE kind = ?1", params![kind], |r| r.get(0)).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Listening stats (Cadence's own plays only).

pub struct Play<'a> {
    pub uri: &'a str,
    pub title: &'a str,
    pub artist: &'a str,
    pub artist_uri: &'a str,
    pub album: &'a str,
    pub album_uri: &'a str,
    pub ms: i64,
}

pub fn record_play(p: &Play) {
    let _ = conn().execute(
        "INSERT INTO plays (uri, title, artist, artist_uri, album, album_uri, ms, at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![p.uri, p.title, p.artist, p.artist_uri, p.album, p.album_uri, p.ms, now()],
    );
}

#[derive(Clone, Debug)]
pub struct Top {
    pub key: String,
    pub title: String,
    pub subtitle: String,
    pub plays: i64,
    pub ms: i64,
}

/// Top songs / artists / albums since `since` (unix seconds; 0 = all time).
pub fn top(what: &str, since: i64, limit: usize) -> Vec<Top> {
    let (key, title, sub) = match what {
        "artists" => ("artist", "artist", "''"),
        "albums" => ("album || '|' || artist", "album", "artist"),
        _ => ("uri", "title", "artist"),
    };
    let sql = format!(
        "SELECT {key}, MAX({title}), MAX({sub}), COUNT(*), SUM(ms) FROM plays WHERE at >= ?1 AND {title} != ''
         GROUP BY {key} ORDER BY COUNT(*) DESC, SUM(ms) DESC LIMIT ?2"
    );
    let c = conn();
    let Ok(mut st) = c.prepare(&sql) else { return Vec::new() };
    st.query_map(params![since, limit as i64], |r| {
        Ok(Top { key: r.get(0)?, title: r.get(1)?, subtitle: r.get(2)?, plays: r.get(3)?, ms: r.get(4)? })
    })
    .map(|it| it.flatten().collect())
    .unwrap_or_default()
}

/// (plays, listening ms) since `since`.
pub fn totals(since: i64) -> (i64, i64) {
    conn()
        .query_row("SELECT COUNT(*), COALESCE(SUM(ms), 0) FROM plays WHERE at >= ?1", params![since], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((0, 0))
}

/// Most played track uris (Home "Most played" shelf, artist fallback).
pub fn most_played_uris(artist_uri: Option<&str>, limit: usize) -> Vec<(String, String, String, i64)> {
    let c = conn();
    let sql = "SELECT uri, MAX(title), MAX(artist), COUNT(*) FROM plays WHERE (?1 = '' OR artist_uri = ?1)
               GROUP BY uri ORDER BY COUNT(*) DESC LIMIT ?2";
    let Ok(mut st) = c.prepare(sql) else { return Vec::new() };
    st.query_map(params![artist_uri.unwrap_or(""), limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
}

pub fn clear_plays() {
    let _ = conn().execute("DELETE FROM plays", []);
}

// ---------------------------------------------------------------------------------------------
// Search history.

pub fn history_add(q: &str) {
    let c = conn();
    let _ = c.execute("INSERT OR REPLACE INTO search_history (query, at) VALUES (?1, ?2)", params![q, now()]);
    let _ = c.execute("DELETE FROM search_history WHERE query NOT IN (SELECT query FROM search_history ORDER BY at DESC LIMIT 20)", []);
}

pub fn history() -> Vec<String> {
    let c = conn();
    let Ok(mut st) = c.prepare("SELECT query FROM search_history ORDER BY at DESC LIMIT 20") else { return Vec::new() };
    st.query_map([], |r| r.get(0)).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub fn history_remove(q: &str) {
    let _ = conn().execute("DELETE FROM search_history WHERE query = ?1", params![q]);
}

pub fn history_clear() {
    let _ = conn().execute("DELETE FROM search_history", []);
}

// ---------------------------------------------------------------------------------------------
// Local files.

#[derive(Clone, Debug, Default)]
pub struct LocalTrack {
    pub path: String,
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: i64,
    pub mtime: i64,
    pub size: i64,
    pub missing: bool,
    pub error: String,
}

pub fn local_upsert(t: &LocalTrack) {
    let _ = conn().execute(
        "INSERT OR REPLACE INTO local_tracks (path, uri, title, artist, album, duration_ms, mtime, size, missing, error)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![t.path, t.uri, t.title, t.artist, t.album, t.duration_ms, t.mtime, t.size, t.missing, t.error],
    );
}

fn local_row(r: &rusqlite::Row) -> rusqlite::Result<LocalTrack> {
    Ok(LocalTrack {
        path: r.get(0)?,
        uri: r.get(1)?,
        title: r.get(2)?,
        artist: r.get(3)?,
        album: r.get(4)?,
        duration_ms: r.get(5)?,
        mtime: r.get(6)?,
        size: r.get(7)?,
        missing: r.get(8)?,
        error: r.get(9)?,
    })
}

pub fn local_all() -> Vec<LocalTrack> {
    let c = conn();
    let Ok(mut st) = c.prepare(
        "SELECT path, uri, title, artist, album, duration_ms, mtime, size, missing, error FROM local_tracks
         ORDER BY artist COLLATE NOCASE, album COLLATE NOCASE, path",
    ) else {
        return Vec::new();
    };
    st.query_map([], local_row).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub fn local_get(path: &str) -> Option<LocalTrack> {
    conn()
        .query_row(
            "SELECT path, uri, title, artist, album, duration_ms, mtime, size, missing, error FROM local_tracks WHERE path = ?1",
            params![path],
            local_row,
        )
        .optional()
        .ok()
        .flatten()
}

pub fn local_by_uri(uri: &str) -> Option<LocalTrack> {
    conn()
        .query_row(
            "SELECT path, uri, title, artist, album, duration_ms, mtime, size, missing, error FROM local_tracks WHERE uri = ?1 AND missing = 0",
            params![uri],
            local_row,
        )
        .optional()
        .ok()
        .flatten()
}

pub fn local_mark_missing(path: &str, missing: bool) {
    let _ = conn().execute("UPDATE local_tracks SET missing = ?2 WHERE path = ?1", params![path, missing]);
}

/// Forgets files under folders that are no longer configured.
pub fn local_retain_roots(roots: &[String]) {
    for t in local_all() {
        if !roots.iter().any(|r| t.path.starts_with(r.as_str())) {
            let _ = conn().execute("DELETE FROM local_tracks WHERE path = ?1", params![t.path]);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Episode progress, lyrics offsets.

pub fn episode_progress(uri: &str) -> Option<(i64, i64, bool)> {
    conn()
        .query_row("SELECT position_ms, duration_ms, played FROM episode_progress WHERE uri = ?1", params![uri], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()
        .ok()
        .flatten()
}

pub fn set_episode_progress(uri: &str, position_ms: i64, duration_ms: i64, played: bool) {
    let _ = conn().execute(
        "INSERT OR REPLACE INTO episode_progress (uri, position_ms, duration_ms, played, updated) VALUES (?1,?2,?3,?4,?5)",
        params![uri, position_ms, duration_ms, played, now()],
    );
}

pub fn lyrics_offset(uri: &str) -> i64 {
    conn().query_row("SELECT ms FROM lyrics_offset WHERE uri = ?1", params![uri], |r| r.get(0)).optional().ok().flatten().unwrap_or(0)
}

pub fn set_lyrics_offset(uri: &str, ms: i64) {
    let c = conn();
    if ms == 0 {
        let _ = c.execute("DELETE FROM lyrics_offset WHERE uri = ?1", params![uri]);
    } else {
        let _ = c.execute("INSERT OR REPLACE INTO lyrics_offset (uri, ms) VALUES (?1, ?2)", params![uri, ms]);
    }
}

// ---------------------------------------------------------------------------------------------
// Sidebar pins and Cadence-local folders.

pub fn pins() -> Vec<String> {
    let c = conn();
    let Ok(mut st) = c.prepare("SELECT uri FROM pins ORDER BY position") else { return Vec::new() };
    st.query_map([], |r| r.get(0)).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub fn set_pinned(uri: &str, pinned: bool) {
    let c = conn();
    if pinned {
        let _ = c.execute("INSERT OR REPLACE INTO pins (uri, position) VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM pins))", params![uri]);
    } else {
        let _ = c.execute("DELETE FROM pins WHERE uri = ?1", params![uri]);
    }
}

pub fn folders() -> Vec<(i64, String)> {
    let c = conn();
    let Ok(mut st) = c.prepare("SELECT id, name FROM folders ORDER BY position, id") else { return Vec::new() };
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub fn folder_create(name: &str) -> i64 {
    let c = conn();
    let _ = c.execute("INSERT INTO folders (name, position) VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM folders))", params![name]);
    c.last_insert_rowid()
}

pub fn folder_rename(id: i64, name: &str) {
    let _ = conn().execute("UPDATE folders SET name = ?2 WHERE id = ?1", params![id, name]);
}

pub fn folder_delete(id: i64) {
    let c = conn();
    let _ = c.execute("DELETE FROM folder_items WHERE folder_id = ?1", params![id]);
    let _ = c.execute("DELETE FROM folders WHERE id = ?1", params![id]);
}

/// Moves a playlist into a folder (`None` = out of any folder).
pub fn folder_put(uri: &str, folder: Option<i64>) {
    let c = conn();
    let _ = c.execute("DELETE FROM folder_items WHERE uri = ?1", params![uri]);
    if let Some(id) = folder {
        let _ = c.execute(
            "INSERT INTO folder_items (folder_id, uri, position) VALUES (?1, ?2, (SELECT COALESCE(MAX(position), 0) + 1 FROM folder_items WHERE folder_id = ?1))",
            params![id, uri],
        );
    }
}

/// uri -> folder id for every filed playlist.
pub fn folder_items() -> Vec<(String, i64)> {
    let c = conn();
    let Ok(mut st) = c.prepare("SELECT uri, folder_id FROM folder_items ORDER BY position") else { return Vec::new() };
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_and_stats_round_trip() {
        // Uses the real db path under a temp APPDATA.
        let dir = std::env::temp_dir().join(format!("cadence-db-test-{}", std::process::id()));
        unsafe { std::env::set_var("APPDATA", &dir) };
        let item = LibItem { uri: "spotify:track:a".into(), kind: "track".into(), title: "Song".into(), artist: "X".into(), artist_uri: "spotify:artist:x".into(), ..Default::default() };
        library_replace("track", std::slice::from_ref(&item));
        assert!(library_contains("spotify:track:a"));
        assert_eq!(library_list("track", "son", "alpha", 10).len(), 1);
        assert_eq!(liked_by_artist("spotify:artist:x").len(), 1);
        record_play(&Play { uri: "spotify:track:a", title: "Song", artist: "X", artist_uri: "", album: "Al", album_uri: "", ms: 60_000 });
        assert_eq!(top("songs", 0, 5)[0].plays, 1);
        assert_eq!(totals(0), (1, 60_000));
        history_add("abc");
        assert_eq!(history(), vec!["abc".to_string()]);
        set_lyrics_offset("spotify:track:a", -300);
        assert_eq!(lyrics_offset("spotify:track:a"), -300);
        let f = folder_create("Mix");
        folder_put("spotify:playlist:p", Some(f));
        assert_eq!(folder_items(), vec![("spotify:playlist:p".to_string(), f)]);
        clear_plays();
        assert_eq!(totals(0).0, 0);
    }
}

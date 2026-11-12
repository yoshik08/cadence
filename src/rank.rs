//! Search ranking: decides the Songs order, the Top result and which artists to show.
//!
//! Score = popularity signal + capped text-match boost + personal boost - version penalty.
//!
//! What Spotify sends shapes the signals (probe in docs/v1.5-part3-notes.md): this app's search
//! responses carry no `popularity` and no artist `followers`, so the popularity signal is the
//! API's own result position (earlier is better). If a popularity value ever arrives, it is used
//! instead; nothing is invented when it is missing.
//!
//! Pure functions over plain data: the caller gathers the personal signals (Liked Songs, play
//! history, followed artists) from the local database.

use std::collections::HashSet;

/// One song from the search response, in API order.
#[derive(Clone, Debug, Default)]
pub struct Song {
    pub uri: String,
    pub title: String,
    /// Artist uris, primary first.
    pub artist_uris: Vec<String>,
    /// "album" | "single" | "compilation".
    pub album_type: String,
    pub popularity: Option<u8>,
}

/// One artist from the search response, in API order.
#[derive(Clone, Debug, Default)]
pub struct Artist {
    pub uri: String,
    pub name: String,
    pub popularity: Option<u8>,
    pub followers: Option<u64>,
}

/// One album from the search response, in API order.
#[derive(Clone, Debug, Default)]
pub struct Album {
    pub title: String,
    pub artist_uris: Vec<String>,
}

/// Local, free signals about the user.
#[derive(Clone, Debug, Default)]
pub struct Personal {
    /// Liked Songs.
    pub liked: HashSet<String>,
    /// Followed artists.
    pub followed: HashSet<String>,
}

impl Personal {
    fn knows_artist(&self, uri: &str) -> bool {
        self.followed.contains(uri)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Top {
    /// Index into the ranked songs.
    Song(usize),
    /// Index into the artists given to `rank`.
    Artist(usize),
    /// Index into the albums given to `rank`.
    Album(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    /// Indices into the input songs, best first, duplicates removed.
    pub songs: Vec<usize>,
    pub top: Option<Top>,
    /// Indices into the input artists to show, in a sensible order.
    pub artists: Vec<usize>,
}

// Weights. The text boost is capped well below the gap between neighbouring popularity ranks
// it could otherwise overturn: one position is worth 8 points, the whole boost at most 12.
const POSITION_TOP: f32 = 100.0;
const POSITION_STEP: f32 = 8.0;
const TEXT_EXACT: f32 = 12.0;
const TEXT_PREFIX: f32 = 8.0;
const TEXT_WORDS: f32 = 5.0;
const LIKED: f32 = 20.0;
const ARTIST_KNOWN: f32 = 10.0;
const VERSION_PENALTY: f32 = 30.0;
const COMPILATION_PENALTY: f32 = 6.0;
/// With popularity present: results this far below the best song, with popularity under the
/// floor, are dropped from the Songs list.
const POPULARITY_FLOOR: u8 = 5;
const POPULARITY_GAP: u8 = 30;
/// How many top songs decide whose artist page is the clear winner.
const TOP_SONGS: usize = 5;

/// Words that mark a re-release or derivative version.
const VERSION_WORDS: [&str; 14] = [
    "sped up", "speed up", "slowed", "reverb", "live", "remix", "instrumental", "karaoke", "cover", "nightcore", "8d", "acoustic", "acapella", "edit",
];

/// Lowercase, punctuation to spaces, single spaces. "Boy With Luv (feat. Halsey)" ->
/// "boy with luv feat halsey".
pub fn normalize(s: &str) -> String {
    let mapped: String = s.chars().map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' }).collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The title without anything in brackets or after " - ": the song itself.
fn base_title(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let cut = out.find(" - ").map_or(out.as_str(), |i| &out[..i]);
    normalize(cut)
}

fn has_word(hay: &str, word: &str) -> bool {
    format!(" {hay} ").contains(&format!(" {word} "))
}

/// The version words in `title` that the query does not ask for.
fn version_marks(title: &str, query: &str) -> usize {
    let (t, q) = (normalize(title), normalize(query));
    VERSION_WORDS.iter().filter(|w| has_word(&t, w) && !has_word(&q, w)).count()
}

/// Text boost of `name` for `query`: exact, prefix, or all query words as whole words.
fn text_boost(name: &str, query: &str) -> f32 {
    let (n, q) = (normalize(name), normalize(query));
    let base = base_title(name);
    if q.is_empty() {
        0.0
    } else if n == q || base == q {
        TEXT_EXACT
    } else if n.starts_with(&format!("{q} ")) || base.starts_with(&format!("{q} ")) {
        TEXT_PREFIX
    } else if q.split(' ').all(|w| has_word(&n, w)) {
        TEXT_WORDS
    } else {
        0.0
    }
}

fn popularity_signal(popularity: Option<u8>, position: usize) -> f32 {
    match popularity {
        Some(p) => p as f32,
        None => (POSITION_TOP - POSITION_STEP * position as f32).max(0.0),
    }
}

fn song_score(s: &Song, position: usize, query: &str, me: &Personal) -> f32 {
    let mut score = popularity_signal(s.popularity, position) + text_boost(&s.title, query);
    if me.liked.contains(&s.uri) {
        score += LIKED;
    }
    if s.artist_uris.iter().any(|a| me.knows_artist(a)) {
        score += ARTIST_KNOWN;
    }
    score -= VERSION_PENALTY * version_marks(&s.title, query) as f32;
    if s.album_type == "compilation" {
        score -= COMPILATION_PENALTY;
    }
    score
}

/// Which copy of a song to keep: the original over versions, the album release over a single
/// over a compilation; then the better score.
fn preference(s: &Song, query: &str) -> (usize, u8) {
    let kind = match s.album_type.as_str() {
        "album" => 0,
        "single" => 1,
        _ => 2,
    };
    (version_marks(&s.title, query), kind)
}

pub fn rank(query: &str, songs: &[Song], artists: &[Artist], albums: &[Album], me: &Personal) -> Ranked {
    let scores: Vec<f32> = songs.iter().enumerate().map(|(i, s)| song_score(s, i, query, me)).collect();

    // Collapse duplicates: same base title and primary artist; keep the preferred copy.
    let mut keep: Vec<usize> = Vec::new();
    for (i, s) in songs.iter().enumerate() {
        let key = (base_title(&s.title), s.artist_uris.first().cloned().unwrap_or_default());
        match keep.iter().position(|&k| (base_title(&songs[k].title), songs[k].artist_uris.first().cloned().unwrap_or_default()) == key) {
            Some(slot) => {
                let k = keep[slot];
                let better = (preference(s, query), -scores[i]) < (preference(&songs[k], query), -scores[k]);
                if better {
                    keep[slot] = i;
                }
            }
            None => keep.push(i),
        }
    }
    keep.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));

    // With popularity: drop near-zero results that sit below clearly popular ones.
    if let Some(best) = keep.first().and_then(|&k| songs[k].popularity) {
        keep.retain(|&k| songs[k].popularity.is_none_or(|p| p >= POPULARITY_FLOOR || best < p.saturating_add(POPULARITY_GAP)));
    }

    let q = normalize(query);
    let top_songs: Vec<&Song> = keep.iter().take(TOP_SONGS).map(|&k| &songs[k]).collect();
    let primary_count = |uri: &str| top_songs.iter().filter(|s| s.artist_uris.first().is_some_and(|a| a == uri)).count();
    let appears = |uri: &str| top_songs.iter().any(|s| s.artist_uris.iter().any(|a| a == uri));

    // Top result: the best song, unless an artist (or album) is clearly the stronger result.
    let artist_wins = |a: &Artist| {
        if normalize(&a.name) != q {
            return false;
        }
        let popular = match (a.popularity, keep.first().and_then(|&k| songs[k].popularity)) {
            (Some(ap), Some(sp)) => ap >= sp,
            (Some(ap), None) => ap >= 50,
            _ => false,
        };
        popular || a.followers.is_some_and(|f| f >= 100_000) || me.knows_artist(&a.uri) || primary_count(&a.uri) >= 2
    };
    let top = if let Some(i) = artists.iter().position(artist_wins) {
        Some(Top::Artist(i))
    } else if let Some(i) = albums.iter().position(|al| normalize(&al.title) == q && al.artist_uris.iter().any(|a| me.knows_artist(a))) {
        Some(Top::Album(i))
    } else {
        keep.first().map(|_| Top::Song(0))
    };

    // Artists: those behind the top songs, plus close name matches the user knows (or that are
    // popular, when Spotify says so). Loose matches nobody plays are left out.
    let close = |a: &Artist| {
        let n = normalize(&a.name);
        n == q || n.starts_with(&format!("{q} "))
    };
    let notable = |a: &Artist| me.knows_artist(&a.uri) || a.popularity.is_some_and(|p| p >= 40) || a.followers.is_some_and(|f| f >= 50_000);
    let mut shown: Vec<usize> = Vec::new();
    for s in &top_songs {
        for uri in &s.artist_uris {
            if let Some(i) = artists.iter().position(|a| &a.uri == uri)
                && !shown.contains(&i)
            {
                shown.push(i);
            }
        }
    }
    for (i, a) in artists.iter().enumerate() {
        if !shown.contains(&i) && close(a) && (notable(a) || appears(&a.uri)) {
            shown.push(i);
        }
    }

    Ranked { songs: keep, top, artists: shown }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(uri: &str, title: &str, artist: &str, album_type: &str) -> Song {
        Song { uri: uri.into(), title: title.into(), artist_uris: vec![artist.into()], album_type: album_type.into(), popularity: None }
    }

    fn artist(uri: &str, name: &str) -> Artist {
        Artist { uri: uri.into(), name: name.into(), ..Default::default() }
    }

    fn titles(r: &Ranked, songs: &[Song]) -> Vec<String> {
        r.songs.iter().map(|&i| songs[i].title.clone()).collect()
    }

    #[test]
    fn normalizes_and_strips_versions() {
        assert_eq!(normalize("Boy With Luv (feat. Halsey)"), "boy with luv feat halsey");
        assert_eq!(base_title("Boy With Luv (feat. Halsey)"), "boy with luv");
        assert_eq!(base_title("Treat You Better - Sped Up"), "treat you better");
        assert_eq!(version_marks("Treat You Better (Sped Up)", "treat you better"), 1);
        assert_eq!(version_marks("Treat You Better (Sped Up)", "treat you better sped up"), 0);
    }

    #[test]
    fn popular_song_beats_obscure_exact_name_match_and_namesake_artist() {
        // "1 of 1": Spotify lists the hit first; an obscure exact-title song and an artist
        // literally named "1 OF 1" must not take over.
        let songs = [song("t:hit", "1 of 1 (TikTok)", "a:star", "single"), song("t:obscure", "1 of 1", "a:nobody", "album")];
        let artists = [artist("a:1of1", "1 OF 1"), artist("a:star", "Star")];
        let r = rank("1 of 1", &songs, &artists, &[], &Personal::default());
        assert_eq!(titles(&r, &songs)[0], "1 of 1 (TikTok)");
        assert_eq!(r.top, Some(Top::Song(0)));
        assert!(!r.artists.contains(&0), "the namesake artist is not shown");
        assert!(r.artists.contains(&1), "the hit's artist is shown");
    }

    #[test]
    fn song_beats_artist_named_like_it() {
        // "boy with luv": the BTS song wins the Top result over an artist called Boy With Luv.
        let songs = [song("t:bwl", "Boy With Luv (feat. Halsey)", "a:bts", "album"), song("t:x", "Luv", "a:other", "single")];
        let artists = [artist("a:boywithluv", "Boy With Luv"), artist("a:bts", "BTS")];
        let r = rank("boy with luv", &songs, &artists, &[], &Personal::default());
        assert_eq!(r.top, Some(Top::Song(0)));
        assert_eq!(songs[r.songs[0]].uri, "t:bwl");
        assert_eq!(r.artists, vec![1], "only BTS, not the namesake");
    }

    #[test]
    fn artist_wins_when_clearly_strongest() {
        // Searching an artist's name: their songs fill the top, so the artist is the Top result.
        let songs = [song("t:1", "Anti-Hero", "a:ts", "album"), song("t:2", "Cruel Summer", "a:ts", "album"), song("t:3", "Love Story", "a:ts", "album")];
        let artists = [artist("a:ts", "Taylor Swift")];
        let r = rank("taylor swift", &songs, &artists, &[], &Personal::default());
        assert_eq!(r.top, Some(Top::Artist(0)));
        // A followed namesake also wins.
        let mut me = Personal::default();
        me.followed.insert("a:1of1".into());
        let r = rank("1 of 1", &[song("t:hit", "Hit", "a:star", "single")], &[artist("a:1of1", "1 OF 1")], &[], &me);
        assert_eq!(r.top, Some(Top::Artist(0)));
    }

    #[test]
    fn duplicates_collapse_to_the_original() {
        let songs = [
            song("t:single", "Treat You Better", "a:sm", "single"),
            song("t:album", "Treat You Better", "a:sm", "album"),
            song("t:comp", "Treat You Better", "a:sm", "compilation"),
        ];
        let r = rank("treat you better", &songs, &[], &[], &Personal::default());
        assert_eq!(r.songs.len(), 1);
        assert_eq!(songs[r.songs[0]].uri, "t:album");
    }

    #[test]
    fn sped_up_and_slowed_versions_rank_below_the_original() {
        // Spotify sometimes lists the sped up upload first.
        let songs = [
            song("t:sped", "Treat You Better (Sped Up)", "a:uploader", "single"),
            song("t:slow", "treat you better - slowed + reverb", "a:uploader2", "single"),
            song("t:orig", "Treat You Better", "a:sm", "album"),
        ];
        let r = rank("treat you better", &songs, &[], &[], &Personal::default());
        assert_eq!(songs[r.songs[0]].uri, "t:orig");
        // Asked for by name, the sped up version is fine at the top.
        let r = rank("treat you better sped up", &songs, &[], &[], &Personal::default());
        assert_eq!(songs[r.songs[0]].uri, "t:sped");
    }

    #[test]
    fn personal_signals_lift_a_result() {
        let songs = [song("t:a", "Song", "a:x", "album"), song("t:b", "Song", "a:y", "album")];
        let mut me = Personal::default();
        me.liked.insert("t:b".into());
        let r = rank("song", &songs, &[], &[], &me);
        assert_eq!(songs[r.songs[0]].uri, "t:b");
    }

    #[test]
    fn text_boost_cannot_lift_obscure_over_popular() {
        // With popularity present, an exact title at 3 stays below a close match at 80.
        let mut songs = [song("t:pop", "Hello Again", "a:1", "album"), song("t:exact", "Hello", "a:2", "album")];
        songs[0].popularity = Some(80);
        songs[1].popularity = Some(3);
        let r = rank("hello", &songs, &[], &[], &Personal::default());
        assert_eq!(songs[r.songs[0]].uri, "t:pop");
        assert!(!r.songs.contains(&1), "near-zero result below a popular one is dropped");
    }

    #[test]
    fn loose_obscure_artists_are_left_out() {
        let songs = [song("t:1", "Midnight City", "a:m83", "album")];
        let artists = [artist("a:m83", "M83"), artist("a:mc", "Midnight City Boys"), artist("a:loose", "The Midnight")];
        let r = rank("midnight city", &songs, &artists, &[], &Personal::default());
        assert_eq!(r.artists, vec![0]);
    }
}

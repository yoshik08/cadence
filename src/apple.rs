//! Apple Music syllable lyrics for the karaoke stage (port of fetchApple() and _apple-ttml.js
//! from yoshik.xyz/api). Compiled only with the `karaoke` feature, which public releases leave
//! off. Needs a Media-User-Token from `APPLE_MEDIA_USER_TOKEN` or
//! `%APPDATA%\cadence\apple_media_token.txt` (local config, never in the repo).

use crate::player::{LyricLine, Lyrics};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

static DEV_TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn media_user_token() -> Option<String> {
    let clean = |s: &str| {
        let s = s.trim_start_matches('\u{feff}').trim().trim_matches('"');
        (!s.is_empty()).then(|| s.to_owned())
    };
    std::env::var("APPLE_MEDIA_USER_TOKEN")
        .ok()
        .and_then(|v| clean(&v))
        .or_else(|| clean(&std::fs::read_to_string(crate::auth::data_dir().join("apple_media_token.txt")).ok()?))
}

fn client() -> &'static reqwest::Client {
    crate::http::client()
}

/// The web player's developer token, scraped from its JS bundle (cached 12 h).
async fn dev_token() -> Option<String> {
    if let Some((t, at)) = DEV_TOKEN.lock().ok()?.clone() {
        if at.elapsed() < Duration::from_secs(12 * 3600) {
            return Some(t);
        }
    }
    let home = client().get("https://music.apple.com/in/home").header("User-Agent", UA).send().await.ok()?;
    let html = home.text().await.ok()?;
    let start = html.find("/assets/index")?;
    let rest = &html[start..];
    let end = rest.find(".js")? + 3;
    let js = client()
        .get(format!("https://music.apple.com{}", &rest[..end]))
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let mut from = 0;
    while let Some(i) = js[from..].find("eyJ") {
        let s = &js[from + i..];
        let len = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')).unwrap_or(s.len());
        let tok = &s[..len];
        if tok.matches('.').count() == 2 && tok.len() > 100 {
            *DEV_TOKEN.lock().ok()? = Some((tok.to_string(), Instant::now()));
            return Some(tok.to_string());
        }
        from += i + 3;
    }
    None
}

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Word-timed lyrics from Apple Music, or None (no token, no match, no syllable lyrics).
pub async fn word_lyrics(isrc: Option<String>, title: &str, artist: &str, duration_ms: u32) -> Option<Lyrics> {
    let mut_tok = media_user_token()?;
    let dev = dev_token().await?;
    let get = |url: String| {
        client()
            .get(url)
            .header("Authorization", format!("Bearer {dev}"))
            .header("Media-User-Token", mut_tok.as_str())
            .header("Origin", "https://music.apple.com")
            .header("Referer", "https://music.apple.com/")
            .header("User-Agent", UA)
            .header("Accept", "application/json")
            .send()
    };
    let primary = artist.split(',').next().unwrap_or(artist).trim();
    for sf in ["in", "us"] {
        let url = match &isrc {
            Some(i) => format!("https://amp-api.music.apple.com/v1/catalog/{sf}/songs?filter[isrc]={i}"),
            None => {
                let term: String = format!("{title} {primary}");
                let term = url_encode(&term);
                format!("https://amp-api.music.apple.com/v1/catalog/{sf}/search?term={term}&types=songs&limit=10")
            }
        };
        let r = get(url).await.ok()?;
        if r.status().as_u16() == 401 || r.status().as_u16() == 403 {
            eprintln!("apple: auth failed ({})", r.status());
            if let Ok(mut t) = DEV_TOKEN.lock() {
                *t = None;
            }
            return None;
        }
        if !r.status().is_success() {
            continue;
        }
        let d: Value = r.json().await.ok()?;
        let songs = if isrc.is_some() { &d["data"] } else { &d["results"]["songs"]["data"] };
        let (tn, an) = (norm(title), norm(primary));
        let mut matches: Vec<&Value> = songs
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        matches.retain(|s| {
            let a = &s["attributes"];
            if isrc.is_none() {
                let st = norm(a["name"].as_str().unwrap_or(""));
                let sa = norm(a["artistName"].as_str().unwrap_or(""));
                if !(st.contains(&tn) || tn.contains(&st)) || !(sa.contains(&an) || an.contains(&sa)) {
                    return false;
                }
            }
            let dur = a["durationInMillis"].as_u64().unwrap_or(0) as i64;
            duration_ms == 0 || (dur - duration_ms as i64).abs() <= 3000
        });
        // Explicit versions first (same order as the site).
        matches.sort_by_key(|s| (s["attributes"]["contentRating"].as_str() != Some("explicit")) as u8);
        for song in matches.iter().take(3) {
            let Some(id) = song["id"].as_str() else { continue };
            let url = format!("https://amp-api.music.apple.com/v1/catalog/{sf}/songs/{id}/syllable-lyrics?extend=ttmlLocalizations");
            let Ok(r) = get(url).await else { continue };
            if !r.status().is_success() {
                continue;
            }
            let Ok(d) = r.json::<Value>().await else { continue };
            let attrs = &d["data"][0]["attributes"];
            let ttml = attrs["ttmlLocalizations"].as_str().or(attrs["ttml"].as_str()).unwrap_or("");
            if let Some(lines) = parse_ttml_words(ttml).filter(|l| !l.is_empty()) {
                return Some(Lyrics { synced: true, word_sync: true, lines });
            }
        }
        if !matches.is_empty() {
            break;
        }
    }
    None
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn ttml_time(s: &str) -> u32 {
    if let Some(sec) = s.strip_suffix('s') {
        return (sec.parse::<f64>().unwrap_or(0.0) * 1000.0).round() as u32;
    }
    let parts: Vec<f64> = s.split(':').map(|p| p.parse().unwrap_or(0.0)).collect();
    let ms = match parts.as_slice() {
        [h, m, sec] => h * 3_600_000.0 + m * 60_000.0 + sec * 1000.0,
        [m, sec] => m * 60_000.0 + sec * 1000.0,
        [sec] => sec * 1000.0,
        _ => 0.0,
    };
    ms.round() as u32
}

fn decode_ent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find(';').filter(|&e| e <= 10);
        let ch = end.and_then(|e| {
            let ent = &rest[1..e];
            match ent.to_ascii_lowercase().as_str() {
                "amp" => Some('&'),
                "apos" => Some('\''),
                "quot" => Some('"'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "nbsp" => Some(' '),
                _ => {
                    let n = ent.strip_prefix('#')?;
                    let cp = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(cp)
                }
            }
        });
        match (ch, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = attrs[from..].find(name) {
        let at = from + i;
        let before_ok = at == 0 || attrs.as_bytes()[at - 1].is_ascii_whitespace();
        let after = attrs[at + name.len()..].trim_start();
        if before_ok && let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start().strip_prefix('"')?;
            return v.find('"').map(|e| &v[..e]);
        }
        from = at + name.len();
    }
    None
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Remove translation / romanisation / background-vocal spans, nested children included.
fn strip_role_spans(inner: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    let mut depth = 0;
    let mut last = 0;
    while let Some(off) = inner[i..].find('<') {
        let at = i + off;
        let Some(close) = inner[at..].find('>') else { break };
        let tag = &inner[at..at + close + 1];
        i = at + close + 1;
        let is_open = tag.starts_with("<span") && !tag.ends_with("/>");
        let is_close = tag.starts_with("</span");
        if depth == 0 {
            if is_open && (tag.contains("x-translation") || tag.contains("x-roman") || tag.contains("x-bg")) && tag.contains("ttm:role") {
                out.push_str(&inner[last..at]);
                depth = 1;
            }
        } else if is_close {
            depth -= 1;
            if depth == 0 {
                last = i;
            }
        } else if is_open {
            depth += 1;
        }
    }
    if depth == 0 {
        out.push_str(&inner[last..]);
    }
    out
}

/// parseAppleTTML(xml, "word"): None when the document is not word-timed.
fn parse_ttml_words(xml: &str) -> Option<Vec<LyricLine>> {
    if !xml.contains("itunes:timing=\"Word\"") {
        return None;
    }
    let mut lines = Vec::new();
    let mut from = 0;
    while let Some(off) = xml[from..].find("<p") {
        let at = from + off;
        let after = &xml[at + 2..];
        if !after.starts_with([' ', '>', '\t', '\n']) {
            from = at + 2;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        let attrs = &after[..gt];
        let body_start = at + 2 + gt + 1;
        let Some(end) = xml[body_start..].find("</p>") else { break };
        let inner = strip_role_spans(&xml[body_start..body_start + end]);
        from = body_start + end + 4;
        let Some(begin) = attr(attrs, "begin") else { continue };
        let p_begin = ttml_time(begin);

        // Chunks: timed spans and the loose text between them (the spaces live there).
        struct Chunk {
            text: String,
            timed: Option<(u32, u32)>,
        }
        let mut chunks = Vec::new();
        let mut i = 0;
        let mut has_spans = false;
        while let Some(o) = inner[i..].find("<span") {
            let s = i + o;
            if s > i {
                chunks.push(Chunk { text: inner[i..s].to_string(), timed: None });
            }
            let Some(gt) = inner[s..].find('>') else { break };
            let sattrs = &inner[s + 5..s + gt];
            let cstart = s + gt + 1;
            let Some(ce) = inner[cstart..].find("</span>") else { break };
            let text = inner[cstart..cstart + ce].to_string();
            i = cstart + ce + 7;
            match attr(sattrs, "begin") {
                Some(b) => {
                    has_spans = true;
                    let st = ttml_time(b);
                    let en = attr(sattrs, "end").map_or(st, ttml_time);
                    chunks.push(Chunk { text, timed: Some((st, en)) });
                }
                None => chunks.push(Chunk { text, timed: None }),
            }
        }
        if i < inner.len() {
            chunks.push(Chunk { text: inner[i..].to_string(), timed: None });
        }
        if !has_spans {
            let t = decode_ent(strip_tags(&inner).trim());
            if !t.is_empty() {
                lines.push(LyricLine { start_ms: p_begin, text: t, words: Vec::new(), ends: Vec::new() });
            }
            continue;
        }

        let mut words: Vec<(String, u32, u32)> = Vec::new();
        let mut cur: Option<(String, u32, u32)> = None;
        let commit = |cur: &mut Option<(String, u32, u32)>, words: &mut Vec<(String, u32, u32)>| {
            if let Some((t, s, e)) = cur.take() {
                if !t.trim().is_empty() {
                    words.push((decode_ent(t.trim()), s, e));
                }
            }
        };
        for ch in chunks {
            let txt = strip_tags(&ch.text);
            let lead = txt.starts_with(char::is_whitespace);
            let trail = txt.ends_with(char::is_whitespace);
            match ch.timed {
                None => {
                    if lead {
                        commit(&mut cur, &mut words);
                    }
                    if let Some(c) = cur.as_mut() {
                        c.0.push_str(&txt);
                    }
                    if trail {
                        commit(&mut cur, &mut words);
                    }
                }
                Some((s, e)) => {
                    if lead {
                        commit(&mut cur, &mut words);
                    }
                    match cur.as_mut() {
                        Some(c) => {
                            c.0.push_str(&txt);
                            c.2 = c.2.max(e);
                        }
                        None => cur = Some((txt.clone(), s, e)),
                    }
                    if trail {
                        commit(&mut cur, &mut words);
                    }
                }
            }
        }
        commit(&mut cur, &mut words);
        if words.is_empty() {
            continue;
        }
        let text = words.iter().map(|w| w.0.as_str()).collect::<Vec<_>>().join(" ");
        let starts: Vec<u32> = words.iter().map(|w| w.1).collect();
        let mut ends: Vec<u32> = words.iter().map(|w| w.2).collect();
        for k in 0..ends.len() {
            if ends[k] <= starts[k] {
                ends[k] = starts.get(k + 1).copied().unwrap_or(starts[k] + 300);
            }
        }
        // A word containing inner spaces would break the 1:1 alignment with the text.
        if text.split_whitespace().count() != starts.len() {
            lines.push(LyricLine { start_ms: p_begin, text, words: Vec::new(), ends: Vec::new() });
        } else {
            lines.push(LyricLine { start_ms: p_begin, text, words: starts, ends });
        }
    }
    Some(lines)
}

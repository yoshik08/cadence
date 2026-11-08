//! The lyrics page (ui/lyrics.slint), ported from yoshik.xyz (lyrics.css, script.js, karaoke.js).
//!
//! - Lines are measured with the bundled font's real advances and wrapped here, so every line
//!   arrives in the UI with its rows, height and offset: the active line can be centred exactly,
//!   long lines wrap onto more rows and nothing is ever cut with "...".
//! - The playback clock lives here: position = last librespot position + monotonic time since.
//!   The classic view is updated exactly when the next letter or line is due; the karaoke stage
//!   (karaoke.js) runs once per rendered frame, paced by the GPU renderer's vsync (frame()), with
//!   a timer as the fallback on the software renderer. Paused, nothing runs at all.
//!
//! UI thread only.

use crate::player::Lyrics;
use crate::{KMotion, KWord, LyrLetter, LyrLine, LyrRow, LyrWord, MainWindow, Playback};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Font metrics (ttf-parser, already part of Slint's text stack).

struct Font {
    face: ttf_parser::Face<'static>,
    upem: f32,
}

impl Font {
    fn new(data: &'static [u8]) -> Font {
        let face = ttf_parser::Face::parse(data, 0).expect("bundled font");
        let upem = face.units_per_em() as f32;
        Font { face, upem }
    }
    /// Advance of `ch` in em.
    fn adv(&self, ch: char) -> f32 {
        self.face
            .glyph_index(ch)
            .and_then(|g| self.face.glyph_hor_advance(g))
            .map_or(0.55, |a| a as f32 / self.upem)
    }
    /// Width in px of `s` at `size` with letter-spacing `ls` em.
    fn width(&self, s: &str, size: f32, ls: f32) -> f32 {
        let (mut w, mut n) = (0.0, 0usize);
        for ch in s.chars() {
            w += self.adv(ch);
            n += 1;
        }
        (w + ls * n as f32) * size
    }
}

thread_local! {
    static LINE_FONT: Font = Font::new(include_bytes!("../assets/fonts/Bricolage-SemiCondensed-700.ttf"));
    static WORD_FONT: Font = Font::new(include_bytes!("../assets/fonts/Bricolage-SemiCondensed-800.ttf"));
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------------------------
// Classic view layout. Mirrors LyrGeo in ui/lyrics.slint.

struct Geometry {
    col_w: f32,
    size: f32,
    active: f32,
}

impl Geometry {
    fn new(w: f32) -> Geometry {
        let pad = w * 0.06;
        let side = (w * 0.38).min(540.0);
        Geometry {
            col_w: (w - (pad + side + pad) - pad).max(80.0),
            size: (w * 0.029).clamp(22.0, 34.0),
            active: (w * 0.04).clamp(30.0, 48.0),
        }
    }
}

/// Greedy word wrap of `text` into rows no wider than `max` px.
fn wrap(f: &Font, text: &str, size: f32, max: f32) -> Vec<String> {
    const LS: f32 = -0.015;
    let space = f.width(" ", size, LS);
    let mut rows = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0;
    for word in text.split_whitespace() {
        let ww = f.width(word, size, LS);
        if ww > max {
            // A single word wider than the column: break it by characters.
            if !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            }
            for ch in word.chars() {
                let cw = f.width(ch.encode_utf8(&mut [0; 4]), size, LS);
                if cur_w + cw > max && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0.0;
                }
                cur.push(ch);
                cur_w += cw;
            }
            continue;
        }
        if cur.is_empty() {
            cur.push_str(word);
            cur_w = ww;
        } else if cur_w + space + ww <= max {
            cur.push(' ');
            cur.push_str(word);
            cur_w += space + ww;
        } else {
            rows.push(std::mem::replace(&mut cur, word.to_string()));
            cur_w = ww;
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Per-letter rows for a word-timed line at the active size. Letter k of a word lights at
/// start + (next word's start - start) * k / n (script.js lyrTick), the last word ending at the
/// next line, or 8 s after the line starts.
fn word_rows(f: &Font, text: &str, starts: &[u32], line_end: u32, size: f32, max: f32) -> Vec<LyrRow> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() != starts.len() || words.is_empty() {
        return Vec::new();
    }
    // Rendered letter by letter, so measure without kerning: letters + the layout's spacing.
    let letter_w = |w: &str| {
        let n = w.chars().count() as f32;
        w.chars().map(|c| f.adv(c)).sum::<f32>() * size - 0.015 * size * (n - 1.0).max(0.0)
    };
    let space = f.adv(' ') * size;
    let mut rows: Vec<Vec<LyrWord>> = vec![Vec::new()];
    let mut used = 0.0f32;
    for (j, w) in words.iter().enumerate() {
        let s = starts[j];
        let mut e = starts.get(j + 1).copied().unwrap_or(line_end);
        if e <= s {
            e = s + 1;
        }
        let chars: Vec<char> = w.chars().collect();
        let n = chars.len() as u32;
        let letters: Vec<LyrLetter> = chars
            .iter()
            .enumerate()
            .map(|(k, ch)| LyrLetter {
                ch: ch.to_string().into(),
                at: (s + (e - s) * k as u32 / n.max(1)) as i32,
            })
            .collect();
        let ww = letter_w(w);
        let row = rows.last_mut().unwrap();
        if !row.is_empty() && used + space + ww > max {
            if let Some(last) = row.last_mut() {
                last.space = false;
            }
            rows.push(Vec::new());
            used = 0.0;
        }
        let row = rows.last_mut().unwrap();
        if !row.is_empty() {
            used += space;
        }
        used += ww;
        row.push(LyrWord { letters: ModelRc::new(VecModel::from(letters)), space: j + 1 < words.len() });
    }
    if let Some(last) = rows.last_mut().and_then(|r| r.last_mut()) {
        last.space = false;
    }
    rows.into_iter()
        .map(|r| LyrRow { words: ModelRc::new(VecModel::from(r)) })
        .collect()
}

fn strings(v: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v.into_iter().map(SharedString::from).collect::<Vec<_>>()))
}

/// The lines for window width `w`, plus the total inactive height.
fn build_lines(lyrics: &Lyrics, duration_ms: u32, w: f32) -> (Vec<LyrLine>, f32) {
    let g = Geometry::new(w);
    // 2% slack: rows are measured without kerning, Slint shapes with it.
    let max = g.col_w * 0.98;
    LINE_FONT.with(|f| {
        let mut out = Vec::with_capacity(lyrics.lines.len());
        let mut top = 0.0f32;
        for (i, l) in lyrics.lines.iter().enumerate() {
            let next = lyrics.lines.get(i + 1).map(|n| n.start_ms);
            let line_end = next.unwrap_or_else(|| (l.start_ms + 8000).min(duration_ms.max(l.start_ms + 1)));
            let rows = wrap(f, &l.text, g.size, max);
            let arows = wrap(f, &l.text, g.active, max);
            let wrows = if lyrics.synced && !l.words.is_empty() {
                word_rows(f, &l.text, &l.words, line_end, g.active, max)
            } else {
                Vec::new()
            };
            let h = 32.0 + rows.len() as f32 * g.size * 1.35;
            let an = if wrows.is_empty() { arows.len() } else { wrows.len() };
            let ah = 32.0 + an as f32 * g.active * 1.35;
            out.push(LyrLine {
                start_ms: l.start_ms as i32,
                rows: strings(rows),
                arows: strings(arows),
                wrows: ModelRc::new(VecModel::from(wrows)),
                top,
                h,
                ah,
            });
            top += h;
        }
        (out, top)
    })
}

// ---------------------------------------------------------------------------------------------
// Karaoke stage (karaoke.js).

const SLOTS: usize = 14;
const LOOK_AHEAD: i64 = 70;
const PERSPECTIVE: f32 = 1200.0;

#[derive(Clone, Copy, PartialEq)]
enum Look {
    Stack,
    Ship,
    Fisheye,
}

struct KW {
    text: SharedString,
    /// Highlight box tilt (deg), karaoke.js --tilt.
    tilt: f32,
    start: i64,
    end: i64,
    end_eff: i64,
    jx: f32,
    jr: f32,
    icon: Option<&'static str>,
    icon_left: bool,
    /// Text width at the base size (em), for the fit-to-width scale.
    width_em: f32,
}

#[derive(Default, Clone, Copy)]
struct L {
    x: f32,
    y: f32,
    z: f32,
    s: f32,
    o: f32,
    b: f32,
    /// rotateZ in degrees
    rz: f32,
}

/// A karaoke word rasterized once: white glyphs in alpha, sizes in logical px at the base em.
struct WordTex {
    img: slint::Image,
    iw: f32,
    ih: f32,
    ix: f32,
    iy: f32,
    tw: f32,
}

static WORD_FONT_DATA: &[u8] = include_bytes!("../assets/fonts/Bricolage-SemiCondensed-800.ttf");

thread_local! {
    static SCALER: RefCell<swash::scale::ScaleContext> = RefCell::new(swash::scale::ScaleContext::new());
}

/// Renders `text` at `em` logical px (rasterized at `em * sf` device px, capped so a long word
/// stays a few MB at most) with letter-spacing -.05em.
fn rasterize(text: &str, em: f32, sf: f32) -> Option<WordTex> {
    use swash::scale::{Render, Source};
    use swash::zeno::{Format, Vector};
    let font = swash::FontRef::from_index(WORD_FONT_DATA, 0)?;
    let px = (em * sf).clamp(8.0, 240.0);
    let k = px / em; // device px per logical px
    let cmap = font.charmap();
    let gm = font.glyph_metrics(&[]).scale(px);
    let m = font.metrics(&[]).scale(px);
    let (asc, desc) = (m.ascent, m.descent.abs());
    let ls = -0.05 * px;
    let mut pen = 0.0f32;
    let mut glyphs = Vec::with_capacity(text.len());
    for ch in text.chars() {
        let g = cmap.map(ch);
        glyphs.push((g, pen));
        pen += gm.advance_width(g) + ls;
    }
    let advance = (pen - ls).max(1.0);
    let margin = (0.06 * px).ceil();
    let (w, h) = ((advance + 2.0 * margin).ceil() as usize, (asc + desc + 2.0 * margin).ceil() as usize);
    let baseline = margin + asc;
    let mut alpha = vec![0u8; w * h];
    SCALER.with(|ctx| {
        let mut ctx = ctx.borrow_mut();
        let mut scaler = ctx.builder(font).size(px).hint(false).build();
        for (g, x) in glyphs {
            let ox = margin + x;
            let mut r = Render::new(&[Source::Outline]);
            r.format(Format::Alpha).offset(Vector::new(ox.fract(), baseline.fract()));
            let Some(img) = r.render(&mut scaler, g) else { continue };
            let p = img.placement;
            let (x0, y0) = (ox.floor() as i32 + p.left, baseline.floor() as i32 - p.top);
            for row in 0..p.height as i32 {
                let y = y0 + row;
                if y < 0 || y >= h as i32 {
                    continue;
                }
                for col in 0..p.width as i32 {
                    let x = x0 + col;
                    if x < 0 || x >= w as i32 {
                        continue;
                    }
                    let a = img.data[(row * p.width as i32 + col) as usize];
                    let d = &mut alpha[y as usize * w + x as usize];
                    *d = d.saturating_add(a);
                }
            }
        }
    });
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w as u32, h as u32);
    for (o, a) in buf.make_mut_slice().iter_mut().zip(alpha) {
        *o = slint::Rgba8Pixel { r: 255, g: 255, b: 255, a };
    }
    // Element box: padding .28em left, .1em top, line box 1em; glyph box centred in it.
    let (iw, ih) = (w as f32 / k, h as f32 / k);
    Some(WordTex {
        img: slint::Image::from_rgba8(buf),
        iw,
        ih,
        ix: 0.28 * em - margin / k,
        iy: 0.6 * em - (margin + (asc + desc) / 2.0) / k,
        tw: advance / k,
    })
}

fn tab(arr: &[f32], x: f32) -> f32 {
    let n = arr.len() - 1;
    if x >= n as f32 {
        return arr[n];
    }
    let i = x.floor().max(0.0) as usize;
    arr[i] + (arr[i + 1] - arr[i]) * (x - i as f32)
}

fn out_back(p: f32) -> f32 {
    let (c1, c3) = (1.9f32, 2.9f32);
    1.0 + c3 * (p - 1.0).powi(3) + c1 * (p - 1.0).powi(2)
}

fn rnd(i: usize, salt: f32) -> f32 {
    let x = ((i as f32) * 127.1 + salt * 311.7).sin() * 43758.547;
    x - x.floor()
}

struct Stage {
    w: f32,
    h: f32,
    u: f32,
    t: f32,
}

fn layout(look: Look, k: f32, w: &KW, fit: f32, c: &Stage) -> L {
    let ak = k.abs();
    let up = k < 0.0;
    let near = ak.min(1.0);
    let sign = if k < 0.0 { -1.0 } else if k > 0.0 { 1.0 } else { 0.0 };
    match look {
        Look::Stack => L {
            s: tab(if up { &[1.0, 0.5, 0.33, 0.24, 0.2] } else { &[1.0, 0.46, 0.3, 0.22, 0.2] }, ak) * fit,
            o: tab(if up { &[1.0, 0.62, 0.26, 0.06, 0.0] } else { &[1.0, 0.5, 0.2, 0.05, 0.0] }, ak),
            y: sign * (c.u * 0.17 * near + c.u * 0.1 * (ak - 1.0).max(0.0)),
            x: w.jx * c.w * 0.12 * (0.45 + 0.55 * near),
            z: -ak * c.u * 0.1,
            b: tab(&[0.0, 1.1, 2.3, 3.4, 4.0], ak),
            rz: w.jr * 2.5,
        },
        Look::Ship => L {
            s: tab(
                if up { &[1.0, 0.62, 0.52, 0.44, 0.38, 0.33, 0.29, 0.26, 0.24] } else { &[1.0, 0.46, 0.3, 0.2] },
                ak,
            ) * fit,
            o: tab(if up { &[1.0, 0.8, 0.6, 0.44, 0.32, 0.22, 0.13, 0.06, 0.0] } else { &[1.0, 0.45, 0.16, 0.0] }, ak),
            y: sign * (c.u * 0.15 * near + c.u * (if up { 0.085 } else { 0.1 }) * (ak - 1.0).max(0.0)),
            x: w.jx * c.w * 0.2 * (0.35 + 0.65 * near) + (c.t * 0.8 + w.jr * 10.0).sin() * c.w * 0.008 * ak,
            z: -ak * c.u * 0.2,
            b: tab(&[0.0, 0.6, 1.2, 1.9, 2.5, 3.0, 3.4, 3.8, 4.0], ak),
            rz: w.jr * 4.0,
        },
        Look::Fisheye => {
            let th = (k * 0.5).clamp(-1.45, 1.45);
            let ct = th.cos();
            L {
                s: (0.32 + 0.68 * ct.powi(5)) * (1.0 + 0.16 * (1.0 - ak).max(0.0)) * fit,
                o: ct.powf(3.2).clamp(0.0, 1.0) * if ak > 3.4 { (4.4 - ak).clamp(0.0, 1.0) } else { 1.0 },
                y: th.sin() * c.h * 0.43,
                x: w.jx * c.w * 0.07 * (0.4 + 0.6 * ct)
                    - th.abs().sin() * c.w * 0.05 * (if w.jx < 0.0 { -1.0 } else { 1.0 }) * 0.6,
                z: (ct - 0.55) * c.u * 0.34,
                b: if ak < 0.5 { 0.0 } else { (ak * 0.9).min(3.4) },
                rz: w.jr * 1.6,
            }
        }
    }
}

/// karaoke.js glyphs as Slint path commands (circles written as two arcs).
fn glyph(name: &str) -> &'static str {
    match name {
        "person" => "M8 8a4 4 0 1 0 8 0a4 4 0 1 0-8 0 M4 21c0-4.4 3.6-7 8-7s8 2.6 8 7",
        "eye" => "M2 12s3.8-7 10-7 10 7 10 7-3.8 7-10 7S2 12 2 12z M9 12a3 3 0 1 0 6 0a3 3 0 1 0-6 0",
        "bubble" => "M21 12c0 4.4-4 8-9 8-1.3 0-2.5-.2-3.6-.7L3 21l1.6-4.3C3.6 15.3 3 13.7 3 12c0-4.4 4-8 9-8s9 3.6 9 8z",
        "hash" => "M5 9h15 M4 15h15 M10 3L8 21 M16 3l-2 18",
        "heart" => "M12 21s-8-5.3-8-11a4.5 4.5 0 0 1 8-2.8A4.5 4.5 0 0 1 20 10c0 5.7-8 11-8 11z",
        "star" => "M12 3l2.7 5.6 6.1.9-4.4 4.3 1 6.1L12 17l-5.4 2.9 1-6.1L3.2 9.5l6.1-.9z",
        "note" => "M9 18V5l11-2v13 M3 18a3 3 0 1 0 6 0a3 3 0 1 0-6 0 M14 16a3 3 0 1 0 6 0a3 3 0 1 0-6 0",
        "clock" => "M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0 M12 7v5l3 2",
        "bolt" => "M13 2L4 14h7l-1 8 9-12h-7z",
        "moon" => "M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5z",
        "sun" => "M8 12a4 4 0 1 0 8 0a4 4 0 1 0-8 0 M12 2v2 M12 20v2 M2 12h2 M20 12h2 M5 5l1.5 1.5 M17.5 17.5L19 19 M19 5l-1.5 1.5 M6.5 17.5L5 19",
        "home" => "M3 11L12 3l9 8v10h-6v-6H9v6H3z",
        _ => "M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0 M14.5 9c-.4-1-1.4-1.5-2.5-1.5-1.5 0-2.6.8-2.6 2s1 1.7 2.6 2 2.6.8 2.6 2-1.1 2-2.6 2c-1.1 0-2.1-.5-2.5-1.5 M12 6v1.5 M12 16.5V18",
    }
}

fn word_icon(w: &str) -> Option<&'static str> {
    let norm: String = w
        .to_lowercase()
        .replace(['\u{2019}', '\u{2018}'], "'")
        .chars()
        .filter(|c| c.is_ascii_lowercase() || *c == '\'')
        .collect();
    if norm.is_empty() {
        return None;
    }
    let map = |w: &str| -> Option<&'static str> {
        Some(match w {
            "you" | "your" | "yours" | "u" | "ya" | "yourself" | "someone" | "somebody" | "anybody" => "person",
            "see" | "saw" | "look" | "looking" | "eyes" | "eye" | "watch" | "watching" | "seen" | "view" => "eye",
            "tell" | "telling" | "told" | "say" | "saying" | "said" | "talk" | "talking" | "speak" | "words" | "call"
            | "whisper" => "bubble",
            "number" | "numbers" | "one" | "two" | "three" | "four" | "five" | "count" | "hundred" | "thousand" => "hash",
            "love" | "loved" | "loving" | "heart" | "hearts" | "baby" | "darling" => "heart",
            "star" | "stars" | "shine" | "shining" => "star",
            "song" | "songs" | "music" | "sing" | "singing" | "melody" | "dance" | "dancing" => "note",
            "time" | "hour" | "hours" | "minute" | "forever" | "always" | "never" | "now" | "tonight" => "clock",
            "fire" | "lightning" | "electric" | "power" | "energy" | "thunder" => "bolt",
            "moon" | "night" | "midnight" | "dream" | "dreams" | "sleep" => "moon",
            "sun" | "sunshine" | "summer" | "morning" | "light" => "sun",
            "home" | "house" => "home",
            "money" | "cash" | "rich" | "dollar" | "dollars" => "dollar",
            _ => return None,
        })
    };
    let strip = |s: &str, suf: &str| s.strip_suffix(suf).unwrap_or(s).to_string();
    let tries = [
        norm.clone(),
        strip(&norm, "'s"),
        strip(&norm, "s"),
        strip(&norm, "ing"),
        strip(&strip(&norm, "'"), "in"),
    ];
    tries.iter().find_map(|t| map(t)).map(glyph)
}

struct Karaoke {
    words: Vec<KW>,
    look: Look,
    icons: bool,
    cursor: f32,
    last_cursor: f32,
    vel: f32,
    t: f32,
    hint: i64,
    slots: Rc<VecModel<KWord>>,
    motion: Rc<VecModel<KMotion>>,
    /// lyric, highlight, secondary
    colors: [slint::Color; 3],
    /// Word textures for the words on stage (by word index), all at `tex_em` / `tex_sf`.
    tex: std::collections::HashMap<usize, WordTex>,
    tex_em: f32,
    tex_sf: f32,
}

fn mix(a: slint::Color, b: slint::Color, t: f32) -> slint::Color {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    slint::Color::from_argb_u8(l(a.alpha(), b.alpha()), l(a.red(), b.red()), l(a.green(), b.green()), l(a.blue(), b.blue()))
}

impl Karaoke {
    fn new() -> Karaoke {
        Karaoke {
            words: Vec::new(),
            look: Look::Stack,
            icons: true,
            cursor: -0.9,
            last_cursor: -0.9,
            vel: 0.0,
            t: 0.0,
            hint: -1,
            slots: Rc::new(VecModel::from(vec![KWord::default(); SLOTS])),
            motion: Rc::new(VecModel::from(vec![KMotion::default(); SLOTS])),
            colors: [slint::Color::from_rgb_u8(255, 255, 255); 3],
            tex: std::collections::HashMap::new(),
            tex_em: 0.0,
            tex_sf: 0.0,
        }
    }

    fn set_style(&mut self, name: &str) {
        (self.look, self.icons) = match name {
            "ship" => (Look::Ship, false),
            "fisheye" => (Look::Fisheye, false),
            "fisheye-visual" => (Look::Fisheye, true),
            _ => (Look::Stack, true),
        };
    }

    fn set_words(&mut self, lyrics: Option<&Lyrics>) {
        self.words.clear();
        self.tex.clear();
        if let Some(l) = lyrics {
            WORD_FONT.with(|f| {
                for line in &l.lines {
                    let texts: Vec<&str> = line.text.split_whitespace().collect();
                    if texts.len() != line.words.len() {
                        continue;
                    }
                    for (j, t) in texts.iter().enumerate() {
                        let i = self.words.len();
                        let start = line.words[j] as i64;
                        let end = line.ends.get(j).map_or(start + 300, |&e| e as i64).max(start + 60);
                        self.words.push(KW {
                            text: (*t).into(),
                            tilt: (rnd(i, 3.0) * 2.0 - 1.0) * 3.2,
                            start,
                            end,
                            end_eff: 0,
                            jx: rnd(i, 1.0) * 2.0 - 1.0,
                            jr: rnd(i, 2.0) * 2.0 - 1.0,
                            icon: None,
                            icon_left: rnd(i, 4.0) <= 0.5,
                            width_em: f.width(t, 1.0, -0.05) + 0.56,
                        });
                    }
                }
            });
        }
        let n = self.words.len();
        for i in 0..n {
            let next = self.words.get(i + 1).map_or(i64::MAX, |w| w.start);
            let w = &mut self.words[i];
            // The box belongs to one word at a time: it ends when the next word starts.
            w.end_eff = (w.start + 80).max((w.end + 140).min(next));
        }
        // Glyphs: at most one per 3 words, no repeat within the last 8.
        let (mut last, mut recent): (i64, Vec<&'static str>) = (-9, Vec::new());
        for i in 0..n {
            let Some(icon) = word_icon(&self.words[i].text) else { continue };
            if (i as i64) - last < 3 || recent.contains(&icon) {
                continue;
            }
            self.words[i].icon = Some(icon);
            last = i as i64;
            recent.push(icon);
            if recent.len() > 8 {
                recent.remove(0);
            }
        }
        self.hint = -1;
        self.cursor = -0.9;
        self.last_cursor = -0.9;
    }

    fn find(&mut self, pos: i64) -> i64 {
        let n = self.words.len() as i64;
        if n == 0 {
            return -1;
        }
        let mut i = self.hint.clamp(0, n - 1);
        if pos >= self.words[i as usize].start {
            while i + 1 < n && self.words[(i + 1) as usize].start <= pos {
                i += 1;
            }
            return i;
        }
        let i = self.words.partition_point(|w| w.start <= pos) as i64 - 1;
        i.max(-1)
    }

    fn update(&mut self, pos: i64, dt: f32, snap: bool, sw: f32, sh: f32, sf: f32) {
        if self.words.is_empty() {
            self.hide_all();
            return;
        }
        let dt = dt.clamp(1.0, 100.0);
        let a = self.find(pos + LOOK_AHEAD);
        self.hint = a;
        let mut tau = 120.0;
        let target = if a < 0 {
            -0.9
        } else {
            let cur = &self.words[a as usize];
            let mut target = a as f32;
            if let Some(nxt) = self.words.get(a as usize + 1) {
                let end_a = cur.end.min(nxt.start);
                let gap = nxt.start - end_a;
                tau = ((nxt.start - cur.start) as f32 * 0.2).clamp(30.0, 80.0);
                if gap > 260 {
                    // Lean toward the next word during a long gap.
                    let p = ((pos - (end_a + 120)) as f32 / (gap - 120) as f32).clamp(0.0, 1.0);
                    target += 0.28 * p * p;
                }
            }
            target
        };
        if snap || (target - self.cursor).abs() > 7.0 {
            self.cursor = target;
        } else {
            self.cursor += (target - self.cursor) * (1.0 - (-dt / tau).exp());
        }
        self.vel = (self.cursor - self.last_cursor) / dt * 1000.0;
        self.last_cursor = self.cursor;
        self.t += dt / 1000.0;
        self.render(pos, sw, sh, sf);
    }

    fn hide_all(&mut self) {
        self.tex.clear();
        for i in 0..SLOTS {
            if self.motion.row_data(i).is_some_and(|m| m.opacity > 0.0) {
                self.motion.set_row_data(i, KMotion::default());
            }
            if self.slots.row_data(i).is_some_and(|w| w.img.size().width > 0) {
                self.slots.set_row_data(i, KWord::default());
            }
        }
    }

    fn render(&mut self, pos: i64, sw: f32, sh: f32, sf: f32) {
        let c = Stage { w: sw, h: sh, u: sw.min(sh * 1.2), t: self.t };
        let fs = (c.u * 0.125).clamp(40.0, 200.0);
        // Re-rasterize only when the base size really changes (resize / DPI change).
        if (fs - self.tex_em).abs() > 0.5 || sf != self.tex_sf {
            self.tex.clear();
            self.tex_em = fs;
            self.tex_sf = sf;
        }
        let (prev, next) = match self.look {
            Look::Stack => (3.0, 3.0),
            Look::Ship => (8.0, 3.0),
            Look::Fisheye => (4.0, 4.0),
        };
        let n = self.words.len() as i64;
        let ci = self.cursor.round() as i64;
        let lo = (ci - prev as i64 - 1).max(0);
        let hi = (ci + next as i64 + 1).min(n - 1);
        let motion_blur = (self.vel.abs() * 0.7).min(2.6);
        let mut out: Vec<(usize, KWord, KMotion)> = Vec::with_capacity(SLOTS);
        for i in lo..=hi {
            let w = &self.words[i as usize];
            let kk = i as f32 - self.cursor;
            let ak = kk.abs();
            let beyond = (ak > prev + 0.2 && kk < 0.0) || (ak > next + 0.2 && kk > 0.0);
            let fit = (c.w * 0.86 / (w.width_em * fs)).min(1.0);
            let p = layout(self.look, kk, w, fit, &c);
            // No blur in the software renderer: blurred words read as a little dimmer instead.
            let blur = p.b * 0.6 + motion_blur * ak.min(1.5) * 0.5;
            // (No blur filter in Slint: blurred words read as a little dimmer instead.)
            let o = if beyond { 0.0 } else { p.o / (1.0 + blur * 0.12) };
            if o < 0.01 {
                continue;
            }
            // perspective: 1200px, origin 50% 46%
            let k = PERSPECTIVE / (PERSPECTIVE - p.z);
            let x = c.w / 2.0 + p.x * k;
            let y = c.h * 0.46 + (c.h * 0.04 + p.y) * k;
            let scale = p.s.max(0.02) * k;
            let i = i as usize;
            if !self.tex.contains_key(&i) {
                let Some(t) = rasterize(&self.words[i].text, fs, sf) else { continue };
                self.tex.insert(i, t);
            }
            let tex = &self.tex[&i];
            let w = &self.words[i];

            let (state, bp, ha) = if pos < w.start {
                (0, 0.0, 0.0)
            } else if pos < w.end_eff {
                (1, out_back(((pos - w.start) as f32 / 110.0).clamp(0.0, 1.0)), 1.0)
            } else {
                let q = ((pos - w.end_eff) as f32 / 80.0).clamp(0.0, 1.0);
                (2, 1.0 - q * 0.45, 1.0 - q)
            };
            let icon = if self.icons { w.icon.unwrap_or("") } else { "" };
            // .vk-t: upcoming lyric @36%, sung secondary (160 ms fade); being sung is white at once.
            let [lyric, _, secondary] = self.colors;
            let white = slint::Color::from_rgb_u8(255, 255, 255);
            let color = match state {
                0 => lyric.with_alpha(0.36),
                1 => white,
                // Quantized so the 160 ms fade uses a handful of recolours, not one per frame.
                _ => mix(white, secondary, (((pos - w.end_eff) as f32 / 160.0) * 8.0).round() / 8.0),
            };
            // Stable slot per word, so a slot's texture only changes when a new word arrives.
            out.push((
                i % SLOTS,
                KWord {
                    img: tex.img.clone(),
                    iw: tex.iw,
                    ih: tex.ih,
                    ix: tex.ix,
                    iy: tex.iy,
                    tw: tex.tw,
                    base: fs,
                    tilt: w.tilt,
                    color,
                    icon: icon.into(),
                    icon_left: w.icon_left,
                },
                KMotion {
                    x,
                    y,
                    scale,
                    rot: p.rz,
                    // Nearest on top.
                    z: -ak,
                    opacity: o,
                    hl: ha * o,
                    hl_sx: 0.86 + 0.14 * bp,
                    hl_sy: 0.9 + 0.1 * bp,
                    hl_dx: (1.0 - bp) * -0.1,
                    icon_scale: 0.35 + 0.65 * bp.min(1.15),
                    icon_rot: (1.0 - bp) * -25.0,
                },
            ));
        }
        // Drop textures of words that left the stage (keeps a couple either side).
        self.tex.retain(|&i, _| (i as i64) >= lo - 2 && (i as i64) <= hi + 2);
        let mut used = [false; SLOTS];
        for (slot, word, motion) in out {
            used[slot] = true;
            if self.slots.row_data(slot).as_ref() != Some(&word) {
                self.slots.set_row_data(slot, word);
            }
            self.motion.set_row_data(slot, motion);
        }
        for (slot, used) in used.iter().enumerate() {
            if !used && self.motion.row_data(slot).is_some_and(|m| m.opacity > 0.0) {
                self.motion.set_row_data(slot, KMotion::default());
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// State, clock and the frame timer.

struct Settings {
    k_style: String,
    k_colors: [usize; 3],
}

const LYRIC_COLORS: &[u32] = &[0xffffff, 0xf5e6c8, 0xc8e6ff, 0xffd6e8, 0xd4ffd6, 0xe0d4ff];
const HIGHLIGHT_COLORS: &[u32] = &[0xff3d8b, 0xff6b3d, 0xffd23d, 0x3dffa8, 0x3db8ff, 0x8b5cff, 0x1c1c22];
const SECONDARY_COLORS: &[u32] = &[0xd6906c, 0xff3b3b, 0x9b9b9b, 0x6cb4d6, 0xb06cd6, 0x6cd68c];

fn palette(i: usize) -> &'static [u32] {
    [LYRIC_COLORS, HIGHLIGHT_COLORS, SECONDARY_COLORS][i]
}

fn settings_path() -> std::path::PathBuf {
    crate::auth::data_dir().join("settings.json")
}

fn load_settings() -> Settings {
    let v: serde_json::Value = std::fs::read(settings_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let col = |i: usize| v["k_colors"][i].as_u64().map_or(0, |c| c as usize % palette(i).len());
    Settings {
        k_style: v["k_style"].as_str().unwrap_or("visual").to_string(),
        k_colors: [col(0), col(1), col(2)],
    }
}

fn save_settings(s: &Settings) {
    let v = serde_json::json!({ "k_style": s.k_style, "k_colors": s.k_colors });
    let _ = std::fs::write(settings_path(), v.to_string());
}

struct State {
    ui: slint::Weak<MainWindow>,
    // clock
    pos: u32,
    at: Instant,
    playing: bool,
    duration: u32,
    // lyrics
    lyrics: Option<Rc<Lyrics>>,
    karaoke_words: Option<Rc<Lyrics>>,
    laid_out_w: f32,
    active: i32,
    settings: Settings,
    karaoke: Karaoke,
    last_frame: Instant,
    last_lyric: i64,
    snap: bool,
    timer: slint::Timer,
    /// The renderer calls frame() before every frame (GPU renderer): karaoke runs on vsync.
    vsync: bool,
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

impl State {
    fn now(&self) -> u32 {
        if self.playing {
            let ms = self.pos as u64 + self.at.elapsed().as_millis() as u64;
            (ms.min(self.duration.max(self.pos) as u64)) as u32
        } else {
            self.pos
        }
    }

    fn karaoke_on(&self, ui: &MainWindow) -> bool {
        let pb = ui.global::<Playback>();
        pb.get_karaoke() && pb.get_expanded()
    }
}

fn window_size(ui: &MainWindow) -> (f32, f32) {
    let s = ui.window().scale_factor();
    let px = ui.window().size();
    let (w, h) = (px.width as f32 / s, px.height as f32 / s);
    if w < 1.0 { (1200.0, 780.0) } else { (w, h) }
}

/// One frame: clock to the UI, active line, karaoke words; then pick the next interval.
fn tick() {
    let Some(ui) = with(|s| s.ui.upgrade()).flatten() else { return };
    with(|s| {
        let pb = ui.global::<Playback>();
        let pos = s.now();
        pb.set_position_ms(pos as i32);
        let lyric = pos as i64;
        pb.set_lyric_ms(lyric.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        if (lyric - s.last_lyric).abs() > 1500 {
            s.snap = true;
        }
        s.last_lyric = lyric;

        // Active line: the last one that has started.
        let active = match &s.lyrics {
            Some(l) if l.synced => l.lines.partition_point(|x| (x.start_ms as i64) <= lyric) as i32 - 1,
            _ => -1,
        };
        if active != s.active {
            s.active = active;
            pb.set_lyr_active(active);
        }

        let now = Instant::now();
        let dt = now.duration_since(s.last_frame).as_secs_f32() * 1000.0;
        s.last_frame = now;
        if s.karaoke_on(&ui) {
            let (w, h) = window_size(&ui);
            s.karaoke.update(lyric, dt, s.snap, w, h, ui.window().scale_factor());
            s.snap = false;
        }

        let want = if !s.playing {
            None
        } else if s.karaoke_on(&ui) {
            if s.vsync {
                // Next vsync frame calls frame() -> tick() again.
                s.timer.stop();
                ui.window().request_redraw();
                return;
            }
            Some(Duration::from_millis(16))
        } else if pb.get_expanded() {
            // Wake exactly when the next letter or line lights (at most 250 ms apart for the
            // progress bars); Slint's own animations run on vsync in between.
            Some(next_change(s, lyric))
        } else {
            Some(Duration::from_millis(500))
        };
        match want {
            None => s.timer.stop(),
            Some(d) => s.timer.start(slint::TimerMode::SingleShot, d, tick),
        }
    });
}

/// Time until the classic view next changes: the next line start or the next letter of the
/// active line to light (same formula as word_rows), capped at 250 ms.
fn next_change(s: &State, lyric: i64) -> Duration {
    let mut next = i64::MAX;
    if let Some(l) = s.lyrics.as_ref().filter(|l| l.synced) {
        let i = s.active;
        if let Some(n) = l.lines.get((i + 1).max(0) as usize) {
            next = n.start_ms as i64;
        }
        if let Some(line) = (i >= 0).then(|| &l.lines[i as usize]) {
            let line_end = l.lines.get(i as usize + 1).map_or((line.start_ms + 8000).min(s.duration.max(line.start_ms + 1)), |n| n.start_ms) as i64;
            let words: Vec<&str> = line.text.split_whitespace().collect();
            if words.len() == line.words.len() {
                for (j, w) in words.iter().enumerate() {
                    let st = line.words[j] as i64;
                    let e = line.words.get(j + 1).map_or(line_end, |&x| x as i64).max(st + 1);
                    let n = w.chars().count().max(1) as i64;
                    for k in 0..n {
                        let at = st + (e - st) * k / n;
                        if at > lyric && at < next {
                            next = at;
                        }
                    }
                }
            }
        }
    }
    Duration::from_millis((next.saturating_sub(lyric)).clamp(1, 250) as u64)
}

fn apply_layout(ui: &MainWindow, s: &mut State, force: bool) {
    let (w, _) = window_size(ui);
    if !force && (w - s.laid_out_w).abs() < 0.5 {
        return;
    }
    s.laid_out_w = w;
    let pb = ui.global::<Playback>();
    match &s.lyrics {
        Some(l) => {
            let (lines, total) = build_lines(l, s.duration, w);
            pb.set_lyr_total_h(total);
            pb.set_lyr_lines(ModelRc::new(VecModel::from(lines)));
        }
        None => {
            pb.set_lyr_total_h(0.0);
            pb.set_lyr_lines(ModelRc::default());
        }
    }
}

fn apply_colors(pb: &Playback, s: &Settings, k: &mut Karaoke) {
    let c = |i: usize| slint::Color::from_argb_encoded(0xff000000 | palette(i)[s.k_colors[i]]);
    k.colors = [c(0), c(1), c(2)];
    let h = k.colors[1];
    pb.set_k_bg(slint::Image::from_rgb8(crate::art::karaoke_bg(h.red(), h.green(), h.blue())));
    pb.set_k_lyric(c(0));
    pb.set_k_highlight(c(1));
    pb.set_k_secondary(c(2));
}

// ---------------------------------------------------------------------------------------------
// Public API (UI thread).

pub fn init(ui: &MainWindow) {
    let settings = load_settings();
    let mut karaoke = Karaoke::new();
    karaoke.set_style(&settings.k_style);
    let pb = ui.global::<Playback>();
    pb.set_k_words(ModelRc::from(karaoke.slots.clone()));
    pb.set_k_motion(ModelRc::from(karaoke.motion.clone()));
    pb.set_k_style(settings.k_style.as_str().into());
    pb.set_karaoke_allowed(cfg!(feature = "karaoke"));
    apply_colors(&pb, &settings, &mut karaoke);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ui: ui.as_weak(),
            pos: 0,
            at: Instant::now(),
            playing: false,
            duration: 0,
            lyrics: None,
            karaoke_words: None,
            laid_out_w: 0.0,
            active: -1,
            settings,
            karaoke,
            last_frame: Instant::now(),
            last_lyric: 0,
            snap: true,
            timer: slint::Timer::default(),
            vsync: false,
        })
    });

    pb.on_set_karaoke(set_karaoke);
    pb.on_set_k_style(|name| {
        with(|s| {
            s.karaoke.set_style(&name);
            s.settings.k_style = name.to_string();
            save_settings(&s.settings);
            s.snap = true;
        });
        if let Some(ui) = with(|s| s.ui.upgrade()).flatten() {
            ui.global::<Playback>().set_k_style(name);
        }
        tick();
    });
    pb.on_cycle_color(|name, dir| {
        let i = match name.as_str() {
            "lyric" => 0,
            "highlight" => 1,
            _ => 2,
        };
        with(|s| {
            let n = palette(i).len() as i32;
            s.settings.k_colors[i] = ((s.settings.k_colors[i] as i32 + dir).rem_euclid(n)) as usize;
            save_settings(&s.settings);
            if let Some(ui) = s.ui.upgrade() {
                apply_colors(&ui.global::<Playback>(), &s.settings, &mut s.karaoke);
                s.snap = true;
            }
        });
    });
    pb.on_reset_k_colors(|| {
        with(|s| {
            s.settings.k_colors = [0; 3];
            save_settings(&s.settings);
            if let Some(ui) = s.ui.upgrade() {
                apply_colors(&ui.global::<Playback>(), &s.settings, &mut s.karaoke);
                s.snap = true;
            }
        });
    });
}

fn set_karaoke(on: bool) {
    with(|s| {
        if let Some(ui) = s.ui.upgrade() {
            let pb = ui.global::<Playback>();
            let on = on && cfg!(feature = "karaoke") && pb.get_karaoke_available();
            pb.set_karaoke(on);
            s.snap = true;
            if !on {
                s.karaoke.hide_all();
            }
        }
    });
    tick();
}

/// Keyboard shortcut on the lyrics page: k toggles karaoke.
pub fn key(k: &str) {
    if k == "k" {
        let on = with(|s| s.ui.upgrade().map(|u| u.global::<Playback>().get_karaoke())).flatten().unwrap_or(false);
        set_karaoke(!on);
    }
}

/// The GPU renderer is up: drive karaoke from frame() instead of a timer.
pub fn enable_vsync() {
    with(|s| s.vsync = true);
}

/// Called by the renderer before every frame. Only the karaoke stage needs per-frame work;
/// everything else returns at once.
pub fn frame() {
    // CADENCE_FPS=1: print rendered frames per second (diagnostics).
    thread_local! { static FPS: std::cell::Cell<(bool, u32, Option<Instant>)> = const { std::cell::Cell::new((false, 0, None)) }; }
    FPS.with(|c| {
        let (init, n, t) = c.get();
        let on = if init { t.is_some() } else { std::env::var("CADENCE_FPS").is_ok_and(|v| v == "1") };
        if !on {
            c.set((true, 0, None));
            return;
        }
        let t0 = t.unwrap_or_else(Instant::now);
        if t0.elapsed() >= Duration::from_secs(1) {
            eprintln!("fps: {}", n + 1);
            c.set((true, 0, Some(Instant::now())));
        } else {
            c.set((true, n + 1, Some(t0)));
        }
    });
    let busy = with(|s| s.vsync && s.playing && s.ui.upgrade().is_some_and(|ui| s.karaoke_on(&ui))).unwrap_or(false);
    if busy {
        tick();
    }
}

/// A new track: reset the clock and the lyrics.
pub fn on_track(duration_ms: u32) {
    with(|s| {
        s.pos = 0;
        s.at = Instant::now();
        s.duration = duration_ms;
        s.lyrics = None;
        s.karaoke_words = None;
        s.karaoke.set_words(None);
        s.active = -1;
        s.snap = true;
        if let Some(ui) = s.ui.upgrade() {
            let pb = ui.global::<Playback>();
            pb.set_lyr_active(-1);
            pb.set_lyr_synced(false);
            pb.set_lyr_hint("finding lyrics…".into());
            pb.set_karaoke_available(false);
            pb.set_karaoke(false);
            s.karaoke.hide_all();
            apply_layout(&ui, s, true);
        }
    });
    tick();
}

/// Lyrics arrived (or none were found).
pub fn set_lyrics(lyrics: Option<Lyrics>) {
    with(|s| {
        let Some(ui) = s.ui.upgrade() else { return };
        let pb = ui.global::<Playback>();
        let lyrics = lyrics.filter(|l| !l.lines.is_empty());
        pb.set_lyr_hint(match &lyrics {
            None => "no lyrics found for this one".into(),
            Some(l) if !l.synced => "unsynced lyrics".into(),
            Some(_) => "".into(),
        });
        pb.set_lyr_synced(lyrics.as_ref().is_some_and(|l| l.synced));
        s.lyrics = lyrics.map(Rc::new);
        s.active = -1;
        pb.set_lyr_active(-1);
        apply_layout(&ui, s, true);
        // Without Apple word lyrics, karaoke can still use word-timed lyrics from the pipeline.
        if s.karaoke_words.is_none() {
            let words = s.lyrics.clone().filter(|l| l.synced && l.word_sync);
            s.karaoke.set_words(words.as_deref());
            pb.set_karaoke_available(cfg!(feature = "karaoke") && words.is_some());
        }
        s.snap = true;
    });
    tick();
}

/// Apple Music word lyrics for the karaoke stage (karaoke builds only).
#[cfg_attr(not(feature = "karaoke"), allow(dead_code))]
pub fn set_karaoke_words(lyrics: Lyrics) {
    with(|s| {
        if lyrics.lines.iter().all(|l| l.words.is_empty()) {
            return;
        }
        let l = Rc::new(lyrics);
        s.karaoke.set_words(Some(&l));
        s.karaoke_words = Some(l);
        s.snap = true;
        if let Some(ui) = s.ui.upgrade() {
            ui.global::<Playback>().set_karaoke_available(true);
        }
    });
    tick();
}

/// librespot reported a position (play, pause, seek, correction).
pub fn on_position(position_ms: u32, playing: bool) {
    with(|s| {
        // How far the running clock had drifted from what librespot now reports (logged for tests).
        let drift = s.now() as i64 - position_ms as i64;
        if s.playing {
            eprintln!("clock: librespot {position_ms} ms, lyrics clock {} ms, drift {drift} ms", s.now());
        }
        if drift.abs() > 1500 {
            s.snap = true;
        }
        s.pos = position_ms;
        s.at = Instant::now();
        s.playing = playing;
    });
    tick();
}

/// The user seeked: move the clock now instead of waiting for librespot's event.
pub fn on_seek(position_ms: u32) {
    with(|s| {
        s.pos = position_ms;
        s.at = Instant::now();
        s.snap = true;
    });
    tick();
}

pub fn on_stopped() {
    with(|s| {
        s.pos = s.now();
        s.at = Instant::now();
        s.playing = false;
    });
    tick();
}

/// Window resized or the page opened/closed: re-wrap when the width changed, re-render.
pub fn relayout() {
    with(|s| {
        if let Some(ui) = s.ui.upgrade() {
            apply_layout(&ui, s, false);
            s.snap = true;
        }
    });
    tick();
}

/// Test hook (CADENCE_DEMO): start the clock at `position_ms` as if playing.
pub fn demo_clock(position_ms: u32, duration_ms: u32, playing: bool) {
    with(|s| s.duration = duration_ms);
    on_position(position_ms, playing);
}

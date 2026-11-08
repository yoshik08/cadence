#![allow(dead_code)]
//! Album art: fetch, DCT-downscaled JPEG decode, square crop, accent colour, small LRU.

use slint::{Rgb8Pixel, SharedPixelBuffer};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{OnceCell, Semaphore};

#[derive(Clone)]
pub struct Art {
    pub buf: SharedPixelBuffer<Rgb8Pixel>,
    pub accent: (u8, u8, u8),
}

/// The decoded-art cache is capped by entry count and by pixel bytes (w*h*3).
const CACHE_CAP: usize = 24;
const CACHE_BYTES: usize = 6 << 20;
static CACHE: Mutex<Vec<(String, Art)>> = Mutex::new(Vec::new());
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
/// At most 4 loads fetch/decode at once, which bounds both threads and transient buffers.
static SEM: Semaphore = Semaphore::const_new(4);

/// A downloaded JPEG body (reqwest's `Bytes`, type-erased so no extra crate is needed).
type Body = Arc<dyn AsRef<[u8]> + Send + Sync>;
/// The last two downloaded bodies, so loading one URL at several sizes (now playing big + bar)
/// downloads it once; concurrent loads of a URL share the same in-flight fetch.
static BODIES: Mutex<Vec<(String, Arc<OnceCell<Body>>)>> = Mutex::new(Vec::new());

/// Smallest image whose width is >= `px`, else the largest. Images without a size (user
/// playlists) count as 0 wide, so ties resolve to the first entry, which Spotify lists largest.
pub fn pick_url(images: &[rspotify::model::Image], px: u32) -> Option<String> {
    let w = |i: &&rspotify::model::Image| i.width.or(i.height).unwrap_or(0);
    images
        .iter()
        .filter(|i| w(i) >= px)
        .min_by_key(w)
        .or_else(|| images.iter().rev().max_by_key(w))
        .map(|i| i.url.clone())
}

/// Fetch `url`, decode at `px`x`px`. Cached by url+px (LRU, 24 entries / 6 MB). `None` on any failure.
pub async fn load(url: &str, px: u32) -> Option<Art> {
    let key = format!("{url}@{px}");
    {
        let mut cache = CACHE.lock().ok()?;
        if let Some(i) = cache.iter().position(|(k, _)| *k == key) {
            let hit = cache.remove(i);
            let art = hit.1.clone();
            cache.insert(0, hit);
            return Some(art);
        }
    }
    let _permit = SEM.acquire().await.ok()?;
    let body = body(url).await.map_err(|e| eprintln!("art: {url}: {e}")).ok()?;
    let art = tokio::task::spawn_blocking(move || decode(AsRef::<[u8]>::as_ref(&*body), px))
        .await
        .ok()?
        .map_err(|e| eprintln!("art: {url}: {e}"))
        .ok()?;
    if let Ok(mut cache) = CACHE.lock() {
        cache.retain(|(k, _)| *k != key);
        cache.insert(0, (key, art.clone()));
        let size = |a: &Art| a.buf.width() as usize * a.buf.height() as usize * 3;
        let mut bytes: usize = cache.iter().map(|(_, a)| size(a)).sum();
        while cache.len() > 1 && (cache.len() > CACHE_CAP || bytes > CACHE_BYTES) {
            if let Some((_, old)) = cache.pop() {
                bytes -= size(&old);
            }
        }
    }
    Some(art)
}

async fn body(url: &str) -> Result<Body, String> {
    let cell = {
        let mut bodies = BODIES.lock().map_err(|e| e.to_string())?;
        match bodies.iter().position(|(u, _)| u == url) {
            Some(i) => bodies[i].1.clone(),
            None => {
                let cell = Arc::new(OnceCell::new());
                bodies.insert(0, (url.to_string(), cell.clone()));
                bodies.truncate(2);
                cell
            }
        }
    };
    cell.get_or_try_init(|| fetch(url)).await.cloned()
}

async fn fetch(url: &str) -> Result<Body, String> {
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    });
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let resp = resp.error_for_status().map_err(|e| e.to_string())?;
    Ok(Arc::new(resp.bytes().await.map_err(|e| e.to_string())?))
}

fn decode(data: &[u8], px: u32) -> Result<Art, String> {
    let e = |e: jpeg_decoder::Error| e.to_string();
    let mut dec = jpeg_decoder::Decoder::new(data);
    dec.read_info().map_err(e)?;
    let info = dec.info().ok_or("no jpeg info")?;
    let (w, h) = (u32::from(info.width).max(1), u32::from(info.height).max(1));
    // Ask for px on the short side so the DCT scale never undershoots the square crop.
    let (rw, rh) = if w >= h { (px * w / h, px) } else { (px, px * h / w) };
    let clamp = |v: u32| v.min(u16::MAX as u32) as u16;
    let (sw, sh) = dec.scale(clamp(rw), clamp(rh)).map_err(e)?;
    let raw = dec.decode().map_err(e)?;
    let fmt = dec.info().ok_or("no jpeg info")?.pixel_format;

    let (sw, sh) = (sw as usize, sh as usize);
    let n = fmt.pixel_bytes();
    if sw == 0 || sh == 0 || raw.len() < sw * sh * n {
        return Err("short jpeg data".into());
    }
    let rgb = |i: usize| -> [u32; 3] {
        let p = &raw[i * n..(i + 1) * n];
        match fmt {
            jpeg_decoder::PixelFormat::RGB24 => [p[0] as u32, p[1] as u32, p[2] as u32],
            jpeg_decoder::PixelFormat::CMYK32 => {
                let k = 255 - p[3] as u32;
                [(255 - p[0] as u32) * k / 255, (255 - p[1] as u32) * k / 255, (255 - p[2] as u32) * k / 255]
            }
            // L8, or L16 (big endian: high byte first).
            _ => [p[0] as u32; 3],
        }
    };

    // Centre-crop to a square, then box-filter to px x px (a single source pixel if upscaling).
    let side = sw.min(sh);
    let (x0, y0) = ((sw - side) / 2, (sh - side) / 2);
    let px = px as usize;
    let span = |d: usize| {
        let a = d * side / px;
        (a, ((d + 1) * side).div_ceil(px).clamp(a + 1, side))
    };
    let mut buf = SharedPixelBuffer::<Rgb8Pixel>::new(px as u32, px as u32);
    for (dy, row) in buf.make_mut_slice().chunks_exact_mut(px).enumerate() {
        let (ya, yb) = span(dy);
        for (dx, out) in row.iter_mut().enumerate() {
            let (xa, xb) = span(dx);
            let mut sum = [0u32; 3];
            for y in ya..yb {
                for x in xa..xb {
                    let c = rgb((y0 + y) * sw + x0 + x);
                    sum.iter_mut().zip(c).for_each(|(s, c)| *s += c);
                }
            }
            let cnt = ((yb - ya) * (xb - xa)) as u32;
            *out = Rgb8Pixel { r: (sum[0] / cnt) as u8, g: (sum[1] / cnt) as u8, b: (sum[2] / cnt) as u8 };
        }
    }
    let accent = accent(&buf);
    Ok(Art { buf, accent })
}

/// Dominant vivid colour: sample a coarse grid, vote into 12 hue bins weighted by saturation
/// (dark and light pixels count little), average the winning bin, then clamp saturation and
/// lightness so it reads as an accent on #0a0a0b. Near-greyscale art gets a soft neutral.
fn accent(buf: &SharedPixelBuffer<Rgb8Pixel>) -> (u8, u8, u8) {
    let (w, h) = (buf.width() as usize, buf.height() as usize);
    let pixels = buf.as_slice();
    let step = (w / 32).max(1);
    let mut bins = [[0f32; 4]; 12]; // weight, then weighted r, g, b
    let (mut luma, mut n) = (0f32, 0f32);
    for y in (step / 2..h).step_by(step) {
        for x in (step / 2..w).step_by(step) {
            let p = pixels[y * w + x];
            let (r, g, b) = (p.r as f32 / 255.0, p.g as f32 / 255.0, p.b as f32 / 255.0);
            let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
            let (l, c) = ((mx + mn) / 2.0, mx - mn);
            luma += l;
            n += 1.0;
            if c < 0.06 {
                continue;
            }
            let sat = c / (1.0 - (2.0 * l - 1.0).abs());
            let wt = sat * (4.0 * l * (1.0 - l)).powi(2);
            let hue = if mx == r {
                ((g - b) / c).rem_euclid(6.0)
            } else if mx == g {
                (b - r) / c + 2.0
            } else {
                (r - g) / c + 4.0
            };
            let bin = &mut bins[(hue * 2.0) as usize % 12];
            *bin = [bin[0] + wt, bin[1] + wt * r, bin[2] + wt * g, bin[3] + wt * b];
        }
    }
    // Pick the bin by its own weight plus half of each neighbour's, so a hue split across a
    // bin edge still wins; the colour itself comes from that bin alone.
    let score = |i: usize| bins[i][0] + 0.5 * (bins[(i + 11) % 12][0] + bins[(i + 1) % 12][0]);
    let best = (0..12).max_by(|&a, &b| score(a).total_cmp(&score(b))).unwrap_or(0);
    let bin = bins[best];

    let (hue, sat, light) = if bin[0] < 0.01 * n.max(1.0) {
        (0.0, 0.0, (luma / n.max(1.0)).clamp(0.74, 0.88))
    } else {
        let (hue, sat, l) = rgb_to_hsl(bin[1] / bin[0], bin[2] / bin[0], bin[3] / bin[0]);
        (hue, sat.clamp(0.45, 0.85), l.clamp(0.60, 0.74))
    };
    let (r, g, b) = hsl_to_rgb(hue, sat, light);
    ((r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
}

/// Hue is in sixths of a turn (0..6).
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let (l, c) = ((mx + mn) / 2.0, mx - mn);
    if c <= f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let hue = if mx == r {
        ((g - b) / c).rem_euclid(6.0)
    } else if mx == g {
        (b - r) / c + 2.0
    } else {
        (r - g) / c + 4.0
    };
    (hue, c / (1.0 - (2.0 * l - 1.0).abs()), l)
}

fn hsl_to_rgb(hue: f32, sat: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * sat;
    let x = c * (1.0 - (hue % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r + m, g + m, b + m)
}

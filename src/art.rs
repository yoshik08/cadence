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
/// A Spotify image entry.
#[derive(Clone, Debug, Default)]
pub struct SpImage {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub fn pick_url(images: &[SpImage], px: u32) -> Option<String> {
    let w = |i: &&SpImage| i.width.or(i.height).unwrap_or(0);
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
    // Spotify art URLs are immutable, so a copy on disk is good forever (pruned by size).
    let name = crate::cache::key(url);
    if let Some(bytes) = tokio::task::spawn_blocking({ let n = name.clone(); move || crate::cache::read("art", &n, None) })
        .await
        .ok()
        .flatten()
    {
        return Ok(Arc::new(bytes));
    }
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let resp = resp.error_for_status().map_err(|e| e.to_string())?;
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let copy = bytes.clone();
    tokio::task::spawn_blocking(move || crate::cache::write("art", &name, &copy));
    Ok(Arc::new(bytes))
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

/// The `.lyrics-bg` look: the art blurred (CSS blur(70px) over a full window) and darkened to
/// 45%. Made tiny on purpose: the renderer's smooth upscale does the rest of the blur.
pub fn blurred_bg(buf: &SharedPixelBuffer<Rgb8Pixel>) -> SharedPixelBuffer<Rgb8Pixel> {
    const N: usize = 40;
    let (w, h) = (buf.width() as usize, buf.height() as usize);
    let src = buf.as_slice();
    let mut px = vec![[0f32; 3]; N * N];
    for (i, p) in px.iter_mut().enumerate() {
        let (dx, dy) = (i % N, i / N);
        let (xa, xb) = (dx * w / N, ((dx + 1) * w / N).max(dx * w / N + 1).min(w));
        let (ya, yb) = (dy * h / N, ((dy + 1) * h / N).max(dy * h / N + 1).min(h));
        let mut s = [0f32; 3];
        for y in ya..yb {
            for x in xa..xb {
                let c = src[y * w + x];
                s[0] += c.r as f32;
                s[1] += c.g as f32;
                s[2] += c.b as f32;
            }
        }
        let n = ((yb - ya) * (xb - xa)).max(1) as f32;
        *p = [s[0] / n, s[1] / n, s[2] / n];
    }
    // Three box passes (radius 3) approximate a gaussian.
    for _ in 0..3 {
        for horizontal in [true, false] {
            let old = px.clone();
            for y in 0..N {
                for x in 0..N {
                    let mut s = [0f32; 3];
                    for d in -3i32..=3 {
                        let (sx, sy) = if horizontal {
                            ((x as i32 + d).clamp(0, N as i32 - 1) as usize, y)
                        } else {
                            (x, (y as i32 + d).clamp(0, N as i32 - 1) as usize)
                        };
                        let c = old[sy * N + sx];
                        s[0] += c[0];
                        s[1] += c[1];
                        s[2] += c[2];
                    }
                    px[y * N + x] = [s[0] / 7.0, s[1] / 7.0, s[2] / 7.0];
                }
            }
        }
    }
    // Bilinear upsample to 256 px: the renderer scales the rest of the way with nearest
    // neighbour, which would show 40 px blocks from the tiny copy.
    const M: usize = 256;
    let mut out = SharedPixelBuffer::<Rgb8Pixel>::new(M as u32, M as u32);
    let at = |x: usize, y: usize| px[y.min(N - 1) * N + x.min(N - 1)];
    for (i, o) in out.make_mut_slice().iter_mut().enumerate() {
        let fx = ((i % M) as f32 + 0.5) * N as f32 / M as f32 - 0.5;
        let fy = ((i / M) as f32 + 0.5) * N as f32 / M as f32 - 0.5;
        let (x0, y0) = (fx.floor().max(0.0) as usize, fy.floor().max(0.0) as usize);
        let (tx, ty) = ((fx - x0 as f32).clamp(0.0, 1.0), (fy - y0 as f32).clamp(0.0, 1.0));
        let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
        // .lyrics-vignette, baked in (a full-window radial gradient costs a lot per repaint):
        // radial-gradient(120% 90%, transparent 55%, rgba(0,0,0,.5)), for a ~16:10 window.
        let u = ((i % M) as f32 + 0.5) / M as f32 - 0.5;
        let v = (((i / M) as f32 + 0.5) / M as f32 - 0.5) / 0.625;
        let t = ((u / 1.2).powi(2) + (v / 0.9).powi(2)).sqrt();
        let shade = 1.0 - ((t - 0.55) / 0.45).clamp(0.0, 1.0) * 0.5;
        let ch = |k: usize| {
            let top = a[k] + (b[k] - a[k]) * tx;
            let bot = c[k] + (d[k] - c[k]) * tx;
            ((top + (bot - top) * ty) * 0.45 * shade).round().clamp(0.0, 255.0) as u8
        };
        *o = Rgb8Pixel { r: ch(0), g: ch(1), b: ch(2) };
    }
    out
}

/// The art with its corners cut in alpha (anti-aliased), `radius` as a fraction of the side.
/// The software renderer ignores border-radius on images; this works over any background.
pub fn rounded(buf: &SharedPixelBuffer<Rgb8Pixel>, radius: f32) -> SharedPixelBuffer<slint::Rgba8Pixel> {
    let (w, h) = (buf.width() as usize, buf.height() as usize);
    let r = radius * w.min(h) as f32;
    let mut out = SharedPixelBuffer::<slint::Rgba8Pixel>::new(w as u32, h as u32);
    let src = buf.as_slice();
    for (i, o) in out.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
        let cx = if x < r { r } else if x > w as f32 - r { w as f32 - r } else { x };
        let cy = if y < r { r } else if y > h as f32 - r { h as f32 - r } else { y };
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        let a = (r - d + 0.5).clamp(0.0, 1.0);
        let c = src[i];
        *o = slint::Rgba8Pixel { r: c.r, g: c.g, b: c.b, a: (a * 255.0).round() as u8 };
    }
    out
}

/// The karaoke stage background (.vk + .vk-glow) as one 16:9 image: #050507, the highlight at 26%
/// in an ellipse around the middle, then the vignette. Pre-rendered because two full-window
/// radial gradients per frame would dominate the software renderer's repaint cost.
pub fn karaoke_bg(r: u8, g: u8, b: u8) -> SharedPixelBuffer<Rgb8Pixel> {
    const W: usize = 384;
    const H: usize = 216;
    let mut out = SharedPixelBuffer::<Rgb8Pixel>::new(W as u32, H as u32);
    for (i, o) in out.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (((i % W) as f32 + 0.5) / W as f32, ((i / W) as f32 + 0.5) / H as f32);
        // radial-gradient(44% 38% at 50% 48%, highlight 26%, transparent 74%)
        let tg = (((x - 0.5) / 0.44).powi(2) + ((y - 0.48) / 0.38).powi(2)).sqrt();
        let glow = 0.26 * (1.0 - (tg / 0.74).clamp(0.0, 1.0));
        // radial-gradient(120% 90%, transparent 55%, rgba(0,0,0,.55))
        let tv = (((x - 0.5) / 1.2).powi(2) + ((y - 0.5) / 0.9).powi(2)).sqrt();
        let shade = 1.0 - ((tv - 0.55) / 0.45).clamp(0.0, 1.0) * 0.55;
        let ch = |base: f32, c: u8| ((base + (c as f32 - base) * glow) * shade).round().clamp(0.0, 255.0) as u8;
        *o = Rgb8Pixel { r: ch(5.0, r), g: ch(5.0, g), b: ch(7.0, b) };
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Local images (profile picture, playlist cover upload, local-file artwork).

/// Decodes JPEG or PNG bytes to a square RGB image `px` wide (center crop, box downscale).
pub fn decode_square(data: &[u8], px: u32) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    if data.starts_with(&[0xFF, 0xD8]) {
        return decode(data, px).ok().map(|a| a.buf);
    }
    let dec = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        _ => return None,
    };
    if info.bit_depth != png::BitDepth::Eight || w == 0 || h == 0 {
        return None;
    }
    let side = w.min(h);
    let (ox, oy) = ((w - side) / 2, (h - side) / 2);
    let px = px as usize;
    let mut out = SharedPixelBuffer::<Rgb8Pixel>::new(px as u32, px as u32);
    let dst = out.make_mut_slice();
    for y in 0..px {
        for x in 0..px {
            // Box filter over the source area this pixel covers.
            let (x0, x1) = (ox + x * side / px, ox + ((x + 1) * side / px).max(x * side / px + 1));
            let (y0, y1) = (oy + y * side / px, oy + ((y + 1) * side / px).max(y * side / px + 1));
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1.min(h) {
                for sx in x0..x1.min(w) {
                    let i = (sy * w + sx) * ch;
                    let (pr, pg, pb) = if ch >= 3 { (buf[i], buf[i + 1], buf[i + 2]) } else { (buf[i], buf[i], buf[i]) };
                    r += pr as u32;
                    g += pg as u32;
                    b += pb as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            dst[y * px + x] = Rgb8Pixel { r: (r / n) as u8, g: (g / n) as u8, b: (b / n) as u8 };
        }
    }
    Some(out)
}

/// A square PNG copy of an image file (profile picture).
pub fn save_square_png(src: &std::path::Path, dst: &std::path::Path, px: u32) -> bool {
    let Ok(data) = std::fs::read(src) else { return false };
    let Some(img) = decode_square(&data, px) else { return false };
    let Ok(file) = std::fs::File::create(dst) else { return false };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width(), img.height());
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut w| w.write_image_data(img.as_bytes())).is_ok()
}

/// A playlist cover from an image file: square JPEG under Spotify's 256 KB limit (as base64,
/// which is how the upload endpoint wants it), plus a preview.
pub fn cover_jpeg_base64(src: &std::path::Path) -> Option<(Vec<u8>, SharedPixelBuffer<Rgb8Pixel>)> {
    use base64::Engine;
    let data = std::fs::read(src).ok()?;
    for (px, q) in [(640u32, 85u8), (512, 80), (400, 75), (300, 70)] {
        let img = decode_square(&data, px)?;
        let mut jpg = Vec::new();
        let enc = jpeg_encoder::Encoder::new(&mut jpg, q);
        enc.encode(img.as_bytes(), px as u16, px as u16, jpeg_encoder::ColorType::Rgb).ok()?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpg).into_bytes();
        if b64.len() <= 256 * 1024 {
            return Some((b64, decode_square(&data, 112)?));
        }
    }
    None
}

/// Box-downscales a square image to `px`.
pub fn decode_rescale(src: &SharedPixelBuffer<Rgb8Pixel>, px: u32) -> SharedPixelBuffer<Rgb8Pixel> {
    let (sw, px) = (src.width() as usize, px.max(1) as usize);
    let s = src.as_slice();
    let mut out = SharedPixelBuffer::<Rgb8Pixel>::new(px as u32, px as u32);
    let d = out.make_mut_slice();
    for y in 0..px {
        for x in 0..px {
            let (x0, x1) = (x * sw / px, ((x + 1) * sw / px).max(x * sw / px + 1).min(sw));
            let (y0, y1) = (y * sw / px, ((y + 1) * sw / px).max(y * sw / px + 1).min(sw));
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = s[sy * sw + sx];
                    r += p.r as u32;
                    g += p.g as u32;
                    b += p.b as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            d[y * px + x] = Rgb8Pixel { r: (r / n) as u8, g: (g / n) as u8, b: (b / n) as u8 };
        }
    }
    out
}

pub fn save_rgb_png(buf: &SharedPixelBuffer<Rgb8Pixel>, path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::create(path) else { return false };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), buf.width(), buf.height());
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut w| w.write_image_data(buf.as_bytes())).is_ok()
}

//! Which is faster for cover art: the app's current path (jpeg-decoder with DCT-domain
//! downscaling, then a box filter to the exact size) or zune-jpeg (full-size decode) followed by
//! fast_image_resize? Both produce a square RGB image of the slot's pixel size.
//!
//! Built with the app's release profile (opt-level "s", fat LTO), on the covers in the folder
//! given as the first argument (the app's disk cache: %LOCALAPPDATA%\cadence\cache\art).

use std::time::{Duration, Instant};

/// The app's path, as in src/art.rs.
fn current(data: &[u8], px: usize) -> Option<Vec<u8>> {
    let mut dec = jpeg_decoder::Decoder::new(data);
    dec.read_info().ok()?;
    let info = dec.info()?;
    let (w, h) = (u32::from(info.width).max(1), u32::from(info.height).max(1));
    let p = px as u32;
    let (rw, rh) = if w >= h { (p * w / h, p) } else { (p, p * h / w) };
    let clamp = |v: u32| v.min(u16::MAX as u32) as u16;
    let (sw, sh) = dec.scale(clamp(rw), clamp(rh)).ok()?;
    let raw = dec.decode().ok()?;
    if dec.info()?.pixel_format != jpeg_decoder::PixelFormat::RGB24 {
        return None;
    }
    let (sw, sh) = (sw as usize, sh as usize);
    let side = sw.min(sh);
    let (x0, y0) = ((sw - side) / 2, (sh - side) / 2);
    let span = |d: usize| {
        let a = d * side / px;
        (a, ((d + 1) * side).div_ceil(px).clamp(a + 1, side))
    };
    let mut out = vec![0u8; px * px * 3];
    for dy in 0..px {
        let (ya, yb) = span(dy);
        for dx in 0..px {
            let (xa, xb) = span(dx);
            let mut sum = [0u32; 3];
            for y in ya..yb {
                for x in xa..xb {
                    let i = ((y0 + y) * sw + x0 + x) * 3;
                    sum[0] += raw[i] as u32;
                    sum[1] += raw[i + 1] as u32;
                    sum[2] += raw[i + 2] as u32;
                }
            }
            let cnt = ((yb - ya) * (xb - xa)) as u32;
            let o = (dy * px + dx) * 3;
            out[o] = (sum[0] / cnt) as u8;
            out[o + 1] = (sum[1] / cnt) as u8;
            out[o + 2] = (sum[2] / cnt) as u8;
        }
    }
    Some(out)
}

/// zune-jpeg full decode, centre crop, fast_image_resize to the slot.
fn candidate(data: &[u8], px: usize, algorithm: fast_image_resize::ResizeAlg) -> Option<Vec<u8>> {
    use fast_image_resize::{PixelType, ResizeOptions, Resizer, images::Image};
    use zune_jpeg::zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    let raw = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    if raw.len() != (w * h * 3) as usize {
        return None;
    }
    let src = Image::from_vec_u8(w, h, raw, PixelType::U8x3).ok()?;
    let mut dst = Image::new(px as u32, px as u32, PixelType::U8x3);
    let side = w.min(h) as f64;
    let options = ResizeOptions::new().resize_alg(algorithm).crop((w as f64 - side) / 2.0, (h as f64 - side) / 2.0, side, side);
    Resizer::new().resize(&src, &mut dst, &options).ok()?;
    Some(dst.into_vec())
}

fn time(files: &[Vec<u8>], rounds: usize, f: impl Fn(&[u8]) -> Option<Vec<u8>>) -> (Duration, usize) {
    let mut ok = 0;
    let start = Instant::now();
    for _ in 0..rounds {
        ok = files.iter().filter(|d| std::hint::black_box(f(d)).is_some()).count();
    }
    (start.elapsed() / (rounds * files.len().max(1)) as u32, ok)
}

fn main() {
    use fast_image_resize::{FilterType, ResizeAlg};
    let dir = std::env::args().nth(1).expect("usage: cadence-bench-decode <folder of cached covers>");
    let mut all: Vec<(u16, Vec<u8>)> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read folder").flatten() {
        let Ok(data) = std::fs::read(entry.path()) else { continue };
        let mut dec = jpeg_decoder::Decoder::new(&data[..]);
        if dec.read_info().is_ok()
            && let Some(info) = dec.info()
        {
            all.push((info.width, data));
        }
    }
    // (source width, slot sizes in physical px): row thumbnail and mini cover at 100 / 150 %,
    // cards at 100 / 150 %, the lyrics page.
    let cases: [(u16, &[usize]); 3] = [(64, &[56]), (300, &[56, 108, 160, 240]), (640, &[108, 240, 540, 640])];
    println!("{:>6} {:>5} {:>5} | {:>12} {:>16} {:>16} | fastest", "source", "files", "slot", "current", "zune + box", "zune + bilinear");
    for (width, slots) in cases {
        let files: Vec<Vec<u8>> = all.iter().filter(|(w, _)| *w == width).take(60).map(|(_, d)| d.clone()).collect();
        if files.is_empty() {
            println!("{width:>6}  (no covers of this size in the folder)");
            continue;
        }
        for &px in slots {
            let (a, ok) = time(&files, 8, |d| current(d, px));
            let (b, _) = time(&files, 8, |d| candidate(d, px, ResizeAlg::Convolution(FilterType::Box)));
            let (c, _) = time(&files, 8, |d| candidate(d, px, ResizeAlg::Convolution(FilterType::Bilinear)));
            let us = |d: Duration| format!("{:.0} us", d.as_secs_f64() * 1e6);
            let best = if a <= b && a <= c { "current" } else if b <= c { "zune + box" } else { "zune + bilinear" };
            println!("{width:>6} {ok:>5} {px:>5} | {:>12} {:>16} {:>16} | {best}", us(a), us(b), us(c));
        }
    }
}

//! Builds assets/cadence.ico and assets/icon-<n>.png from the 1024 px master.
//!
//! Every size is rendered separately from the master with an area-average downscale in linear
//! light on premultiplied alpha. 24 px and up come straight from the master. At 16 and 20 px the
//! three arcs sit about two pixels apart and smear into one blob, so those sizes are rendered from
//! a hinted master: the faint dark outline is dropped (replaced by the disc colour of that row),
//! and the arc weight and a sub-pixel vertical nudge of the arcs are chosen per size to give the
//! strongest arc / gap contrast. Measured, not guessed: thickening alone closed the gaps.
//!
//! Run from the repo root: cargo run --release --manifest-path scripts/icons/Cargo.toml
//! `-- --sheet <file.png>` also writes a contact sheet of every size on a dark and a light bar.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

const SIZES: [usize; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
/// Sizes rendered from the hinted master.
const SMALL: [usize; 2] = [16, 20];
/// Arc weight changes tried, in master pixels (negative thins, positive thickens).
const WEIGHTS: [f32; 6] = [-14.0, -10.0, -6.0, -3.0, 0.0, 3.0];

struct Img {
    w: usize,
    h: usize,
    /// Straight (not premultiplied) RGBA8.
    px: Vec<[u8; 4]>,
}

fn read_png(path: &Path) -> Img {
    let mut dec = png::Decoder::new(std::io::BufReader::new(File::open(path).expect("open source png")));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png data");
    assert_eq!(info.color_type, png::ColorType::Rgba, "source must be RGBA");
    let px = buf[..info.buffer_size()].chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]).collect();
    Img { w: info.width as usize, h: info.height as usize, px }
}

fn png_bytes(img: &Img) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.w as u32, img.h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::High);
        let mut w = enc.write_header().expect("png header");
        let flat: Vec<u8> = img.px.iter().flatten().copied().collect();
        w.write_image_data(&flat).expect("png data");
    }
    out
}

fn to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let v = if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (v * 255.0).round() as u8
}

/// Area-average downscale: every destination pixel is the coverage-weighted mean of the source
/// area it covers (fractional edges included), in linear light, premultiplied.
fn downscale(src: &Img, size: usize) -> Img {
    let lin: Vec<[f32; 4]> = src
        .px
        .iter()
        .map(|p| {
            let a = p[3] as f32 / 255.0;
            [to_linear(p[0]) * a, to_linear(p[1]) * a, to_linear(p[2]) * a, a]
        })
        .collect();
    let (fx, fy) = (src.w as f32 / size as f32, src.h as f32 / size as f32);
    let mut px = Vec::with_capacity(size * size);
    for dy in 0..size {
        let (y0, y1) = (dy as f32 * fy, (dy + 1) as f32 * fy);
        for dx in 0..size {
            let (x0, x1) = (dx as f32 * fx, (dx + 1) as f32 * fx);
            let mut sum = [0f32; 4];
            let mut area = 0f32;
            for sy in y0.floor() as usize..(y1.ceil() as usize).min(src.h) {
                let wy = (y1.min(sy as f32 + 1.0) - y0.max(sy as f32)).max(0.0);
                for sx in x0.floor() as usize..(x1.ceil() as usize).min(src.w) {
                    let wx = (x1.min(sx as f32 + 1.0) - x0.max(sx as f32)).max(0.0);
                    let p = lin[sy * src.w + sx];
                    let w = wx * wy;
                    for k in 0..4 {
                        sum[k] += p[k] * w;
                    }
                    area += w;
                }
            }
            let a = sum[3] / area;
            let un = |v: f32| if a > 0.0 { to_srgb(v / area / a) } else { 0 };
            px.push([un(sum[0]), un(sum[1]), un(sum[2]), (a * 255.0).round() as u8]);
        }
    }
    Img { w: size, h: size, px }
}

/// The master without the dark outline around the arcs, with the arc weight changed by `grow`
/// px (negative thins) and the arcs moved down by `dy` px.
fn cleaned(src: &Img, grow: f32, dy: i32) -> Img {
    // How white a pixel is: 0 on the green disc and its darker outline, 1 on the arcs.
    let white: Vec<f32> = (0..src.w * src.h)
        .map(|i| {
            let y = (i / src.w) as i32 - dy;
            if y < 0 || y >= src.h as i32 {
                return 0.0;
            }
            let p = src.px[y as usize * src.w + i % src.w];
            ((p[0] as f32 - 96.0) / (255.0 - 96.0)).clamp(0.0, 1.0)
        })
        .collect();
    // The disc colour of each row: the mean of its opaque, clearly green pixels.
    let row_green: Vec<[f32; 3]> = (0..src.h)
        .map(|y| {
            let (mut s, mut n) = ([0f32; 3], 0f32);
            for x in 0..src.w {
                let p = src.px[y * src.w + x];
                if p[3] == 255 && p[0] < 90 && p[1] > 150 {
                    for k in 0..3 {
                        s[k] += p[k] as f32;
                    }
                    n += 1.0;
                }
            }
            if n > 0.0 { [s[0] / n, s[1] / n, s[2] / n] } else { [76.0, 175.0, 80.0] }
        })
        .collect();
    // Thicken (max filter) or thin (min filter) over a disc of radius |grow|, which keeps the
    // anti-aliased edge of the source.
    let thin = grow < 0.0;
    let grow = grow.abs();
    let r = grow.ceil() as i32;
    let mut grown = white.clone();
    if grow > 0.0 {
        for y in 0..src.h as i32 {
            for x in 0..src.w as i32 {
                let mut m: f32 = if thin { 1.0 } else { 0.0 };
                for dy in -r..=r {
                    let yy = y + dy;
                    if yy < 0 || yy >= src.h as i32 {
                        continue;
                    }
                    let span = ((grow * grow - (dy * dy) as f32).max(0.0)).sqrt() as i32;
                    for dx in -span..=span {
                        let xx = x + dx;
                        if xx >= 0 && xx < src.w as i32 {
                            let v = white[yy as usize * src.w + xx as usize];
                            m = if thin { m.min(v) } else { m.max(v) };
                        }
                    }
                }
                grown[y as usize * src.w + x as usize] = m;
            }
        }
    }
    let px = src
        .px
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let g = row_green[i / src.w];
            let w = grown[i];
            let mix = |k: usize| (g[k] + (255.0 - g[k]) * w).round() as u8;
            [mix(0), mix(1), mix(2), p[3]]
        })
        .collect();
    Img { w: src.w, h: src.h, px }
}

/// Arc / gap contrast of a small icon: how far the red channel swings between vertically
/// adjacent pixels inside the disc (arcs are white, gaps green), minus a penalty for arcs that
/// got too faint to read as white.
fn contrast(img: &Img) -> f32 {
    let mut swing = 0f32;
    let mut peak = 0f32;
    for x in img.w / 4..img.w * 3 / 4 {
        for y in 1..img.h {
            let (a, b) = (img.px[(y - 1) * img.w + x][0] as f32, img.px[y * img.w + x][0] as f32);
            swing += (a - b).abs();
            peak = peak.max(b);
        }
    }
    if peak < 225.0 { 0.0 } else { swing }
}

/// The best of every arc weight and vertical nudge (up to half a destination pixel) for `size`.
fn hinted(src: &Img, size: usize) -> Img {
    let half = (src.h / size / 2) as i32;
    let mut best: Option<(f32, f32, i32, Img)> = None;
    for &grow in &WEIGHTS {
        for dy in (-half..=half).step_by(4) {
            let img = downscale(&cleaned(src, grow, dy), size);
            let score = contrast(&img);
            if best.as_ref().is_none_or(|b| score > b.0) {
                best = Some((score, grow, dy, img));
            }
        }
    }
    let (score, grow, dy, img) = best.expect("at least one candidate");
    println!("{size} px: arc weight {grow:+} px, nudge {dy:+} px (contrast {score:.0})");
    img
}

/// A 32-bit BMP icon image (BITMAPINFOHEADER, bottom-up BGRA, then the 1-bit AND mask).
fn dib_bytes(img: &Img) -> Vec<u8> {
    let (w, h) = (img.w, img.h);
    let mut out = Vec::new();
    out.extend(40u32.to_le_bytes());
    out.extend((w as i32).to_le_bytes());
    out.extend((h as i32 * 2).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend([0u8; 24]);
    for y in (0..h).rev() {
        for p in &img.px[y * w..(y + 1) * w] {
            out.extend([p[2], p[1], p[0], p[3]]);
        }
    }
    let stride = w.div_ceil(32) * 4;
    for y in (0..h).rev() {
        let mut row = vec![0u8; stride];
        for x in 0..w {
            if img.px[y * w + x][3] == 0 {
                row[x / 8] |= 0x80 >> (x % 8);
            }
        }
        out.extend(row);
    }
    out
}

fn write_ico(path: &Path, images: &[Img]) {
    // 256 px is stored PNG-compressed, the smaller ones as BMP (the most compatible mix).
    let blobs: Vec<Vec<u8>> = images.iter().map(|i| if i.w >= 256 { png_bytes(i) } else { dib_bytes(i) }).collect();
    let mut f = BufWriter::new(File::create(path).expect("create ico"));
    f.write_all(&[0, 0, 1, 0]).unwrap();
    f.write_all(&(images.len() as u16).to_le_bytes()).unwrap();
    let mut offset = 6 + 16 * images.len();
    for (img, blob) in images.iter().zip(&blobs) {
        let dim = if img.w >= 256 { 0 } else { img.w as u8 };
        f.write_all(&[dim, dim, 0, 0]).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(&(blob.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&(offset as u32).to_le_bytes()).unwrap();
        offset += blob.len();
    }
    for blob in &blobs {
        f.write_all(blob).unwrap();
    }
}

/// Every size at 1:1 and the three small ones enlarged 6x, on a dark and a light taskbar colour.
fn sheet(images: &[Img]) -> Img {
    const ZOOM: usize = 6;
    let small: Vec<&Img> = images.iter().filter(|i| i.w <= 24).collect();
    let row_w: usize = images.iter().map(|i| i.w + 16).sum::<usize>() + 16;
    let zoom_w: usize = small.iter().map(|i| i.w * ZOOM + 16).sum::<usize>() + 16;
    let (w, band) = (row_w.max(zoom_w), 256 + 32 + 24 * ZOOM + 16);
    let mut out = Img { w, h: band * 2, px: vec![[0, 0, 0, 255]; w * band * 2] };
    for (b, bg) in [[32u8, 32, 32], [243, 243, 243]].into_iter().enumerate() {
        for p in &mut out.px[b * band * w..(b + 1) * band * w] {
            *p = [bg[0], bg[1], bg[2], 255];
        }
        let mut blit = |img: &Img, x0: usize, y0: usize, zoom: usize| {
            for y in 0..img.h * zoom {
                for x in 0..img.w * zoom {
                    let s = img.px[(y / zoom) * img.w + x / zoom];
                    let a = s[3] as f32 / 255.0;
                    let d = &mut out.px[(b * band + y0 + y) * w + x0 + x];
                    for k in 0..3 {
                        d[k] = (s[k] as f32 * a + d[k] as f32 * (1.0 - a)).round() as u8;
                    }
                }
            }
        };
        let mut x = 16;
        for img in images {
            blit(img, x, 16 + 256 - img.h, 1);
            x += img.w + 16;
        }
        let mut x = 16;
        for img in &small {
            blit(img, x, 256 + 32, ZOOM);
            x += img.w * ZOOM + 16;
        }
    }
    out
}

fn main() {
    let assets = Path::new("assets");
    let src = read_png(&assets.join("icon-source.png"));
    assert_eq!((src.w, src.h), (1024, 1024), "assets/icon-source.png must be 1024x1024");
    let plain = std::env::args().any(|a| a == "--plain");
    let images: Vec<Img> = SIZES
        .iter()
        .map(|&size| if SMALL.contains(&size) && !plain { hinted(&src, size) } else { downscale(&src, size) })
        .collect();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--profile") {
        // Red channel down three columns of the small sizes: arcs read high, gaps low.
        for img in images.iter().filter(|i| i.w <= 24) {
            println!("{} px", img.w);
            for y in 0..img.h {
                let row: Vec<String> = [img.w * 3 / 8, img.w / 2, img.w * 5 / 8].iter().map(|&x| format!("{:3}", img.px[y * img.w + x][0])).collect();
                println!("  {y:2}: {}", row.join(" "));
            }
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--sheet")
        && let Some(path) = args.get(i + 1)
    {
        std::fs::write(path, png_bytes(&sheet(&images))).expect("write sheet");
        println!("wrote {path}");
    }
    if plain {
        // Comparison only (small sizes straight from the master): nothing is written to assets.
        return;
    }
    for img in &images {
        std::fs::write(assets.join(format!("icon-{}.png", img.w)), png_bytes(img)).expect("write png");
    }
    write_ico(&assets.join("cadence.ico"), &images);
    println!("wrote assets/cadence.ico ({} sizes) and assets/icon-<n>.png", images.len());
}

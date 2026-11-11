//! Local audio processing in the sink, all on 44.1 kHz stereo f32: 6-band equalizer, mono, and
//! time-stretching (WSOLA) for podcast speed. Off = untouched samples and no CPU.
//! Memory: a few KB of filter state and a ~0.1 s stretch buffer.

use std::f32::consts::PI;
use std::sync::Mutex;

const RATE: f32 = 44_100.0;
pub const BANDS: [f32; 6] = [60.0, 150.0, 400.0, 1000.0, 2400.0, 15000.0];

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z: [[f32; 2]; 2],
}

impl Biquad {
    /// RBJ cookbook: low shelf (first band), high shelf (last), peaking in between.
    fn new(band: usize, gain_db: f32) -> Biquad {
        let f0 = BANDS[band];
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / RATE;
        let (sin, cos) = w0.sin_cos();
        let (b0, b1, b2, a0, a1, a2);
        if band == 0 || band == BANDS.len() - 1 {
            let alpha = sin / 2.0 * (2.0f32).sqrt();
            let sq = 2.0 * a.sqrt() * alpha;
            if band == 0 {
                b0 = a * ((a + 1.0) - (a - 1.0) * cos + sq);
                b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
                b2 = a * ((a + 1.0) - (a - 1.0) * cos - sq);
                a0 = (a + 1.0) + (a - 1.0) * cos + sq;
                a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
                a2 = (a + 1.0) + (a - 1.0) * cos - sq;
            } else {
                b0 = a * ((a + 1.0) + (a - 1.0) * cos + sq);
                b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
                b2 = a * ((a + 1.0) + (a - 1.0) * cos - sq);
                a0 = (a + 1.0) - (a - 1.0) * cos + sq;
                a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
                a2 = (a + 1.0) - (a - 1.0) * cos - sq;
            }
        } else {
            let alpha = sin / (2.0 * 1.0);
            b0 = 1.0 + alpha * a;
            b1 = -2.0 * cos;
            b2 = 1.0 - alpha * a;
            a0 = 1.0 + alpha / a;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha / a;
        }
        Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, z: [[0.0; 2]; 2] }
    }

    #[inline]
    fn run(&mut self, x: f32, ch: usize) -> f32 {
        // Transposed direct form II.
        let y = self.b0 * x + self.z[ch][0];
        self.z[ch][0] = self.b1 * x - self.a1 * y + self.z[ch][1];
        self.z[ch][1] = self.b2 * x - self.a2 * y;
        y
    }
}

/// WSOLA time-stretch: output plays `speed` times faster without changing pitch.
struct Stretch {
    input: Vec<[f32; 2]>,
    /// Read position in `input` (fractional hop accumulates here).
    pos: f64,
    tail: Vec<[f32; 2]>,
    /// The natural continuation of the last segment (search reference).
    reference: Vec<[f32; 2]>,
    window: Vec<f32>,
}

const N: usize = 1024;
const HS: usize = N / 2;
const SEARCH: usize = 256;

impl Stretch {
    fn new() -> Stretch {
        let window = (0..HS).map(|k| 0.5 - 0.5 * (PI * k as f32 / HS as f32).cos()).collect();
        Stretch { input: Vec::new(), pos: SEARCH as f64, tail: vec![[0.0; 2]; HS], reference: Vec::new(), window }
    }

    fn process(&mut self, speed: f32, frames: &[[f32; 2]], out: &mut Vec<f32>) {
        self.input.extend_from_slice(frames);
        let ha = HS as f64 * speed as f64;
        loop {
            let base = self.pos as usize;
            if base + SEARCH + N >= self.input.len() {
                break;
            }
            // Best offset: highest correlation with the reference (decimated by 4, step 2).
            let mut best = 0isize;
            if !self.reference.is_empty() {
                let mut best_score = f32::MIN;
                let mut d = -(SEARCH as isize);
                while d <= SEARCH as isize {
                    let start = (base as isize + d) as usize;
                    let mut score = 0.0;
                    for k in (0..HS).step_by(4) {
                        let a = &self.input[start + k];
                        let r = &self.reference[k];
                        score += (a[0] + a[1]) * (r[0] + r[1]);
                    }
                    if score > best_score {
                        best_score = score;
                        best = d;
                    }
                    d += 2;
                }
            }
            let start = (base as isize + best) as usize;
            for k in 0..HS {
                let w = self.window[k];
                let s = self.input[start + k];
                let t = self.tail[k];
                out.push(t[0] * (1.0 - w) + s[0] * w);
                out.push(t[1] * (1.0 - w) + s[1] * w);
            }
            self.tail.copy_from_slice(&self.input[start + HS..start + N]);
            self.reference.clear();
            self.reference.extend_from_slice(&self.input[start + HS..start + N]);
            self.pos += ha;
        }
        // Drop consumed input, keeping room for the search window.
        let drop = (self.pos as usize).saturating_sub(SEARCH);
        if drop > 0 {
            self.input.drain(..drop);
            self.pos -= drop as f64;
        }
    }
}

pub struct Dsp {
    eq_on: bool,
    gains: [f32; 6],
    filters: [Biquad; 6],
    preamp: f32,
    mono: bool,
    speed: f32,
    stretch: Option<Stretch>,
}

static DSP: Mutex<Option<Dsp>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Dsp) -> R) -> R {
    let mut g = DSP.lock().unwrap_or_else(|e| e.into_inner());
    let d = g.get_or_insert_with(|| Dsp { eq_on: false, gains: [0.0; 6], filters: [Biquad::new(0, 0.0); 6], preamp: 1.0, mono: false, speed: 1.0, stretch: None });
    f(d)
}

pub fn set_eq(on: bool, gains: [f32; 6]) {
    with(|d| {
        d.eq_on = on && gains.iter().any(|g| g.abs() > 0.05);
        d.gains = gains;
        for (i, f) in d.filters.iter_mut().enumerate() {
            let z = f.z;
            *f = Biquad::new(i, gains[i].clamp(-12.0, 12.0));
            f.z = z;
        }
        // Headroom for boosts so the EQ doesn't clip.
        let boost = gains.iter().cloned().fold(0.0f32, f32::max);
        d.preamp = 10f32.powf(-boost / 20.0);
    });
}

pub fn set_mono(on: bool) {
    with(|d| d.mono = on);
}

pub fn set_speed(speed: f32) {
    with(|d| {
        d.speed = speed.clamp(0.5, 2.0);
        if (d.speed - 1.0).abs() < 0.01 {
            d.stretch = None;
        } else if d.stretch.is_none() {
            d.stretch = Some(Stretch::new());
        }
    });
}

/// Interleaved stereo in, processed interleaved stereo out (may be shorter or longer).
pub fn process(samples: &mut Vec<f32>) {
    with(|d| {
        if d.eq_on {
            for fr in samples.chunks_exact_mut(2) {
                for (ch, v) in fr.iter_mut().enumerate() {
                    let mut x = *v * d.preamp;
                    for f in d.filters.iter_mut() {
                        x = f.run(x, ch);
                    }
                    *v = x;
                }
            }
        }
        if d.mono {
            for fr in samples.chunks_exact_mut(2) {
                let m = (fr[0] + fr[1]) * 0.5;
                fr[0] = m;
                fr[1] = m;
            }
        }
        if let Some(st) = d.stretch.as_mut() {
            let frames: Vec<[f32; 2]> = samples.chunks_exact(2).map(|f| [f[0], f[1]]).collect();
            let mut out = Vec::with_capacity((samples.len() as f32 / d.speed) as usize + 8);
            st.process(d.speed, &frames, &mut out);
            *samples = out;
        }
    });
}

/// A seek or track change: forget the stretch history so old audio doesn't bleed in.
pub fn flush() {
    with(|d| {
        if d.stretch.is_some() {
            d.stretch = Some(Stretch::new());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_shortens_at_double_speed() {
        set_speed(2.0);
        let mut total_out = 0;
        for _ in 0..50 {
            let mut buf: Vec<f32> = (0..4410).flat_map(|i| {
                let v = (i as f32 * 0.05).sin() * 0.3;
                [v, v]
            }).collect();
            process(&mut buf);
            total_out += buf.len() / 2;
        }
        let input = 50 * 4410;
        assert!(total_out > input * 4 / 10 && total_out < input * 6 / 10, "{total_out}");
        set_speed(1.0);
    }

    #[test]
    fn flat_eq_is_identity_and_boost_is_bounded() {
        set_eq(true, [0.0; 6]);
        let mut buf = vec![0.25f32, -0.25, 0.5, -0.5];
        let orig = buf.clone();
        process(&mut buf);
        assert_eq!(buf, orig);
        set_eq(true, [12.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let mut sine: Vec<f32> = (0..44100).flat_map(|i| { let v = (i as f32 * 2.0 * PI * 1000.0 / 44100.0).sin() * 0.9; [v, v] }).collect();
        process(&mut sine);
        // 1 kHz sits outside the 60 Hz shelf: the preamp keeps it below the input level.
        assert!(sine.iter().all(|v| v.abs() < 1.0));
        set_eq(false, [0.0; 6]);
    }
}

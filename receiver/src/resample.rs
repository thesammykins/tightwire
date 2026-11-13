//! Variable-ratio windowed-sinc resampler for drift compensation.
//!
//! Written here rather than pulled from a crate because the receiver needs a
//! pull-style interface (the ratio changes every callback and input frames are
//! fetched from the jitter buffer one at a time) with no allocation after
//! construction; rubato's chunked API would need extra buffering and latency.

use std::f64::consts::PI;

pub const MAX_CHANNELS: usize = 8;
/// Filter length. 48 taps gives a flat response to ~20 kHz at 48 kHz with
/// >90 dB image rejection while adding only TAPS/2 = 24 frames (0.5 ms) of delay.
pub const TAPS: usize = 48;
/// Kernel phases stored per input sample; intermediate phases are linearly
/// interpolated between neighbouring rows.
const PHASES: usize = 512;
const KAISER_BETA: f64 = 9.0;

/// Precomputed polyphase kernel for one cutoff.
pub struct KernelTable {
    /// (PHASES + 1) rows of TAPS coefficients.
    rows: Vec<f32>,
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..64 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl KernelTable {
    /// `cutoff` is the passband edge as a fraction of the *output* Nyquist
    /// frequency, expressed relative to the input rate via `scale` (= min(1,
    /// out_rate / in_rate)), so downsampling streams are band-limited first.
    pub fn new(scale: f64) -> Self {
        let fc = 0.5 * 0.94 * scale.min(1.0); // cycles per input sample
        let half = TAPS as f64 / 2.0;
        let i0b = bessel_i0(KAISER_BETA);
        let mut rows = vec![0f32; (PHASES + 1) * TAPS];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row = &mut rows[p * TAPS..(p + 1) * TAPS];
            let mut sum = 0.0;
            let mut vals = [0f64; TAPS];
            for (j, v) in vals.iter_mut().enumerate() {
                // Distance from output instant to input sample j (see `Resampler`).
                let x = half - 1.0 + frac - j as f64;
                let w = if x.abs() >= half {
                    0.0
                } else {
                    let r = x / half;
                    bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0b
                };
                let s = if x == 0.0 {
                    2.0 * fc
                } else {
                    (2.0 * PI * fc * x).sin() / (PI * x)
                };
                *v = s * w;
                sum += *v;
            }
            // Unity DC gain at every phase, otherwise the phase sweep under a
            // changing ratio would amplitude-modulate the signal.
            for (r, v) in row.iter_mut().zip(vals) {
                *r = (v / sum) as f32;
            }
        }
        Self { rows }
    }
}

/// Pull-style fractional resampler. Holds the last TAPS input frames; each
/// output frame is the kernel evaluated between history[TAPS/2-1] and
/// history[TAPS/2] at offset `frac`.
pub struct Resampler {
    channels: usize,
    /// Per channel, a doubled ring so any TAPS-long window is contiguous.
    hist: Vec<f32>,
    pos: usize,
    frac: f64,
}

impl Resampler {
    pub fn new() -> Self {
        Self {
            channels: 1,
            hist: vec![0.0; MAX_CHANNELS * 2 * TAPS],
            pos: 0,
            frac: 0.0,
        }
    }

    /// Clears history (used when the stream format changes).
    pub fn reset(&mut self, channels: usize) {
        self.channels = channels.clamp(1, MAX_CHANNELS);
        self.hist.fill(0.0);
        self.pos = 0;
        self.frac = 0.0;
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Fractional input position past the last frame pulled, in [0, 1):
    /// with the count of frames pulled it gives the read position without
    /// the ±1-frame quantisation of whole pulls.
    pub fn frac(&self) -> f64 {
        self.frac
    }

    fn push(&mut self, frame: &[f32; MAX_CHANNELS]) {
        for (c, &v) in frame.iter().enumerate().take(self.channels) {
            let base = c * 2 * TAPS;
            self.hist[base + self.pos] = v;
            self.hist[base + self.pos + TAPS] = v;
        }
        self.pos = (self.pos + 1) % TAPS;
    }

    /// Produces one output frame into `out` (stream channels), consuming input
    /// frames from `source` as the position advances by `ratio` (input frames
    /// per output frame).
    #[inline]
    pub fn next_frame(
        &mut self,
        table: &KernelTable,
        ratio: f64,
        out: &mut [f32; MAX_CHANNELS],
        mut source: impl FnMut(&mut [f32; MAX_CHANNELS]),
    ) {
        let phase = self.frac * PHASES as f64;
        let p = (phase as usize).min(PHASES - 1);
        let t = (phase - p as f64) as f32;
        let r0 = &table.rows[p * TAPS..(p + 1) * TAPS];
        let r1 = &table.rows[(p + 1) * TAPS..(p + 2) * TAPS];
        for (c, o) in out.iter_mut().enumerate().take(self.channels) {
            // Oldest-first window: hist[pos..pos+TAPS] in the doubled ring.
            let base = c * 2 * TAPS + self.pos;
            let h = &self.hist[base..base + TAPS];
            let mut a0 = 0f32;
            let mut a1 = 0f32;
            for j in 0..TAPS {
                a0 += h[j] * r0[j];
                a1 += h[j] * r1[j];
            }
            *o = a0 + (a1 - a0) * t;
        }
        self.frac += ratio;
        let mut frame = [0f32; MAX_CHANNELS];
        while self.frac >= 1.0 {
            self.frac -= 1.0;
            source(&mut frame);
            self.push(&frame);
        }
    }
}

impl Default for Resampler {
    fn default() -> Self {
        Self::new()
    }
}

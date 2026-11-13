//! A simulated output device on a virtual clock, for driving `Engine` the way
//! real drivers do rather than the way the file backend does.
//!
//! The sender emits fixed-size packets on its own crystal; the network adds
//! delay; the device calls `render` on a schedule built from a profile:
//! regular, in bursts (a coarse internal timer handing out several short
//! buffers back to back), with stalls (with or without catching up), at a
//! real rate offset from nominal, and with callback jitter. Everything is
//! deterministic (fixed-seed xorshift), so a failure reproduces exactly.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use tightwire_recv::engine::{self, Engine, EngineConfig, SharedStats, Slot};
use tightwire_recv::protocol::{AudioHeader, FLAG_DISCONTINUITY, SampleFormat};

pub const RATE: u32 = 48_000;

/// How the device schedules its callbacks.
#[derive(Debug, Clone, Copy)]
pub enum Cadence {
    /// One callback per buffer, when that buffer is due.
    Regular,
    /// A timer every `tick_s` hands out every buffer due before the next
    /// tick, back to back (`gap_s` apart).
    Burst { tick_s: f64, gap_s: f64 },
}

/// A device stall: no callbacks in [at_s, at_s + len_s).
#[derive(Debug, Clone, Copy)]
pub struct Stall {
    pub at_s: f64,
    pub len_s: f64,
    /// true: the missed buffers are requested back to back when it resumes;
    /// false: they are skipped (the device glitched and moved on).
    pub catch_up: bool,
}

#[derive(Debug, Clone)]
pub struct Device {
    /// Frames per callback.
    pub buffer_frames: usize,
    pub cadence: Cadence,
    /// Real consumption rate offset from nominal (ppm; + = consumes faster).
    pub rate_ppm: f64,
    /// Each callback is late by a uniform random 0..jitter_s.
    pub jitter_s: f64,
    pub stalls: Vec<Stall>,
}

impl Device {
    pub fn regular(buffer_frames: usize) -> Self {
        Self {
            buffer_frames,
            cadence: Cadence::Regular,
            rate_ppm: 0.0,
            jitter_s: 0.0,
            stalls: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sender {
    pub packet_frames: usize,
    /// Sender crystal offset (ppm; + = sends faster than nominal).
    pub ppm: f64,
    /// Network delay: base + uniform random 0..jitter_s.
    pub base_delay_s: f64,
    pub jitter_s: f64,
    /// Extra one-off delays: packets sent in [at, at+len) are held until at+len.
    pub holds: Vec<(f64, f64)>,
}

impl Sender {
    pub fn lan(packet_frames: usize) -> Self {
        Self {
            packet_frames,
            ppm: 0.0,
            base_delay_s: 0.0002,
            jitter_s: 0.0003,
            holds: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub min_target_s: f64,
    pub max_target_s: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_target_s: 0.0,
            max_target_s: 0.020,
        }
    }
}

/// One second of observations.
#[derive(Debug, Clone, Copy, Default)]
pub struct Second {
    pub t: f64,
    /// What the engine reports (and controls): stats depth.
    pub reported_ms: f64,
    /// Time-averaged real jitter-buffer depth: frames that have arrived
    /// minus the read position, sampled every 0.1 ms of virtual time.
    pub real_ms: f64,
    pub target_ms: f64,
    pub correction_ppm: f64,
    /// Fraction of the second the correction sat at its clamp.
    pub saturated: f64,
    pub underruns: u64,
    pub lost: u64,
    pub late: u64,
    pub trims: u64,
}

pub struct Run {
    pub seconds: Vec<Second>,
    /// Spells with the drift loop saturated: (start, length) in seconds.
    pub saturated_spells: Vec<(f64, f64)>,
    pub engine: Engine,
    pub stats: Arc<SharedStats>,
}

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Callback times and sizes for `secs` of virtual time.
fn schedule(d: &Device, secs: f64, rng: &mut Rng) -> Vec<f64> {
    let real_rate = RATE as f64 * (1.0 + d.rate_ppm * 1e-6);
    let buf_s = d.buffer_frames as f64 / real_rate;
    let mut out = Vec::new();
    let mut last = f64::MIN;
    let mut burst_tick = f64::NAN;
    let mut in_burst = 0usize;
    let mut k = 0u64;
    loop {
        let due = k as f64 * buf_s;
        k += 1;
        if due > secs {
            break;
        }
        let mut t = match d.cadence {
            Cadence::Regular => due,
            Cadence::Burst { tick_s, gap_s } => {
                // Handed out at the tick before it is due.
                let tick = (due / tick_s).floor() * tick_s;
                if tick != burst_tick {
                    burst_tick = tick;
                    in_burst = 0;
                }
                in_burst += 1;
                tick + (in_burst - 1) as f64 * gap_s
            }
        };
        let mut skip = false;
        for s in &d.stalls {
            if t >= s.at_s && t < s.at_s + s.len_s {
                if s.catch_up {
                    t = s.at_s + s.len_s;
                } else {
                    skip = true;
                }
            }
        }
        if skip {
            continue;
        }
        t += rng.unit() * d.jitter_s;
        // Callbacks never overtake each other; back-to-back ones are 20 µs apart.
        t = t.max(last + 20e-6);
        last = t;
        out.push(t);
    }
    out
}

fn header(seq: u32, ts: u64, channels: u8) -> AudioHeader {
    AudioHeader {
        stream_id: 7,
        sequence: seq,
        timestamp: ts,
        sample_rate: RATE,
        channels,
        format: SampleFormat::F32,
        flags: if seq == 0 { FLAG_DISCONTINUITY } else { 0 },
    }
}

/// Runs `secs` of virtual time; `each_callback` sees the engine after
/// every render.
pub fn run(
    sender: &Sender,
    device: &Device,
    limits: Limits,
    secs: f64,
    mut each_callback: impl FnMut(f64, &mut Engine),
) -> Run {
    let stats = Arc::new(SharedStats::default());
    let (mut tx, mut e) = engine::channel(
        EngineConfig {
            out_rate: RATE,
            out_channels: 1,
            period_frames: device.buffer_frames,
            min_target_s: limits.min_target_s,
            max_target_s: limits.max_target_s,
        },
        stats.clone(),
    );
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let callbacks = schedule(device, secs, &mut rng);
    let send_rate = RATE as f64 * (1.0 + sender.ppm * 1e-6);
    let pf = sender.packet_frames;
    let mut template = Slot::empty();
    template.len = pf;
    for i in 0..pf {
        template.samples[i] =
            (0.5 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48_000.0).sin()) as f32;
    }
    let mut buf = vec![0f32; device.buffer_frames];

    let mut seq = 0u32;
    let mut last_arrival = 0.0f64;
    let next_packet = |seq: u32, last_arrival: f64, rng: &mut Rng| -> f64 {
        let sent = (seq as f64 + 1.0) * pf as f64 / send_rate;
        let mut a = sent + sender.base_delay_s + rng.unit() * sender.jitter_s;
        for &(at, len) in &sender.holds {
            if sent >= at && sent < at + len {
                a = a.max(at + len);
            }
        }
        // FIFO network.
        a.max(last_arrival)
    };
    let mut pkt_t = next_packet(seq, last_arrival, &mut rng);
    let mut arrived_end = 0u64; // stream frames that have arrived
    let mut cb = 0usize;
    let mut sample_t = 0.0f64;
    const SAMPLE_DT: f64 = 1e-4;

    let mut seconds = Vec::new();
    let mut cur = Second::default();
    let (mut rep_sum, mut real_sum, mut n_samples) = (0.0, 0.0, 0u64);
    let (mut corr_sum, mut sat_n) = (0.0, 0u64);
    let mut second_end = 1.0;
    let mut spells = Vec::new();
    let mut spell_start: Option<f64> = None;

    loop {
        let cb_t = callbacks.get(cb).copied().unwrap_or(f64::INFINITY);
        let t = pkt_t.min(cb_t).min(sample_t);
        if t > secs {
            break;
        }
        if t == pkt_t {
            let mut s = template.clone();
            s.header = header(seq, seq as u64 * pf as u64, 1);
            s.arrival_s = pkt_t;
            assert!(tx.push(s).is_ok(), "ring full");
            arrived_end = (seq as u64 + 1) * pf as u64;
            last_arrival = pkt_t;
            seq += 1;
            pkt_t = next_packet(seq, last_arrival, &mut rng);
        } else if t == cb_t {
            e.render(&mut buf, cb_t);
            each_callback(cb_t, &mut e);
            cb += 1;
        } else {
            sample_t += SAMPLE_DT;
            if e.jitter().is_playing() {
                let real = arrived_end.saturating_sub(e.jitter().read_pos()) as f64 / RATE as f64;
                real_sum += real;
                rep_sum += stats.depth_us.load(Relaxed) as f64 / 1e6;
                corr_sum += stats.drift_ppb.load(Relaxed) as f64 * 1e-9;
                let sat = stats.saturated.load(Relaxed) == 1;
                sat_n += sat as u64;
                match (sat, spell_start) {
                    (true, None) => spell_start = Some(t),
                    (false, Some(s0)) => {
                        spells.push((s0, t - s0));
                        spell_start = None;
                    }
                    _ => {}
                }
                n_samples += 1;
            }
            if t >= second_end {
                let n = n_samples.max(1) as f64;
                let c = e.jitter().counters;
                cur.t = second_end;
                cur.reported_ms = rep_sum / n * 1e3;
                cur.real_ms = real_sum / n * 1e3;
                cur.target_ms = stats.target_us.load(Relaxed) as f64 / 1e3;
                cur.correction_ppm = corr_sum / n * 1e6;
                cur.saturated = sat_n as f64 / n;
                cur.underruns = c.underruns;
                cur.lost = c.lost;
                cur.late = c.late;
                cur.trims = c.trims;
                seconds.push(cur);
                (rep_sum, real_sum, n_samples, corr_sum, sat_n) = (0.0, 0.0, 0, 0.0, 0);
                second_end += 1.0;
            }
        }
    }
    if let Some(s0) = spell_start {
        spells.push((s0, secs - s0));
    }
    Run {
        seconds,
        saturated_spells: spells,
        engine: e,
        stats,
    }
}

pub fn print(name: &str, r: &Run) {
    for s in &r.seconds {
        eprintln!(
            "{name} t={:>4.0}s reported {:>6.2} real {:>6.2} target {:>6.2} corr {:>+8.1} sat {:>4.2} underruns {} trims {} lost {} late {}",
            s.t,
            s.reported_ms,
            s.real_ms,
            s.target_ms,
            s.correction_ppm,
            s.saturated,
            s.underruns,
            s.trims,
            s.lost,
            s.late
        );
    }
}

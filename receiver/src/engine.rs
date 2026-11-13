//! The render engine: everything that runs inside the output callback.
//!
//! Packets reach it through a wait-free SPSC ring of fixed-size slots (no
//! allocation per packet on either side). `render` drains the ring into the
//! jitter buffer, updates the drift controller and resamples into the device
//! buffer. It never allocates, locks or logs; observability goes out through
//! relaxed atomics in `SharedStats`.
//!
//! Depth is measured against what the device really played (0.2.1). 0.2.0
//! modelled playout as "where the last callback started, then nominal
//! speed", which is right for a device that calls back once per period on a
//! steady clock and wrong for most virtual endpoints: a device that hands
//! out several buffers from a coarse timer, stalls, or runs thousands of ppm
//! off nominal had its floor sized for one period (so every burst emptied
//! the buffer), a correction range too small to follow it, and a few ms of
//! bias in the measured depth. `devclock.rs` now fits the device's playout
//! line from its callbacks, the floor covers its bursts, large errors are
//! removed with one faded step instead of seconds of saturated pitch
//! correction, and the stats report what the device does.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::devclock::DeviceClock;
use crate::drift::DriftController;
use crate::jitter::{InsertOutcome, JitterBuffer, MAX_CHANNELS};
use crate::protocol::{AudioHeader, FLAG_MARKER, MAX_SAMPLES, SAMPLE_RATES};
use crate::resample::{KernelTable, Resampler, TAPS};

/// One received audio packet, converted to f32. Fixed size so the ring never
/// allocates.
#[derive(Clone)]
pub struct Slot {
    pub header: AudioHeader,
    pub arrival_s: f64,
    pub len: usize,
    pub samples: [f32; MAX_SAMPLES],
}

impl Slot {
    pub fn empty() -> Self {
        Self {
            header: AudioHeader {
                stream_id: 0,
                sequence: 0,
                timestamp: 0,
                sample_rate: 48_000,
                channels: 1,
                format: crate::protocol::SampleFormat::F32,
                flags: 0,
            },
            arrival_s: 0.0,
            len: 0,
            samples: [0.0; MAX_SAMPLES],
        }
    }
}

/// Time constant of the fast depth estimate that sizes discrete steps.
const FAST_TAU_S: f64 = 0.05;
/// A depth error this large (or this fraction of the target, whichever is
/// more; deficits get the larger pair) lasting `STEP_HOLD_S` is removed in
/// one step. Below it the drift loop's ±1000 ppm closes the gap within a
/// few seconds.
const STEP_MIN_S: f64 = 0.003;
const STEP_EXCESS: f64 = 0.25;
const STEP_DEFICIT_MIN_S: f64 = 0.002;
const STEP_DEFICIT: f64 = 0.25;
const STEP_HOLD_S: f64 = 0.25;
/// At most one step per this long, so a misbehaving device cannot turn
/// steps into a stream of dropouts.
const STEP_HOLDOFF_S: f64 = 1.0;

/// Ring depth in packets: ~0.5 s of 1 ms packets, so a stalled output thread
/// (e.g. device switching) does not immediately drop audio on the floor.
pub const RING_SLOTS: usize = 512;

#[derive(Default)]
pub struct SharedStats {
    pub depth_us: AtomicU32,
    pub target_us: AtomicU32,
    pub received: AtomicU64,
    pub lost: AtomicU64,
    pub late: AtomicU64,
    pub duplicates: AtomicU64,
    pub underruns: AtomicU64,
    pub resets: AtomicU64,
    pub concealed_frames: AtomicU64,
    pub drift_ppm: AtomicI32,
    pub stream_id: AtomicU32,
    pub stream_rate: AtomicU32,
    pub stream_channels: AtomicU32,
    pub playing: AtomicU32,
    /// Packets dropped because the SPSC ring was full (socket side).
    pub ring_full: AtomicU64,
    /// Output callbacks so far (lets the main thread see the device is alive).
    pub callbacks: AtomicU64,
    // Written by the socket thread.
    pub auth_dropped: AtomicU64,
    pub replayed: AtomicU64,
    pub filtered: AtomicU64,
    /// Measured arrival jitter (µs) feeding the adaptive target.
    pub jitter_us: AtomicU32,
    /// Its parts: typical (EWMA) and recent peak arrival delay (µs).
    pub jitter_mean_us: AtomicU32,
    pub jitter_peak_us: AtomicU32,
    /// Floor and cap the target is clamped to (µs), as currently applied.
    pub min_target_us: AtomicU32,
    pub max_target_us: AtomicU32,
    /// Resampling correction in parts per billion.
    pub drift_ppb: AtomicI32,
    /// Worst lateness of an output callback vs its schedule (µs); file backend only.
    pub output_late_us: AtomicU32,
    /// Last latency marker played: stream timestamp of its first frame and
    /// the receiver-clock time (µs, `clock::now_s`) that frame was handed to
    /// the output device. Written by the audio thread, read by the socket
    /// thread for the stats extension.
    pub marker_timestamp: AtomicU64,
    pub marker_output_us: AtomicU64,
    /// Floor/cap requested by the Mac (µs); applied by the audio thread when
    /// `tuning_generation` changes, so the callback never takes a lock.
    pub tuning_min_us: AtomicU32,
    pub tuning_max_us: AtomicU32,
    pub tuning_generation: AtomicU32,
    /// Overflow trims and the engine's discrete steps (skip / insert).
    pub trims: AtomicU64,
    pub skips: AtomicU64,
    pub inserts: AtomicU64,
    /// 1 while the drift loop is at a limit (`DriftController::saturated`).
    pub saturated: AtomicU32,
    /// Instantaneous jitter-buffer depth after the last callback (µs).
    pub real_depth_us: AtomicU32,
    /// The device's measured rate vs nominal (ppb; 0 until known) and how
    /// far it pulls ahead of its playout (µs, see devclock.rs).
    pub device_rate_ppb: AtomicI32,
    pub device_lead_us: AtomicU32,
    /// Callback cadence, accumulated by the audio thread and taken (swapped
    /// to zero) by whoever logs it: count, output frames, sum and max of the
    /// intervals between callbacks (µs).
    pub cadence_callbacks: AtomicU64,
    pub cadence_frames: AtomicU64,
    pub cadence_interval_sum_us: AtomicU64,
    pub cadence_interval_max_us: AtomicU32,
    /// Audio the output endpoint holds queued after a callback (WASAPI
    /// padding), averaged by the backend; µs at the output rate. Latency
    /// on top of the jitter buffer.
    pub device_queue_us: AtomicU32,
    /// 1 once the backend gave up on exclusive mode for this run (watchdog).
    pub exclusive_fallback: AtomicU32,
}

impl SharedStats {
    /// Asks the engine to use a new target floor and cap (any thread).
    pub fn request_limits(&self, min_target_us: u32, max_target_us: u32) {
        self.tuning_min_us.store(min_target_us, Relaxed);
        self.tuning_max_us.store(max_target_us, Relaxed);
        // Release: the engine that sees the new generation sees both values.
        self.tuning_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    pub out_rate: u32,
    pub out_channels: usize,
    /// Nominal device period in frames (for the jitter floor).
    pub period_frames: usize,
    pub min_target_s: f64,
    pub max_target_s: f64,
}

pub struct Engine {
    cfg: EngineConfig,
    rx: rtrb::Consumer<Slot>,
    jb: JitterBuffer,
    rs: Resampler,
    drift: DriftController,
    /// One kernel per possible stream rate, built up front so a format change
    /// in the callback never allocates.
    tables: Vec<(u32, KernelTable)>,
    table_idx: usize,
    stats: Arc<SharedStats>,
    /// What the device does: playout line, burst lead, rate.
    device: DeviceClock,
    /// Output frames rendered so far.
    out_frames: u64,
    /// After the previous render, if it ended playing: (its time, read
    /// position including the resampler's fraction, output frames rendered,
    /// ratio). See `drain`.
    anchor: Option<(f64, f64, u64, f64)>,
    margin_sum: f64,
    margin_n: u32,
    last_drift_update: f64,
    depth_est_s: Option<f64>,
    /// Faster-moving copy of the depth (τ = 50 ms) to size discrete steps.
    depth_fast_s: Option<f64>,
    /// Since when the depth has been far from the target (step pending).
    off_since: Option<f64>,
    last_step_at: f64,
    underruns_seen: u64,
    /// Read position at which the pending marker's first frame comes out of
    /// the resampler (marker timestamp + its TAPS/2 group delay).
    marker_due: Option<(u64, u64)>,
    tuning_seen: u32,
}

impl Engine {
    pub fn new(cfg: EngineConfig, rx: rtrb::Consumer<Slot>, stats: Arc<SharedStats>) -> Self {
        let tables = SAMPLE_RATES
            .iter()
            .map(|&r| (r, KernelTable::new(cfg.out_rate as f64 / r as f64)))
            .collect();
        let period_s = cfg.period_frames as f64 / cfg.out_rate as f64;
        Self {
            cfg,
            rx,
            jb: JitterBuffer::new(period_s, cfg.min_target_s, cfg.max_target_s),
            rs: Resampler::new(),
            drift: DriftController::new(),
            tables,
            table_idx: 1,
            stats,
            device: DeviceClock::new(cfg.out_rate as f64, cfg.period_frames),
            out_frames: 0,
            anchor: None,
            margin_sum: 0.0,
            margin_n: 0,
            last_drift_update: 0.0,
            depth_est_s: None,
            depth_fast_s: None,
            off_since: None,
            last_step_at: f64::MIN,
            underruns_seen: 0,
            marker_due: None,
            tuning_seen: 0,
        }
    }

    pub fn stats(&self) -> &Arc<SharedStats> {
        &self.stats
    }

    pub fn config(&self) -> EngineConfig {
        self.cfg
    }

    pub fn jitter(&self) -> &JitterBuffer {
        &self.jb
    }

    pub fn drift(&self) -> &DriftController {
        &self.drift
    }

    pub fn device(&self) -> &DeviceClock {
        &self.device
    }

    /// Moves queued packets into the jitter buffer and measures each one's
    /// arrival margin: how far its last frame was ahead of the playout point
    /// at the moment it arrived. The playout point is the read position
    /// after the previous render, moved along the device's fitted playout
    /// line (`devclock.rs`) to the arrival time — behind that read position
    /// by whatever the device pulled ahead. Sampling depth only at render
    /// instants instead would alias packet-arrival sawtooth against the
    /// device period into a slow beat (at the clock-offset frequency) that
    /// the drift loop would chase.
    fn drain(&mut self) {
        while let Ok(slot) = self.rx.pop() {
            let samples = &slot.samples[..slot.len];
            let outcome = self.jb.insert(&slot.header, samples, slot.arrival_s);
            if slot.header.flags & FLAG_MARKER != 0
                && matches!(outcome, InsertOutcome::Stored | InsertOutcome::Reset)
            {
                let ts = slot.header.timestamp;
                self.marker_due = Some((ts, ts + TAPS as u64 / 2));
            }
            match outcome {
                InsertOutcome::Reset => {
                    let rate = slot.header.sample_rate;
                    self.table_idx = self
                        .tables
                        .iter()
                        .position(|(r, _)| *r == rate)
                        .unwrap_or(1);
                    self.rs.reset(slot.header.channels as usize);
                    self.drift.reset_depth();
                    self.anchor = None;
                    self.margin_sum = 0.0;
                    self.margin_n = 0;
                    self.depth_est_s = None;
                    self.depth_fast_s = None;
                    self.off_since = None;
                    self.marker_due = None;
                }
                InsertOutcome::Stored if self.jb.is_playing() => {
                    if let Some((t, pos, out, ratio)) = self.anchor {
                        let frames = (slot.len / slot.header.channels as usize) as f64;
                        // A packet stamped before the previous render but
                        // queued after it could not have been played then;
                        // count it from that render, as 0.2.0 did.
                        let at = slot.arrival_s.max(t);
                        let played = self.device.line_at(at) - out as f64;
                        let playout = pos + played * ratio;
                        let end = slot.header.timestamp as f64 + frames;
                        // Mean depth over the packet interval is margin - frames/2.
                        self.margin_sum += (end - playout - frames / 2.0) / self.jb.rate();
                        self.margin_n += 1;
                    }
                }
                _ => {}
            }
        }
    }

    /// Fills `out` (interleaved, `out_channels` wide) with audio. `now_s` is
    /// the callback time on the same monotonic clock as `Slot::arrival_s`.
    pub fn render(&mut self, out: &mut [f32], now_s: f64) {
        self.apply_tuning();
        let och = self.cfg.out_channels;
        let frames_out = out.len() / och;
        self.device.observe(now_s, frames_out, self.out_frames);
        self.jb.set_device_lead(self.device.lead_s());
        self.drain();
        let nominal = self.jb.rate() / self.cfg.out_rate as f64;
        if self.margin_n > 0 {
            let depth = self.margin_sum / self.margin_n as f64;
            self.margin_sum = 0.0;
            self.margin_n = 0;
            let dt = if self.depth_est_s.is_some() {
                now_s - self.last_drift_update
            } else {
                0.0
            };
            self.last_drift_update = now_s;
            self.depth_fast_s = Some(match self.depth_fast_s {
                Some(f) => f + (depth - f) * (dt.max(0.0) / FAST_TAU_S).min(1.0),
                None => depth,
            });
            self.drift.update(depth, self.jb.target_s(), dt.max(0.0));
            self.depth_est_s = self.drift.smoothed_depth_s();
            self.consider_step(now_s);
        }
        if !self.jb.is_playing() {
            self.depth_est_s = None;
            self.depth_fast_s = None;
            self.off_since = None;
        }
        let correction = self.drift.correction();
        let ratio = nominal * (1.0 + correction);
        let playing_before = self.jb.is_playing();
        if !playing_before {
            // Priming refills to the current target; restart the depth and
            // target filters there so a target jump is not integrated as error.
            self.drift.reset_depth();
        }

        let table = &self.tables[self.table_idx].1;
        let sch = self.rs.channels();
        let mut frame = [0f32; MAX_CHANNELS];
        let out_rate = self.cfg.out_rate as f64;
        for (i, o) in out.chunks_exact_mut(och).enumerate() {
            if let Some((ts, due)) = self.marker_due
                && self.jb.read_pos() >= due
            {
                // This output frame carries the marker's first frame.
                self.marker_due = None;
                let at = now_s + i as f64 / out_rate;
                self.stats
                    .marker_output_us
                    .store((at * 1e6) as u64, Relaxed);
                self.stats
                    .marker_timestamp
                    .store(ts, std::sync::atomic::Ordering::Release);
            }
            let jb = &mut self.jb;
            self.rs
                .next_frame(table, ratio, &mut frame, |f| jb.next_frame(f));
            if sch == 1 {
                o.fill(frame[0]);
            } else {
                for (c, v) in o.iter_mut().enumerate() {
                    *v = if c < sch { frame[c] } else { 0.0 };
                }
            }
        }
        self.out_frames += frames_out as u64;
        let underruns = self.jb.counters.underruns;
        if underruns != self.underruns_seen {
            // An underrun is a deficit step the device forced: the depth
            // fell by about the target since the last step. Two in a row
            // teach the loop the rate that keeps causing them.
            self.underruns_seen = underruns;
            self.drift.stepped(-self.jb.target_s(), now_s);
        }
        // The anchor for the next interval's arrivals: where the stream read
        // position and the device's output count stand now. Only valid while
        // playing; a render that ended priming anchors where playback began.
        self.anchor = self.jb.is_playing().then_some((
            now_s,
            self.jb.read_pos() as f64 + self.rs.frac(),
            self.out_frames,
            ratio,
        ));
        self.publish(correction);
    }

    /// Removes a large, lasting depth error in one faded step instead of
    /// seconds of saturated pitch correction: a step of 20 ms would take the
    /// ±1000 ppm loop 20 s. The cost is one fade-out/fade-in (~1.3 ms dip)
    /// plus the skipped or inserted audio, once per disturbance.
    fn consider_step(&mut self, now_s: f64) {
        let (Some(fast), Some(target)) = (self.depth_fast_s, self.drift.target_s()) else {
            return;
        };
        let err = fast - target;
        // Too deep costs latency; too shallow risks an underrun.
        let limit = if err > 0.0 {
            STEP_MIN_S.max(STEP_EXCESS * target)
        } else {
            STEP_DEFICIT_MIN_S.max(STEP_DEFICIT * target)
        };
        if err.abs() < limit || self.jb.step_pending() {
            self.off_since = None;
            return;
        }
        let since = *self.off_since.get_or_insert(now_s);
        if now_s - since < STEP_HOLD_S || now_s - self.last_step_at < STEP_HOLDOFF_S {
            return;
        }
        let frames = (err * self.jb.rate()).round() as i64;
        if self.jb.request_step(frames) {
            self.last_step_at = now_s;
            self.off_since = None;
            self.drift.stepped(err, now_s);
            self.depth_est_s = self.drift.smoothed_depth_s();
            self.depth_fast_s = Some(fast - err);
        }
    }

    fn apply_tuning(&mut self) {
        let g = self
            .stats
            .tuning_generation
            .load(std::sync::atomic::Ordering::Acquire);
        if g != self.tuning_seen {
            self.tuning_seen = g;
            let min = self.stats.tuning_min_us.load(Relaxed) as f64 / 1e6;
            let max = self.stats.tuning_max_us.load(Relaxed) as f64 / 1e6;
            self.jb.set_limits(min, max);
        }
    }

    /// Smoothed mean jitter-buffer depth while playing.
    pub fn depth_s(&self) -> Option<f64> {
        self.depth_est_s
    }

    fn publish(&mut self, correction: f64) {
        let cad = self.device.take_cadence();
        let s = &self.stats;
        s.cadence_callbacks.fetch_add(cad.callbacks, Relaxed);
        s.cadence_frames.fetch_add(cad.frames, Relaxed);
        s.cadence_interval_sum_us
            .fetch_add((cad.interval_sum_s * 1e6) as u64, Relaxed);
        s.cadence_interval_max_us
            .fetch_max((cad.interval_max_s * 1e6) as u32, Relaxed);
        s.device_rate_ppb.store(
            self.device
                .rate_ppm()
                .map_or(0, |p| (p * 1e3).round() as i32),
            Relaxed,
        );
        s.device_lead_us
            .store((self.device.lead_s() * 1e6) as u32, Relaxed);
        s.saturated.store(self.drift.saturated() as u32, Relaxed);
        let c = &self.jb.counters;
        let rate = self.jb.rate();
        let depth = self
            .depth_est_s
            .unwrap_or(self.jb.depth_frames() as f64 / rate);
        s.depth_us.store((depth * 1e6) as u32, Relaxed);
        // The target being steered to (upward jumps glide, falls are
        // rate-limited; see drift.rs), which is what the depth should match.
        let target = self
            .drift
            .target_s()
            .filter(|_| self.jb.is_playing())
            .unwrap_or_else(|| self.jb.target_s());
        s.target_us.store((target * 1e6) as u32, Relaxed);
        s.jitter_us
            .store((self.jb.jitter_s() * 1e6) as u32, Relaxed);
        s.jitter_mean_us
            .store((self.jb.jitter_mean_s() * 1e6) as u32, Relaxed);
        s.jitter_peak_us
            .store((self.jb.jitter_peak_s() * 1e6) as u32, Relaxed);
        let (min, max) = self.jb.limits_s();
        s.min_target_us.store((min * 1e6) as u32, Relaxed);
        s.max_target_us.store((max * 1e6) as u32, Relaxed);
        s.drift_ppb
            .store((correction * 1e9).round() as i32, Relaxed);
        s.received.store(c.received, Relaxed);
        s.lost.store(c.lost, Relaxed);
        s.late.store(c.late, Relaxed);
        s.duplicates.store(c.duplicates, Relaxed);
        s.underruns.store(c.underruns, Relaxed);
        s.resets.store(c.resets, Relaxed);
        s.concealed_frames.store(c.concealed_frames, Relaxed);
        s.trims.store(c.trims, Relaxed);
        s.skips.store(c.skips, Relaxed);
        s.inserts.store(c.inserts, Relaxed);
        s.real_depth_us
            .store((self.jb.depth_frames() as f64 / rate * 1e6) as u32, Relaxed);
        s.drift_ppm
            .store((correction * 1e6).round() as i32, Relaxed);
        if let Some(f) = self.jb.format() {
            s.stream_id.store(f.stream_id as u32, Relaxed);
            s.stream_rate.store(f.sample_rate, Relaxed);
            s.stream_channels.store(f.channels as u32, Relaxed);
        }
        s.playing.store(self.jb.is_playing() as u32, Relaxed);
        s.callbacks.fetch_add(1, Relaxed);
    }
}

/// Creates the producer/engine pair.
pub fn channel(cfg: EngineConfig, stats: Arc<SharedStats>) -> (rtrb::Producer<Slot>, Engine) {
    let (tx, rx) = rtrb::RingBuffer::new(RING_SLOTS);
    (tx, Engine::new(cfg, rx, stats))
}

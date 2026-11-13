//! Timestamp-indexed jitter buffer (SPEC §5).
//!
//! Lives entirely on the audio callback thread: packets arrive through the SPSC
//! ring and are written straight into a frame ring indexed by stream timestamp,
//! so reordering and duplicates need no sorting. All storage is allocated in
//! `new`; nothing here allocates, locks or blocks afterwards.

use crate::protocol::AudioHeader;

pub const MAX_CHANNELS: usize = 8;

/// Frame-ring size. Must be a power of two and comfortably above the largest
/// `--max-ms` at 96 kHz.
pub const CAPACITY: usize = 1 << 15;
const MASK: u64 = (CAPACITY - 1) as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub stream_id: u16,
    pub sample_rate: u32,
    pub channels: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Stored,
    /// Stream id, format or discontinuity flag changed: buffer was flushed.
    Reset,
    Duplicate,
    Late,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub received: u64,
    pub lost: u64,
    pub late: u64,
    pub duplicates: u64,
    pub underruns: u64,
    pub resets: u64,
    /// Frames played as silence/fade because their packet never arrived.
    pub concealed_frames: u64,
    /// Times the buffer was faded out and skipped forward because it
    /// overflowed (a burst of arrivals), and the frames discarded.
    pub trims: u64,
    pub trimmed_frames: u64,
    /// Discrete depth corrections requested by the engine: fade, skip
    /// forward (too deep) or fade, insert silence (too shallow), fade in.
    pub skips: u64,
    pub skipped_frames: u64,
    pub inserts: u64,
    pub inserted_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Priming,
    Playing,
}

/// Estimates how far behind the earliest-ever packet arrivals can run.
/// `excess` = arrival transit time minus the minimum transit seen, tracked as
/// an EWMA (typical) and a slowly decaying peak (recent worst case).
#[derive(Debug, Clone, Copy)]
struct JitterEstimator {
    base: f64,
    last_arrival: f64,
    ewma: f64,
    peak: f64,
    primed: bool,
}

/// The minimum-transit reference may creep upward this fast (s/s) so it can
/// follow clock drift (≤ 1000 ppm) and route changes without staying stuck low.
const BASE_LEAK: f64 = 2e-3;
/// Peak decays with this time constant (s): a burst raises the target at once
/// and lets it relax only after a few calm seconds.
const PEAK_DECAY_S: f64 = 8.0;
/// Added to the SPEC floor (packet + period). Mean depth sits at the target,
/// but the packet and period sawtooths take it down to target − (P+O)/2, so
/// at the bare floor a 0.5 ms hiccup the arrival-jitter estimator never saw
/// (output-side scheduling, drift-loop lag) already underruns. See SPEC
/// deviations.
const SAFETY_MARGIN_S: f64 = 0.0005;
/// Depth above 2 × target + this is trimmed (faded, skipped) back to target.
const TRIM_SLACK_S: f64 = 0.002;

/// A depth correction waiting for the fade-out to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    None,
    Skip(u64),
    Insert(u64),
}

impl JitterEstimator {
    fn new() -> Self {
        Self {
            base: 0.0,
            last_arrival: 0.0,
            ewma: 0.0,
            peak: 0.0,
            primed: false,
        }
    }

    fn observe(&mut self, arrival_s: f64, media_s: f64) {
        let transit = arrival_s - media_s;
        if !self.primed {
            *self = Self {
                base: transit,
                last_arrival: arrival_s,
                ewma: 0.0,
                peak: 0.0,
                primed: true,
            };
            return;
        }
        let dt = (arrival_s - self.last_arrival).max(0.0);
        self.last_arrival = arrival_s;
        self.base = (self.base + BASE_LEAK * dt).min(transit);
        let excess = transit - self.base;
        self.ewma += (excess - self.ewma) / 16.0;
        self.peak = (self.peak * (-dt / PEAK_DECAY_S).exp()).max(excess);
    }

    /// Extra buffering (s) needed beyond the floor.
    fn needed(&self) -> f64 {
        self.peak.max(3.0 * self.ewma)
    }
}

pub struct JitterBuffer {
    data: Vec<f32>,
    present: Vec<bool>,
    format: Option<StreamFormat>,
    state: State,
    read_pos: u64,
    write_end: u64,
    start_seq: u32,
    highest_seq: u32,
    /// Bit i set = sequence (highest_seq - i) has been received.
    seq_window: u128,
    gain: f32,
    gain_step: f32,
    fade_frames: u64,
    /// Decaying copy of the last output, used when audio stops without warning.
    tail: [f32; MAX_CHANNELS],
    last_out: [f32; MAX_CHANNELS],
    tail_decay: f32,
    jitter: JitterEstimator,
    last_packet_frames: usize,
    /// How far the output device pulls frames ahead of its playout (s):
    /// half a period for a steady device, half a burst for a bursty one
    /// (`devclock.rs`). The target floor includes it once.
    device_lead_s: f64,
    min_target_s: f64,
    max_target_s: f64,
    /// Read position at the most recent priming → playing transition.
    play_start_pos: u64,
    step: Step,
    /// Frames of silence still to insert before playing on.
    inserting: u64,
    pub counters: Counters,
}

impl JitterBuffer {
    pub fn new(output_period_s: f64, min_target_s: f64, max_target_s: f64) -> Self {
        Self {
            data: vec![0.0; CAPACITY * MAX_CHANNELS],
            present: vec![false; CAPACITY],
            format: None,
            state: State::Idle,
            read_pos: 0,
            write_end: 0,
            start_seq: 0,
            highest_seq: 0,
            seq_window: 0,
            gain: 0.0,
            gain_step: 1.0 / 32.0,
            fade_frames: 32,
            tail: [0.0; MAX_CHANNELS],
            last_out: [0.0; MAX_CHANNELS],
            tail_decay: 0.9,
            jitter: JitterEstimator::new(),
            last_packet_frames: 0,
            device_lead_s: output_period_s / 2.0,
            play_start_pos: 0,
            step: Step::None,
            inserting: 0,
            min_target_s,
            max_target_s,
            counters: Counters::default(),
        }
    }

    pub fn format(&self) -> Option<StreamFormat> {
        self.format
    }

    pub fn is_playing(&self) -> bool {
        self.state == State::Playing
    }

    /// Read position where playback last (re)started.
    pub fn play_start_pos(&self) -> u64 {
        self.play_start_pos
    }

    /// Stream timestamp of the next frame to be read.
    pub fn read_pos(&self) -> u64 {
        self.read_pos
    }

    /// Frames buffered ahead of the read position.
    pub fn depth_frames(&self) -> u64 {
        self.write_end.saturating_sub(self.read_pos)
    }

    pub fn rate(&self) -> f64 {
        self.format.map_or(48_000.0, |f| f.sample_rate as f64)
    }

    /// Adaptive target depth in seconds: one packet plus how far the output
    /// device pulls ahead of its playout (half a period, or half a burst)
    /// plus a fixed safety margin, plus measured arrival jitter, clamped to
    /// [--target-ms, --max-ms]. The depth is the time average measured
    /// against the device's playout line, so its minimum just before a
    /// pull is about target − packet/2 − lead: the floor leaves half a
    /// packet plus the margin. 0.2.0's "packet + one period" floor was the
    /// same real depth for a steady device, because its measurement read
    /// half a period high.
    pub fn target_s(&self) -> f64 {
        (self.floor_s() + self.jitter.needed())
            .max(self.min_target_s)
            .min(self.max_target_s)
    }

    /// The adaptive floor before jitter and limits.
    fn floor_s(&self) -> f64 {
        self.last_packet_frames as f64 / self.rate() + self.device_lead_s + SAFETY_MARGIN_S
    }

    /// Sets how far the output device pulls ahead of its playout (s); see
    /// `devclock::DeviceClock::lead_s`.
    pub fn set_device_lead(&mut self, lead_s: f64) {
        self.device_lead_s = lead_s;
    }

    /// Asks for a one-off depth correction: positive frames are skipped,
    /// negative frames of silence inserted, each between a fade-out and a
    /// fade-in. Ignored while another is pending or not playing.
    pub fn request_step(&mut self, frames: i64) -> bool {
        if self.state != State::Playing || self.step != Step::None || self.inserting > 0 {
            return false;
        }
        self.step = match frames {
            f if f > 0 => Step::Skip(f as u64),
            f if f < 0 => Step::Insert(f.unsigned_abs()),
            _ => return false,
        };
        true
    }

    pub fn step_pending(&self) -> bool {
        self.step != Step::None || self.inserting > 0
    }

    /// Recent worst arrival delay beyond the fastest packet (s).
    pub fn jitter_s(&self) -> f64 {
        self.jitter.needed()
    }

    /// Typical (EWMA) arrival delay beyond the fastest packet (s).
    pub fn jitter_mean_s(&self) -> f64 {
        self.jitter.ewma
    }

    /// Slowly decaying peak of that delay (s).
    pub fn jitter_peak_s(&self) -> f64 {
        self.jitter.peak
    }

    /// Configured floor and cap of the target (s).
    pub fn limits_s(&self) -> (f64, f64) {
        (self.min_target_s, self.max_target_s)
    }

    /// Changes the floor and cap while running (Mac tuning message). The
    /// target follows on the next frame; the drift loop glides to it.
    pub fn set_limits(&mut self, min_target_s: f64, max_target_s: f64) {
        self.min_target_s = min_target_s;
        self.max_target_s = max_target_s.max(min_target_s);
    }

    /// Instantaneous depth to (re)start or trim to: the target plus the
    /// device's lead. The target is a time average against the device's
    /// playout line; right before the device pulls its next period or
    /// burst the buffer holds about this much, and right after it about
    /// target − lead. Starting at the bare target would let a device that
    /// pulls 10 ms at once empty a 6.5 ms buffer on its first pull.
    pub fn fill_frames(&self) -> u64 {
        ((self.target_s() + self.device_lead_s) * self.rate()).round() as u64
    }

    pub fn target_frames(&self) -> u64 {
        (self.target_s() * self.rate()).round() as u64
    }

    fn reset(&mut self, format: StreamFormat, h: &AudioHeader) {
        self.present.fill(false);
        self.format = Some(format);
        self.state = State::Priming;
        self.read_pos = h.timestamp;
        self.write_end = h.timestamp;
        self.start_seq = h.sequence;
        self.highest_seq = h.sequence.wrapping_sub(1);
        self.seq_window = 0;
        self.jitter = JitterEstimator::new();
        self.step = Step::None;
        self.inserting = 0;
        // Fade length ~0.67 ms regardless of rate.
        self.fade_frames = (format.sample_rate as u64 / 1500).max(8);
        self.gain_step = 1.0 / self.fade_frames as f32;
        // Tail falls by ~20 dB over one fade length.
        self.tail_decay = 0.1f32.powf(1.0 / self.fade_frames as f32);
        self.start_tail();
        self.counters.resets += 1;
    }

    fn start_tail(&mut self) {
        let ch = self.format.map_or(1, |f| f.channels);
        self.tail[..ch].copy_from_slice(&self.last_out[..ch]);
        self.gain = 0.0;
    }

    /// Stores one packet. `arrival_s` is the receive time on a monotonic clock.
    pub fn insert(&mut self, h: &AudioHeader, samples: &[f32], arrival_s: f64) -> InsertOutcome {
        let channels = h.channels as usize;
        let format = StreamFormat {
            stream_id: h.stream_id,
            sample_rate: h.sample_rate,
            channels,
        };
        let frames = samples.len() / channels;
        let mut outcome = InsertOutcome::Stored;
        // A repeated discontinuity flag on the stream's first sequence is a
        // duplicate of the start packet, not a new start.
        let restart =
            h.discontinuity() && !(self.format == Some(format) && h.sequence == self.start_seq);
        if self.format != Some(format) || restart {
            self.reset(format, h);
            outcome = InsertOutcome::Reset;
        }
        self.counters.received += 1;
        self.last_packet_frames = frames;

        // Sequence bookkeeping: loss is counted when a sequence is skipped and
        // taken back if the packet turns up later (reordered or late).
        let ahead = h.sequence.wrapping_sub(self.highest_seq);
        if ahead != 0 && ahead < 1 << 31 {
            self.counters.lost += (ahead - 1) as u64;
            self.seq_window = if ahead >= 128 {
                0
            } else {
                self.seq_window << ahead
            };
            self.seq_window |= 1;
            self.highest_seq = h.sequence;
        } else {
            let behind = self.highest_seq.wrapping_sub(h.sequence);
            if behind >= 128 {
                self.counters.late += 1;
                return InsertOutcome::Late;
            }
            let bit = 1u128 << behind;
            if self.seq_window & bit != 0 {
                self.counters.duplicates += 1;
                return InsertOutcome::Duplicate;
            }
            self.seq_window |= bit;
            self.counters.lost = self.counters.lost.saturating_sub(1);
        }
        // Every distinct packet counts toward jitter, including reordered
        // ones: their delay is exactly what the buffer has to absorb.
        self.jitter
            .observe(arrival_s, h.timestamp as f64 / h.sample_rate as f64);

        let start = h.timestamp;
        let end = start.saturating_add(frames as u64);
        if end <= self.read_pos {
            self.counters.late += 1;
            return InsertOutcome::Late;
        }
        if end - self.read_pos > CAPACITY as u64 {
            // Sender jumped far ahead (e.g. long stall): restart from here.
            self.reset(format, h);
            self.highest_seq = h.sequence;
            self.seq_window = 1;
            outcome = InsertOutcome::Reset;
        }
        let first = self.read_pos.saturating_sub(start) as usize;
        for f in first..frames {
            let ts = start + f as u64;
            let slot = (ts & MASK) as usize;
            let dst = &mut self.data[slot * MAX_CHANNELS..slot * MAX_CHANNELS + channels];
            dst.copy_from_slice(&samples[f * channels..(f + 1) * channels]);
            self.present[slot] = true;
        }
        self.write_end = self.write_end.max(end);
        outcome
    }

    fn present_at(&self, ts: u64) -> bool {
        ts < self.write_end && self.present[(ts & MASK) as usize]
    }

    /// Produces the next input frame for the resampler. Advances only while
    /// playing; while priming it emits silence (plus any decaying tail).
    pub fn next_frame(&mut self, out: &mut [f32; MAX_CHANNELS]) {
        let Some(fmt) = self.format else {
            out.fill(0.0);
            return;
        };
        let ch = fmt.channels;
        if self.state == State::Priming {
            let depth = self.depth_frames();
            let target = self.fill_frames();
            if depth >= target.max(1) {
                // Start with exactly `target` buffered so latency starts low.
                self.skip_to(self.write_end - target);
                self.play_start_pos = self.read_pos;
                self.state = State::Playing;
                self.gain = 0.0;
            }
        }
        if self.state == State::Playing && self.inserting > 0 {
            // Inserting silence: the device plays on, the stream waits.
            self.inserting -= 1;
            self.emit_tail_only(out, ch);
            return;
        }
        if self.state == State::Playing && self.read_pos >= self.write_end {
            self.counters.underruns += 1;
            self.state = State::Priming;
            self.step = Step::None;
            if self.gain > 0.0 {
                self.start_tail();
            }
        }
        if self.state != State::Playing {
            self.emit_tail_only(out, ch);
            return;
        }

        // Trim back to the target when the buffer is far above it (a burst
        // after a stall), not only at the hard cap: the drift loop can only
        // drain 1 ms per second, and this is a live-monitoring path. Never
        // closer to the target than the depth's own sawtooth (one packet
        // plus one device burst) and some slack: at the cap, target and
        // cap coincide, and 0.2.0 then trimmed every sawtooth peak — a fade
        // every few ms ("static") that also held the mean below target.
        let rate = fmt.sample_rate as f64;
        let target = self.target_s();
        let headroom =
            self.last_packet_frames as f64 / rate + 2.0 * self.device_lead_s + TRIM_SLACK_S;
        let trim_s = (2.0 * target + TRIM_SLACK_S)
            .min(self.max_target_s)
            .max(target + headroom);
        let overfull = self.depth_frames() as f64 > trim_s * rate;
        let stepping = self.step != Step::None;
        let here = self.present_at(self.read_pos);
        // Look one fade length ahead: a known hole there (data beyond it has
        // arrived) means fade out now so the gain reaches zero at the hole.
        // Frames past write_end are simply not here yet, not holes.
        let ahead = self.read_pos + self.fade_frames;
        let ahead_ok = ahead >= self.write_end || self.present[(ahead & MASK) as usize];
        let want = if here && ahead_ok && !overfull && !stepping {
            1.0
        } else {
            0.0
        };

        if !here && self.gain > 0.0 {
            // Hole without warning (lookahead was not possible): hand off to tail.
            self.start_tail();
        }
        if want > self.gain {
            self.gain = (self.gain + self.gain_step).min(1.0);
        } else if want < self.gain {
            self.gain = (self.gain - self.gain_step).max(0.0);
        }

        let slot = (self.read_pos & MASK) as usize;
        let src = &self.data[slot * MAX_CHANNELS..slot * MAX_CHANNELS + ch];
        for c in 0..ch {
            let v = if here { src[c] * self.gain } else { 0.0 };
            out[c] = v + self.tail[c];
            self.tail[c] *= self.tail_decay;
            self.last_out[c] = out[c];
        }
        if !here {
            self.counters.concealed_frames += 1;
        }
        self.present[slot] = false;
        self.read_pos += 1;

        if overfull && self.gain == 0.0 {
            // Faded out: skip ahead to the target depth, then fade back in.
            let before = self.read_pos;
            self.skip_to(self.write_end.saturating_sub(self.fill_frames()));
            self.counters.trims += 1;
            self.counters.trimmed_frames += self.read_pos - before;
            self.step = Step::None;
        } else if stepping && self.gain == 0.0 {
            match self.step {
                Step::Skip(n) => {
                    // Keep at least half the target so a skip can never
                    // cause the underrun it is meant to prevent.
                    let keep = self.fill_frames() / 2;
                    let to = (self.read_pos + n).min(self.write_end.saturating_sub(keep));
                    let before = self.read_pos;
                    self.skip_to(to);
                    self.counters.skips += 1;
                    self.counters.skipped_frames += self.read_pos - before;
                }
                Step::Insert(n) => {
                    self.inserting = n;
                    self.counters.inserts += 1;
                    self.counters.inserted_frames += n;
                }
                Step::None => {}
            }
            self.step = Step::None;
        }
    }

    /// Moves the read position forward, clearing presence marks so the ring
    /// slots read as empty when they come round again.
    fn skip_to(&mut self, pos: u64) {
        while self.read_pos < pos {
            self.present[(self.read_pos & MASK) as usize] = false;
            self.read_pos += 1;
        }
    }

    fn emit_tail_only(&mut self, out: &mut [f32; MAX_CHANNELS], ch: usize) {
        out[..ch].copy_from_slice(&self.tail[..ch]);
        self.last_out[..ch].copy_from_slice(&self.tail[..ch]);
        for t in &mut self.tail[..ch] {
            *t *= self.tail_decay;
        }
    }
}

//! What the output device actually does, measured from its render callbacks.
//!
//! Real drivers do not call back once per period on a steady clock: virtual
//! endpoints hand out several short buffers back to back from a coarse timer,
//! stall and then catch up (or not), and run at a rate that can be a few
//! thousand ppm off nominal. The engine needs three things from that pattern,
//! all computed here on the callback thread without allocation:
//!
//! - a smooth *playout line*: output frames the device has really played by
//!   time t, fitted through the middle of the callback staircase. Measuring
//!   jitter-buffer depth against this line (rather than against "the last
//!   callback, then nominal speed") gives the time-averaged depth whatever
//!   the cadence, so the drift loop controls the same quantity the buffer
//!   really holds;
//! - the *lead*: how far ahead of that line the device has pulled frames
//!   (half a period for a steady device, half a burst for a bursty one). The
//!   jitter-buffer floor must cover twice its recent peak, or a burst
//!   empties the buffer;
//! - the device's long-run rate and callback cadence, for the stats log.

/// Natural frequency of the playout line (rad/s). Fast enough to absorb a
/// stall within ~0.3 s, slow enough to average a 16 ms burst timer and
/// several ms of callback jitter.
const LINE_OMEGA: f64 = 10.0;
/// The line's rate may not leave nominal by more than this; a stall would
/// otherwise kick it far off while it re-locks.
const LINE_MAX_RATE_DEV: f64 = 0.02;
/// Lead peak decays like the network jitter peak (jitter.rs).
const LEAD_DECAY_S: f64 = 8.0;
/// Forgetting time of the long-run rate fit (s).
const RATE_TAU_S: f64 = 4.0;
/// Rate is reported only once this much callback time has been seen.
const RATE_MIN_S: f64 = 2.0;
/// Callbacks closer together than this are one burst: the line is updated
/// once per burst, from its mean point, so a burst's lead is measured
/// against where the line stood before it.
const BURST_GAP_S: f64 = 0.000_25;

#[derive(Debug, Clone)]
pub struct DeviceClock {
    nominal: f64,
    /// Nominal callback size, for the lead floor.
    period_frames: f64,
    // Playout line: o(t) = o0 + slope·(t − t0).
    init: bool,
    t0: f64,
    o0: f64,
    slope: f64,
    lead_peak_s: f64,
    last_t: f64,
    // Exponentially forgetting least-squares fit of the same points, for
    // the long-run rate. Centred on (rt, ro) to keep the sums small.
    rt: f64,
    ro: f64,
    sw: f64,
    st: f64,
    so: f64,
    stt: f64,
    sto: f64,
    seen_s: f64,
    // Cadence since the last `take_cadence`.
    cad: Cadence,
    // The burst being collected: time of its last callback, and the
    // weighted sum of its points and weights.
    g_t: f64,
    g_tw: f64,
    g_ow: f64,
    g_w: f64,
}

/// Callback cadence over a reporting window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cadence {
    pub callbacks: u64,
    pub frames: u64,
    pub interval_sum_s: f64,
    pub interval_max_s: f64,
    /// Wall time covered (first to last callback).
    pub span_s: f64,
}

impl DeviceClock {
    pub fn new(nominal_rate: f64, period_frames: usize) -> Self {
        Self {
            nominal: nominal_rate,
            period_frames: period_frames.max(1) as f64,
            init: false,
            t0: 0.0,
            o0: 0.0,
            slope: nominal_rate,
            lead_peak_s: 0.0,
            last_t: 0.0,
            rt: 0.0,
            ro: 0.0,
            sw: 0.0,
            st: 0.0,
            so: 0.0,
            stt: 0.0,
            sto: 0.0,
            seen_s: 0.0,
            cad: Cadence::default(),
            g_t: 0.0,
            g_tw: 0.0,
            g_ow: 0.0,
            g_w: 0.0,
        }
    }

    /// One render callback at `t` that consumed `frames` output frames,
    /// `before` being the total rendered before it.
    pub fn observe(&mut self, t: f64, frames: usize, before: u64) {
        let f = frames as f64;
        // The callback's frames play over the next f/rate seconds, so the
        // staircase's middle at t is half of them.
        let obs = before as f64 + f / 2.0;
        let w = f / self.nominal;
        if !self.init {
            self.init = true;
            self.t0 = t;
            self.o0 = obs;
            self.slope = self.nominal;
            self.rt = t;
            self.ro = obs;
            self.last_t = t;
            self.g_t = t;
            self.lead_peak_s = 0.0;
        } else {
            let dt = (t - self.last_t).max(0.0);
            self.cad.callbacks += 1;
            self.cad.interval_sum_s += dt;
            self.cad.interval_max_s = self.cad.interval_max_s.max(dt);
            self.cad.span_s += dt;
            self.last_t = t;
            if t - self.g_t > BURST_GAP_S {
                self.flush_burst();
            }
            self.lead_peak_s *= (-dt / LEAD_DECAY_S).exp();
            self.fit(t, obs, w, dt);
        }
        self.g_t = t;
        self.g_tw += w * t;
        self.g_ow += w * obs;
        self.g_w += w;
        self.cad.frames += frames as u64;
        let lead = (before as f64 + f - self.line_at(t)) / self.nominal;
        self.lead_peak_s = self.lead_peak_s.max(lead);
    }

    /// Moves the playout line by one burst (its weighted mean point).
    fn flush_burst(&mut self) {
        if self.g_w <= 0.0 {
            return;
        }
        let (t, obs, w) = (self.g_tw / self.g_w, self.g_ow / self.g_w, self.g_w);
        (self.g_tw, self.g_ow, self.g_w) = (0.0, 0.0, 0.0);
        // Critically damped second-order loop, gains weighted by the audio
        // time the burst represents so the bandwidth does not depend on the
        // callback size or how callbacks are grouped.
        let pred = self.o0 + self.slope * (t - self.t0);
        let e = obs - pred;
        let a = (2.0 * LINE_OMEGA * w).min(1.0);
        self.o0 = pred + a * e;
        self.t0 = t;
        self.slope = (self.slope + LINE_OMEGA * LINE_OMEGA * w * e).clamp(
            self.nominal * (1.0 - LINE_MAX_RATE_DEV),
            self.nominal * (1.0 + LINE_MAX_RATE_DEV),
        );
    }

    fn fit(&mut self, t: f64, o: f64, w: f64, dt: f64) {
        let k = (-dt / RATE_TAU_S).exp();
        self.sw *= k;
        self.st *= k;
        self.so *= k;
        self.stt *= k;
        self.sto *= k;
        // Re-centre now and then so the sums stay well conditioned.
        if t - self.rt > RATE_TAU_S {
            let (dt0, do0) = (t - self.rt, o - self.ro);
            let (st, so) = (self.st, self.so);
            self.stt += -2.0 * dt0 * st + dt0 * dt0 * self.sw;
            self.sto += -dt0 * so - do0 * st + dt0 * do0 * self.sw;
            self.st -= dt0 * self.sw;
            self.so -= do0 * self.sw;
            self.rt = t;
            self.ro = o;
        }
        let (x, y) = (t - self.rt, o - self.ro);
        self.sw += w;
        self.st += w * x;
        self.so += w * y;
        self.stt += w * x * x;
        self.sto += w * x * y;
        self.seen_s += w;
    }

    /// Output frames played by time `t` (fractional) on the fitted line.
    pub fn line_at(&self, t: f64) -> f64 {
        self.o0 + self.slope * (t - self.t0)
    }

    pub fn is_locked(&self) -> bool {
        self.init
    }

    /// Recent peak of how far the device pulls ahead of its playout line,
    /// never less than half a nominal callback (s).
    pub fn lead_s(&self) -> f64 {
        self.lead_peak_s
            .max(self.period_frames / 2.0 / self.nominal)
    }

    /// Long-run consumption rate vs nominal (ppm; + = consumes faster), once
    /// a few seconds have been seen.
    pub fn rate_ppm(&self) -> Option<f64> {
        if self.seen_s < RATE_MIN_S {
            return None;
        }
        let den = self.sw * self.stt - self.st * self.st;
        if den <= 0.0 {
            return None;
        }
        let slope = (self.sw * self.sto - self.st * self.so) / den;
        Some((slope / self.nominal - 1.0) * 1e6)
    }

    /// Cadence since the last call, and resets it.
    pub fn take_cadence(&mut self) -> Cadence {
        std::mem::take(&mut self.cad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(d: &mut DeviceClock, times: impl Iterator<Item = (f64, usize)>) -> u64 {
        let mut total = 0u64;
        for (t, f) in times {
            d.observe(t, f, total);
            total += f as u64;
        }
        total
    }

    #[test]
    fn steady_device_leads_by_half_a_period_and_reports_its_rate() {
        let mut d = DeviceClock::new(48_000.0, 144);
        let rate = 48_000.0 * (1.0 - 3000e-6);
        feed(&mut d, (0..20_000).map(|k| (k as f64 * 144.0 / rate, 144)));
        assert!((d.lead_s() - 0.0015).abs() < 0.0001, "{}", d.lead_s());
        let ppm = d.rate_ppm().unwrap();
        assert!((ppm + 3000.0).abs() < 5.0, "{ppm}");
    }

    #[test]
    fn bursts_raise_the_lead_to_half_a_burst() {
        let mut d = DeviceClock::new(48_000.0, 144);
        // A 12 ms timer hands out four 3 ms buffers back to back.
        let times =
            (0..5000).flat_map(|n| (0..4).map(move |i| (n as f64 * 0.012 + i as f64 * 2e-5, 144)));
        feed(&mut d, times);
        let lead_ms = d.lead_s() * 1e3;
        assert!((5.5..7.5).contains(&lead_ms), "lead {lead_ms} ms");
        assert!(d.rate_ppm().unwrap().abs() < 50.0);
    }
}

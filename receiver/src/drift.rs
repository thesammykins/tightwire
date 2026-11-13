//! PI controller that keeps the jitter buffer at its target depth by nudging
//! the resampling ratio (SPEC §5, drift compensation).
//!
//! Loop model: depth error e (seconds) changes at δ − u, where δ is the
//! clock offset between the sender and the output device and u the
//! correction. With u = Kp·e + Ki·∫e the loop is s² + Kp·s + Ki; Kp = 2ωn,
//! Ki = ωn² gives critical damping. ωn = 0.5 rad/s settles a 200 ppm crystal
//! offset within ~10 s with a peak depth error of about δ/(e·ωn) ≈ 0.15 ms.
//! Slower lets the start-up offset eat the safety margin for seconds (seen as
//! underruns in loopback); faster turns jitter-target changes into pitch
//! wobble (tests bound the wander at < 120 ppm ≈ 0.2 cent with ±0.3 ms
//! simulated jitter).
//!
//! Rates beyond ±1000 ppm (0.2.1). A virtual device can run thousands of
//! ppm off nominal; following it is no pitch change to the listener, only
//! the correct rate. So the correction is `tracking + loop`, where `loop`
//! is the 0.2.0 PI controller, unchanged and still clamped to ±1000 ppm
//! (1.7 cents: the pitch actually moved to steer the depth), and `tracking`
//! (±1 %) is the part of the rate difference the loop has handed over. It
//! learns two ways: when the loop's integral has sat near its limit for
//! seconds *and the error has not shrunk meanwhile*, that excess moves into
//! `tracking`; and when two of the engine's discrete depth steps
//! (`engine.rs`) or underruns in a row go the same way, 0.5–5 s apart, the
//! rate they imply is added at once. A start-up or refill transient shrinks
//! while the loop works on it, so it is never learned as a rate.

const OMEGA_N: f64 = 0.5;
const KP: f64 = 2.0 * OMEGA_N;
const KI: f64 = OMEGA_N * OMEGA_N;
const SMOOTH_TAU_S: f64 = 0.25;
/// The adaptive target jumps when a new jitter peak is seen; feeding that step
/// straight into Kp would kick the ratio by hundreds of ppm. Gliding toward
/// it keeps the correction (and so the pitch) steady.
const TARGET_TAU_S: f64 = 2.0;
/// Limit of the PI loop (and of its integral).
pub const MAX_CORRECTION: f64 = 1000e-6;
/// Limit of the tracked rate difference.
pub const MAX_TRACKING: f64 = 10_000e-6;
/// Two same-direction steps further apart than this are unrelated events.
const LEARN_WINDOW_S: f64 = 5.0;
/// Steps closer together than this are one disturbance (or a device whose
/// bursts the capped buffer cannot cover), not a rate to learn; and one
/// lesson moves the tracked rate by at most this much.
const LEARN_MIN_GAP_S: f64 = 0.5;
const MAX_LEARN_STEP: f64 = 3000e-6;
/// The loop's integral beyond this fraction of its limit for this long, with
/// the error no smaller than `HANDOVER_SHRINK` × what it was, is a rate the
/// loop cannot finish tracking alone; the excess over `HANDOVER_KEEP` moves
/// into `tracking` with this time constant.
const HANDOVER_AT: f64 = 0.8;
const HANDOVER_AFTER_S: f64 = 2.0;
const HANDOVER_SHRINK: f64 = 1.0;
const HANDOVER_KEEP: f64 = 0.5;
const HANDOVER_TAU_S: f64 = 1.0;
/// The target may fall at most this fast (s/s): a decaying jitter peak can
/// drop it faster than ±1000 ppm can drain, which would pin the loop.
const TARGET_FALL_MAX: f64 = 0.0006;
/// The loop output pinned at its limit this long counts as saturated.
const SATURATED_AFTER_S: f64 = 2.0;

#[derive(Debug, Clone)]
pub struct DriftController {
    integral: f64,
    smoothed_s: Option<f64>,
    target_s: Option<f64>,
    /// The PI loop's output (±MAX_CORRECTION).
    pi: f64,
    tracking: f64,
    /// How long the integral has been near its limit (pushing the way the
    /// error points), the error when that spell started, and whether a
    /// handover is running.
    high_s: f64,
    err_mark: f64,
    handing: bool,
    /// How long the PI output has been at its limit.
    pinned_s: f64,
    /// Time and sign of the last discrete step (fade-skip / fade-insert).
    last_step: Option<(f64, f64)>,
}

impl DriftController {
    pub fn new() -> Self {
        Self {
            integral: 0.0,
            smoothed_s: None,
            target_s: None,
            pi: 0.0,
            tracking: 0.0,
            high_s: 0.0,
            err_mark: 0.0,
            handing: false,
            pinned_s: 0.0,
            last_step: None,
        }
    }

    /// Forget the depth history (stream restart). The integrator is kept: it
    /// encodes the clock offset between the two machines, which survives
    /// a sender restart.
    pub fn reset_depth(&mut self) {
        self.smoothed_s = None;
        self.target_s = None;
    }

    /// Feeds one depth observation taken `dt_s` after the previous one and
    /// returns the ratio correction (input frames consumed per output frame
    /// is multiplied by `1 + correction`).
    pub fn update(&mut self, depth_s: f64, target_s: f64, dt_s: f64) -> f64 {
        let smoothed = match self.smoothed_s {
            None => depth_s,
            Some(prev) => prev + (depth_s - prev) * (dt_s / SMOOTH_TAU_S).min(1.0),
        };
        self.smoothed_s = Some(smoothed);
        let target = match self.target_s {
            None => target_s,
            // Glide upward steps; let it fall at a rate the loop can follow.
            Some(prev) if target_s > prev => {
                prev + (target_s - prev) * (dt_s / TARGET_TAU_S).min(1.0)
            }
            Some(prev) => target_s.max(prev - TARGET_FALL_MAX * dt_s),
        };
        self.target_s = Some(target);
        let err = smoothed - target;
        // Anti-windup: the integral alone may never demand more than the clamp.
        self.integral = (self.integral + KI * err * dt_s).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        if self.integral.abs() >= HANDOVER_AT * MAX_CORRECTION
            && self.integral.signum() == err.signum()
        {
            self.high_s += dt_s;
            if self.high_s >= HANDOVER_AFTER_S {
                // Most of the loop's authority for seconds, and the error
                // did not shrink: a rate, not a transient.
                if err.abs() >= HANDOVER_SHRINK * self.err_mark.abs() {
                    self.handing = true;
                }
                self.high_s = 0.0;
                self.err_mark = err;
            }
        } else {
            self.high_s = 0.0;
            self.err_mark = err;
        }
        if self.handing {
            let keep = HANDOVER_KEEP * MAX_CORRECTION * self.integral.signum();
            let moved = (self.integral - keep) * (dt_s / HANDOVER_TAU_S).min(1.0);
            self.integral -= moved;
            self.tracking = (self.tracking + moved).clamp(-MAX_TRACKING, MAX_TRACKING);
            if self.integral.abs() <= HANDOVER_KEEP * MAX_CORRECTION * 1.01 {
                self.handing = false;
            }
        }
        self.pi = (KP * err + self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        if self.pi.abs() >= MAX_CORRECTION * 0.999 {
            self.pinned_s += dt_s;
        } else {
            self.pinned_s = 0.0;
        }
        self.correction()
    }

    /// The engine removed (`removed_s` > 0) or added (< 0) this much depth in
    /// one step at `now_s`. Shifts the depth history accordingly and, if the
    /// previous step went the same way recently, learns the rate both imply.
    pub fn stepped(&mut self, removed_s: f64, now_s: f64) {
        if let Some(s) = self.smoothed_s.as_mut() {
            *s -= removed_s;
        }
        let dir = removed_s.signum();
        if let Some((t, d)) = self.last_step
            && d == dir
            && now_s - t < LEARN_WINDOW_S
            && now_s - t >= LEARN_MIN_GAP_S
        {
            // Depth grew (or shrank) by `removed_s` in `now − t` seconds
            // despite the correction: consume that much faster (slower).
            let rate = (removed_s / (now_s - t)).clamp(-MAX_LEARN_STEP, MAX_LEARN_STEP);
            self.tracking = (self.tracking + rate).clamp(-MAX_TRACKING, MAX_TRACKING);
        }
        self.last_step = Some((now_s, dir));
    }

    pub fn correction(&self) -> f64 {
        self.tracking + self.pi
    }

    /// Rate difference handed over from the loop (beyond ±1000 ppm).
    pub fn tracking(&self) -> f64 {
        self.tracking
    }

    /// True while the loop has been pinned at its limit for seconds, or the
    /// tracked rate is at its own: the depth is not being held.
    pub fn saturated(&self) -> bool {
        self.pinned_s >= SATURATED_AFTER_S || self.tracking.abs() >= MAX_TRACKING * 0.999
    }

    /// The target the loop is steering to (upward changes glide).
    pub fn target_s(&self) -> Option<f64> {
        self.target_s
    }

    pub fn smoothed_depth_s(&self) -> Option<f64> {
        self.smoothed_s
    }
}

impl Default for DriftController {
    fn default() -> Self {
        Self::new()
    }
}

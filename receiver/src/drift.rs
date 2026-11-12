//! PI controller that keeps the jitter buffer at its target depth by nudging
//! the resampling ratio (SPEC §5, drift compensation).
//!
//! Loop model: depth error e (seconds) changes at δ − u, where δ is the
//! sender/receiver clock offset and u the correction. With u = Kp·e + Ki·∫e
//! the loop is s² + Kp·s + Ki; Kp = 2ωn, Ki = ωn² gives critical damping.
//! ωn = 0.5 rad/s settles a 200 ppm crystal offset within ~10 s with a peak
//! depth error of about δ/(e·ωn) ≈ 0.15 ms. Slower lets the start-up offset
//! eat the safety margin for seconds (seen as underruns in loopback); faster
//! turns jitter-target changes into pitch wobble (tests bound the wander at
//! < 120 ppm ≈ 0.2 cent with ±0.3 ms simulated jitter).

const OMEGA_N: f64 = 0.5;
const KP: f64 = 2.0 * OMEGA_N;
const KI: f64 = OMEGA_N * OMEGA_N;
const SMOOTH_TAU_S: f64 = 0.25;
/// The adaptive target jumps when a new jitter peak is seen; feeding that step
/// straight into Kp would kick the ratio by hundreds of ppm. Gliding toward
/// it keeps the correction (and so the pitch) steady.
const TARGET_TAU_S: f64 = 2.0;
pub const MAX_CORRECTION: f64 = 1000e-6;

#[derive(Debug, Clone)]
pub struct DriftController {
    integral: f64,
    smoothed_s: Option<f64>,
    target_s: Option<f64>,
    correction: f64,
}

impl DriftController {
    pub fn new() -> Self {
        Self {
            integral: 0.0,
            smoothed_s: None,
            target_s: None,
            correction: 0.0,
        }
    }

    /// Forget the depth history (stream restart). The integrator is kept: it
    /// encodes the crystal offset between the two machines, which survives
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
            // Glide only upward steps; the target falls by slow decay anyway,
            // and gliding that too would just make depth lag behind it.
            Some(prev) if target_s > prev => {
                prev + (target_s - prev) * (dt_s / TARGET_TAU_S).min(1.0)
            }
            Some(_) => target_s,
        };
        self.target_s = Some(target);
        let err = smoothed - target;
        // Anti-windup: the integral alone may never demand more than the clamp.
        self.integral = (self.integral + KI * err * dt_s).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        self.correction = (KP * err + self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        self.correction
    }

    pub fn correction(&self) -> f64 {
        self.correction
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

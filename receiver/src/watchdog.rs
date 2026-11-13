//! Exclusive-mode health watchdog (0.2.1).
//!
//! Exclusive mode hands the stream straight to the driver, so a virtual
//! device that accepts it but then calls back in bursts or at an
//! off-nominal rate has nothing in between to smooth it out (field report:
//! underruns climbing and the drift loop pinned while shared mode on the
//! same endpoint was healthy). The tray feeds this once a second while an
//! exclusive stream runs; when it trips, the tray reopens the endpoint in
//! shared mode, turns the endpoint's exclusive preference off (so it is not
//! retried until the user turns it on again), notifies once and logs why.
//! Platform-independent so it is unit-tested on any OS.

use std::collections::VecDeque;

/// Nothing is judged in the first seconds after opening: priming and the
/// device clock locking on are expected to cost an underrun or two.
pub const GRACE_S: f64 = 5.0;
/// The drift loop saturated this long without a break trips the watchdog.
pub const SATURATED_FOR_S: f64 = 10.0;
/// This many underruns within `WINDOW_S` trips it.
pub const UNDERRUNS_IN_WINDOW: u64 = 4;
pub const WINDOW_S: f64 = 10.0;

#[derive(Debug, Clone, Default)]
pub struct ExclusiveWatchdog {
    opened_at: Option<f64>,
    saturated_since: Option<f64>,
    /// (time, underrun counter) samples within the window.
    underruns: VecDeque<(f64, u64)>,
    tripped: bool,
}

impl ExclusiveWatchdog {
    pub fn new() -> Self {
        Self::default()
    }

    /// One observation (about once a second). Returns the reason, once,
    /// when exclusive mode should be abandoned.
    pub fn observe(&mut self, now_s: f64, saturated: bool, underruns: u64) -> Option<String> {
        if self.tripped {
            return None;
        }
        let opened = *self.opened_at.get_or_insert(now_s);
        if now_s - opened < GRACE_S {
            self.underruns.clear();
            self.underruns.push_back((now_s, underruns));
            return None;
        }
        self.underruns.push_back((now_s, underruns));
        while self
            .underruns
            .front()
            .is_some_and(|&(t, _)| now_s - t > WINDOW_S)
        {
            self.underruns.pop_front();
        }
        let recent = self
            .underruns
            .front()
            .map_or(0, |&(_, u)| underruns.saturating_sub(u));
        self.saturated_since = if saturated {
            Some(self.saturated_since.unwrap_or(now_s))
        } else {
            None
        };
        let reason = if recent >= UNDERRUNS_IN_WINDOW {
            Some(format!("{recent} underruns in the last {WINDOW_S:.0} s"))
        } else {
            self.saturated_since
                .filter(|&since| now_s - since >= SATURATED_FOR_S)
                .map(|since| {
                    format!(
                        "the drift correction has been at its limit for {:.0} s (the device is not keeping time)",
                        now_s - since
                    )
                })
        };
        if reason.is_some() {
            self.tripped = true;
        }
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_stream_never_trips() {
        let mut w = ExclusiveWatchdog::new();
        for t in 0..600 {
            // One underrun at start-up, one isolated later.
            let u = if t < 300 { 1 } else { 2 };
            assert_eq!(w.observe(t as f64, false, u), None, "t={t}");
        }
    }

    #[test]
    fn climbing_underruns_trip_once() {
        let mut w = ExclusiveWatchdog::new();
        let mut tripped = vec![];
        for t in 0..60 {
            // One underrun every 2 s from the start (field report B).
            if let Some(r) = w.observe(t as f64, false, t / 2) {
                tripped.push((t, r));
            }
        }
        assert_eq!(tripped.len(), 1, "{tripped:?}");
        assert!(tripped[0].0 <= 15, "{tripped:?}");
        assert!(tripped[0].1.contains("underruns"));
    }

    #[test]
    fn long_saturation_trips_short_does_not() {
        let mut w = ExclusiveWatchdog::new();
        // 8 s saturated after a disturbance, then fine.
        for t in 0..40 {
            assert_eq!(w.observe(t as f64, (10..18).contains(&t), 0), None);
        }
        let r = (40..60).find_map(|t| w.observe(t as f64, true, 0));
        assert!(r.unwrap().contains("limit for 10 s"));
    }

    #[test]
    fn start_up_underruns_are_forgiven() {
        let mut w = ExclusiveWatchdog::new();
        for t in 0..30 {
            let u = if t < 4 { t } else { 4 };
            assert_eq!(w.observe(t as f64, false, u), None, "t={t}");
        }
    }
}

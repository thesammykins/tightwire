//! One monotonic clock shared by the socket thread (packet arrival) and the
//! output callback (render time), so the two can be compared directly.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Seconds since the first call. Lock-free after initialisation (call once at
/// start-up, before any audio thread exists).
pub fn now_s() -> f64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Puts the calling thread in the scheduling class CoreAudio uses for device
/// IO threads (Mach time-constraint policy). Ordinary threads on a busy Mac
/// are routinely woken milliseconds late, which shows up as fake network or
/// device jitter. No-op elsewhere (Windows uses MMCSS / thread priority).
#[cfg(target_os = "macos")]
// libc marks its Mach bindings deprecated in favour of the mach2 crate; these
// three calls are stable kernel ABI and not worth another dependency.
#[allow(deprecated)]
pub fn promote_to_realtime(period: Duration) {
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: plain Mach calls on the current thread with a correctly sized policy struct.
    unsafe {
        libc::mach_timebase_info(&mut tb);
        let to_abs = |d: Duration| (d.as_nanos() as u64 * tb.denom as u64 / tb.numer as u64) as u32;
        let mut policy = libc::thread_time_constraint_policy {
            period: to_abs(period),
            computation: to_abs(period / 4),
            constraint: to_abs(period),
            preemptible: 1,
        };
        libc::thread_policy_set(
            libc::mach_thread_self(),
            libc::THREAD_TIME_CONSTRAINT_POLICY as u32,
            &mut policy as *mut _ as libc::thread_policy_t,
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        );
    }
}

#[cfg(not(target_os = "macos"))]
pub fn promote_to_realtime(_period: Duration) {}

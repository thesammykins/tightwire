//! The engine against output devices that misbehave the way real (mostly
//! virtual) endpoints do: callbacks in bursts, stalls, rates thousands of
//! ppm off nominal, jittery callback times — plus the receiver's socket
//! thread holding arrivals. Each profile runs on a virtual clock
//! (`support/simdev.rs`) and must meet the same bar:
//!
//! - the depth the engine reports and controls agrees with the real
//!   time-averaged jitter-buffer depth within 2 ms;
//! - the drift loop is never saturated for more than 3 s at a time;
//! - after a disturbance the real depth is back within 2 ms of the target
//!   by the end of the second-but-next second (≈2 s);
//! - underruns stop once the engine has seen the device's pattern;
//! - lost and late stay 0 (those are network counters; nothing here may
//!   touch them).

mod support;

use support::simdev::*;

const SECS: f64 = 40.0;

fn exclusive_144() -> Device {
    Device::regular(144)
}

struct Expect {
    /// Seconds after start before steady-state checks apply.
    warmup_s: f64,
    /// Disturbance times: depth must recover by t + 3 (end of that second).
    disturbances: Vec<f64>,
    max_underruns: u64,
}

fn check(name: &str, sender: Sender, device: Device, x: Expect) -> Run {
    let r = run(&sender, &device, Limits::default(), SECS, |_, _| {});
    print(name, &r);
    let quiet = |t: f64| t > x.warmup_s && !x.disturbances.iter().any(|&d| t > d && t <= d + 3.0);
    for s in &r.seconds {
        if quiet(s.t) {
            assert!(
                (s.reported_ms - s.real_ms).abs() <= 2.0,
                "{name} t={}: reported {:.2} ms vs real {:.2} ms",
                s.t,
                s.reported_ms,
                s.real_ms
            );
            assert!(
                (s.real_ms - s.target_ms).abs() <= 2.0,
                "{name} t={}: real depth {:.2} ms vs target {:.2} ms",
                s.t,
                s.real_ms,
                s.target_ms
            );
        }
    }
    let longest = r.saturated_spells.iter().map(|s| s.1).fold(0.0, f64::max);
    assert!(
        longest <= 3.0,
        "{name}: drift loop saturated for {longest:.1} s ({:?})",
        r.saturated_spells
    );
    let last = r.seconds.last().unwrap();
    let at_warmup = r.seconds[x.warmup_s as usize - 1].underruns;
    assert!(
        last.underruns <= x.max_underruns,
        "{name}: {} underruns",
        last.underruns
    );
    assert!(
        last.underruns - at_warmup <= x.disturbances.len() as u64,
        "{name}: underruns kept coming after warm-up ({} → {})",
        at_warmup,
        last.underruns
    );
    assert_eq!((last.lost, last.late), (0, 0), "{name}: lost/late");
    r
}

fn steady() -> Expect {
    Expect {
        warmup_s: 6.0,
        disturbances: vec![],
        max_underruns: 2,
    }
}

/// Field report B, reproduced: a 3 ms exclusive-mode virtual endpoint whose
/// driver hands out its buffers from a 10 ms timer (three or four back to
/// back), running 2000 ppm slow, with callback jitter, fed 128-frame packets.
/// 0.2.0 sized the floor for one 3 ms period, so every burst emptied the
/// buffer (an underrun storm, the tray flapping between streaming and
/// searching) and its ±1000 ppm loop sat saturated; the reported depth was
/// off the real one by several ms.
#[test]
fn field_b_bursty_slow_exclusive_endpoint() {
    let mut d = exclusive_144();
    d.cadence = Cadence::Burst {
        tick_s: 0.010,
        gap_s: 20e-6,
    };
    d.rate_ppm = -2000.0;
    d.jitter_s = 0.0005;
    let r = check("B", Sender::lan(128), d, steady());
    let c = r.engine.jitter().counters;
    assert!(c.trims <= 2, "{c:?}");
}

/// Field report C, reproduced: one 40 ms hold on the receiving side drives
/// the jitter peak, and so the target, to the 20 ms cap. In 0.2.0 the trim
/// threshold then equalled the target, so every sawtooth peak was faded and
/// skipped — about twenty fades a second ("static") — the mean depth was
/// held below target and the loop sat at −1000 ppm until the peak decayed.
#[test]
fn field_c_target_at_cap_does_not_trim_every_peak() {
    let mut s = Sender::lan(128);
    s.holds = vec![(10.0, 0.040)];
    let r = check(
        "C",
        s,
        exclusive_144(),
        Expect {
            warmup_s: 6.0,
            disturbances: vec![10.0],
            max_underruns: 2,
        },
    );
    let c = r.engine.jitter().counters;
    assert!(c.trims <= 1, "trim storm: {c:?}");
}

#[test]
fn regular_device() {
    check("regular", Sender::lan(48), exclusive_144(), steady());
}

#[test]
fn bursts_of_a_16ms_timer() {
    let mut d = exclusive_144();
    d.cadence = Cadence::Burst {
        tick_s: 0.016,
        gap_s: 1e-4,
    };
    check("burst16", Sender::lan(128), d, steady());
}

/// A device consuming 0.5 % fast drains a 5 ms buffer in about a second
/// faster than ±1000 ppm can refill it, so learning its rate from a cold
/// start costs a few underruns (each one a lesson); none after that.
#[test]
fn device_5000ppm_fast() {
    let mut d = exclusive_144();
    d.rate_ppm = 5000.0;
    check(
        "fast5000",
        Sender::lan(128),
        d,
        Expect {
            max_underruns: 5,
            ..steady()
        },
    );
}

/// The stats line reports what the device consumed per wall-second.
#[test]
fn stats_line_reports_the_device_rate() {
    use tightwire_recv::output::OutputFormat;
    use tightwire_recv::protocol::OutputMode;
    let mut d = exclusive_144();
    d.rate_ppm = 1500.0;
    let r = run(&Sender::lan(128), &d, Limits::default(), 20.0, |_, _| {});
    let line = tightwire_recv::net::summary_line(
        &r.stats,
        &OutputFormat {
            rate: RATE,
            channels: 1,
            period_frames: 144,
            mode: OutputMode::WasapiExclusive,
        },
    );
    eprintln!("{line}");
    let ppm: f64 = line
        .split(" ppm frames/s vs nominal")
        .next()
        .and_then(|s| s.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .expect(&line);
    assert!((ppm - 1500.0).abs() < 50.0, "{line}");
    assert!(line.contains("callbacks/s interval 3.00/"), "{line}");
}

#[test]
fn device_5000ppm_slow() {
    let mut d = exclusive_144();
    d.rate_ppm = -5000.0;
    check("slow5000", Sender::lan(128), d, steady());
}

#[test]
fn shared_mode_period_with_jittery_callbacks() {
    let mut d = Device::regular(480);
    d.jitter_s = 0.002;
    check("jitter", Sender::lan(48), d, steady());
}

#[test]
fn stall_then_skip_returns_to_target() {
    let mut d = exclusive_144();
    d.stalls = vec![Stall {
        at_s: 15.0,
        len_s: 0.030,
        catch_up: false,
    }];
    check(
        "stall-skip",
        Sender::lan(128),
        d,
        Expect {
            warmup_s: 6.0,
            disturbances: vec![15.0],
            max_underruns: 2,
        },
    );
}

#[test]
fn stall_then_catch_up_returns_to_target() {
    let mut d = exclusive_144();
    d.stalls = vec![Stall {
        at_s: 15.0,
        len_s: 0.030,
        catch_up: true,
    }];
    check(
        "stall-catch-up",
        Sender::lan(128),
        d,
        Expect {
            warmup_s: 6.0,
            disturbances: vec![15.0],
            max_underruns: 3,
        },
    );
}

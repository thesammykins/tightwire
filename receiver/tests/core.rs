//! SPEC §7.2: jitter buffer, drift controller and resampler under simulated
//! network and clock conditions. Everything runs on a virtual clock, so the
//! tests are deterministic and need no audio device.

use std::sync::Arc;

use tightwire_recv::engine::{self, Engine, EngineConfig, SharedStats, Slot};
use tightwire_recv::protocol::{AudioHeader, FLAG_DISCONTINUITY, SampleFormat};
use tightwire_recv::resample::{KernelTable, MAX_CHANNELS, Resampler};

const RATE: u32 = 48_000;
const PACKET: usize = 48;
const PERIOD: usize = 48;

fn header(seq: u32, ts: u64) -> AudioHeader {
    AudioHeader {
        stream_id: 7,
        sequence: seq,
        timestamp: ts,
        sample_rate: RATE,
        channels: 1,
        format: SampleFormat::F32,
        flags: if seq == 0 { FLAG_DISCONTINUITY } else { 0 },
    }
}

fn sine(freq: f64, rate: f64, n: u64) -> f32 {
    (0.5 * (2.0 * std::f64::consts::PI * freq * n as f64 / rate).sin()) as f32
}

fn slot(seq: u32, arrival_s: f64, freq: f64) -> Slot {
    let ts = seq as u64 * PACKET as u64;
    let mut s = Slot::empty();
    s.header = header(seq, ts);
    s.arrival_s = arrival_s;
    s.len = PACKET;
    for i in 0..PACKET {
        s.samples[i] = sine(freq, RATE as f64, ts + i as u64);
    }
    s
}

fn engine(min_target_ms: f64) -> (rtrb::Producer<Slot>, Engine, Arc<SharedStats>) {
    let stats = Arc::new(SharedStats::default());
    let cfg = EngineConfig {
        out_rate: RATE,
        out_channels: 1,
        period_frames: PERIOD,
        min_target_s: min_target_ms / 1000.0,
        max_target_s: 0.020,
    };
    let (tx, e) = engine::channel(cfg, stats.clone());
    (tx, e, stats)
}

/// Drives an engine on a virtual clock. `deliveries` gives (arrival time,
/// sequence) pairs; the output callback runs every PERIOD frames.
fn run(
    deliveries: &[(f64, u32)],
    seconds: f64,
    freq: f64,
    min_target_ms: f64,
) -> (Vec<f32>, Engine) {
    let (mut tx, mut e, _) = engine(min_target_ms);
    let mut order: Vec<_> = deliveries.to_vec();
    order.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut next = 0;
    let mut out = Vec::new();
    let mut buf = vec![0f32; PERIOD];
    let ticks = (seconds * RATE as f64 / PERIOD as f64) as usize;
    for t in 0..ticks {
        let now = t as f64 * PERIOD as f64 / RATE as f64;
        while next < order.len() && order[next].0 <= now {
            tx.push(slot(order[next].1, order[next].0, freq))
                .ok()
                .unwrap();
            next += 1;
        }
        e.render(&mut buf, now);
        out.extend_from_slice(&buf);
    }
    (out, e)
}

fn steady(n: u32, latency_s: f64) -> Vec<(f64, u32)> {
    (0..n)
        .map(|s| {
            (
                (s as f64 + 1.0) * PACKET as f64 / RATE as f64 + latency_s,
                s,
            )
        })
        .collect()
}

/// Largest sample-to-sample step: a click shows up as a step far above what a
/// 0.5-amplitude sine at `freq` can produce.
fn max_step(x: &[f32]) -> f32 {
    x.windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max)
}

fn sine_step_limit(freq: f64) -> f32 {
    (0.5 * 2.0 * std::f64::consts::PI * freq / RATE as f64) as f32 * 1.05
}

#[test]
fn reordering_is_transparent() {
    let freq = 1000.0;
    let mut d = steady(1000, 0.0003);
    // Every 10th packet is held back until just after its successor.
    for i in (10..990).step_by(10) {
        d[i].0 = d[i + 1].0 + 0.0001;
    }
    let (out, e) = run(&d, 1.0, freq, 0.0);
    let c = e.jitter().counters;
    assert_eq!(c.lost, 0, "{c:?}");
    assert_eq!(c.late, 0, "{c:?}");
    assert_eq!(c.underruns, 0, "{c:?}");
    assert_eq!(c.concealed_frames, 0, "{c:?}");
    // Continuous sine after the start-up fade: no step beyond the sine's own
    // slope, and never a run of silence.
    let start = (0.05 * RATE as f64) as usize;
    let tail = &out[start..(0.95 * RATE as f64) as usize];
    let step = max_step(tail);
    assert!(step <= sine_step_limit(freq), "discontinuity: step {step}");
    assert!(
        !tail.windows(24).any(|w| w.iter().all(|v| v.abs() < 1e-3)),
        "silent gap"
    );
}

#[test]
fn duplicates_are_dropped() {
    let base = steady(1000, 0.0003);
    let mut dup = base.clone();
    dup.extend(base.iter().map(|&(t, s)| (t + 0.0002, s)));
    let (a, _) = run(&base, 1.0, 1000.0, 3.0);
    let (b, e) = run(&dup, 1.0, 1000.0, 3.0);
    let c = e.jitter().counters;
    // All duplicates delivered before the run ended (the last ~1 ms is still queued).
    assert!(c.duplicates >= 995, "{c:?}");
    assert_eq!(c.lost, 0);
    assert_eq!(a, b);
}

#[test]
fn loss_fades_without_clicks() {
    let freq = 1000.0;
    let mut d = steady(1000, 0.0003);
    d.retain(|&(_, s)| s != 500);
    let (out, e) = run(&d, 1.0, freq, 3.0);
    let c = e.jitter().counters;
    assert_eq!(c.lost, 1, "{c:?}");
    assert_eq!(c.underruns, 0, "{c:?}");
    assert!(c.concealed_frames >= PACKET as u64, "{c:?}");
    // Skip the initial fade-in; then no step may exceed what the sine itself makes.
    let start = (0.05 * RATE as f64) as usize;
    let end = (0.95 * RATE as f64) as usize;
    let step = max_step(&out[start..end]);
    // Fades are 32 frames, so they add at most amplitude/32 per sample.
    let limit = sine_step_limit(freq) + 0.5 / 32.0;
    assert!(step <= limit, "click: max step {step} > {limit}");
    // And the hole really went silent somewhere.
    let quiet = out[start..end]
        .windows(16)
        .any(|w| w.iter().all(|v| v.abs() < 1e-4));
    assert!(quiet, "gap was not muted");
}

#[test]
fn burst_jitter_adapts_target_and_stops_underruns() {
    // Every 6th ms the network stalls, then delivers the backlog at once.
    let d: Vec<_> = (0..10_000u32)
        .map(|s| {
            let burst_end = ((s / 6) as f64 + 1.0) * 6.0 * PACKET as f64 / RATE as f64;
            (burst_end + 0.0002, s)
        })
        .collect();
    let (mut tx, mut e, _) = engine(0.0);
    let mut buf = vec![0f32; PERIOD];
    let mut next = 0;
    let mut underruns_at_5s = 0;
    for t in 0..(10.0 * RATE as f64 / PERIOD as f64) as usize {
        let now = t as f64 * PERIOD as f64 / RATE as f64;
        while next < d.len() && d[next].0 <= now {
            tx.push(slot(d[next].1, d[next].0, 440.0)).ok().unwrap();
            next += 1;
        }
        e.render(&mut buf, now);
        if t == (5.0 * RATE as f64 / PERIOD as f64) as usize {
            underruns_at_5s = e.jitter().counters.underruns;
        }
    }
    let c = e.jitter().counters;
    let target_ms = e.jitter().target_s() * 1000.0;
    assert!(
        target_ms >= 6.0,
        "target {target_ms} ms did not grow to cover 6 ms bursts"
    );
    assert!(target_ms <= 12.0, "target {target_ms} ms overshot");
    assert_eq!(
        c.underruns, underruns_at_5s,
        "underruns continued after adapting: {c:?}"
    );
    assert_eq!(c.lost, 0);
}

/// Sender clock runs `ppm` fast relative to the receiver; arrivals carry a
/// little random jitter. The controller must absorb it without underrun.
fn drift_case(ppm: f64) {
    let (mut tx, mut e, stats) = engine(0.0);
    let sender_rate = RATE as f64 * (1.0 + ppm * 1e-6);
    let mut rng = 0x1234_5678u64;
    let mut seq = 0u32;
    let mut buf = vec![0f32; PERIOD];
    let seconds = 60.0;
    let ticks = (seconds * RATE as f64 / PERIOD as f64) as usize;
    let mut late_ratio = Vec::new();
    for t in 0..ticks {
        let now = t as f64 * PERIOD as f64 / RATE as f64;
        loop {
            let send = (seq as f64 + 1.0) * PACKET as f64 / sender_rate;
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let jitter = (rng % 300) as f64 * 1e-6; // up to 0.3 ms
            if send + jitter > now {
                break;
            }
            tx.push(slot(seq, send + jitter, 440.0)).ok().unwrap();
            seq += 1;
        }
        e.render(&mut buf, now);
        if now > 40.0 {
            late_ratio.push(e.drift().correction());
        }
    }
    let c = e.jitter().counters;
    assert_eq!(c.underruns, 0, "{ppm} ppm: {c:?}");
    assert_eq!(c.late, 0, "{ppm} ppm: {c:?}");
    let mean = late_ratio.iter().sum::<f64>() / late_ratio.len() as f64 * 1e6;
    assert!(
        (mean - ppm).abs() < 10.0,
        "{ppm} ppm: correction converged to {mean:.1} ppm"
    );
    // Pitch wobble: how far the ratio wanders once converged.
    let spread = late_ratio.iter().fold(f64::MIN, |a, &b| a.max(b))
        - late_ratio.iter().fold(f64::MAX, |a, &b| a.min(b));
    assert!(
        spread * 1e6 < 120.0,
        "{ppm} ppm: correction wanders {:.1} ppm",
        spread * 1e6
    );
    let depth_ms = e.drift().smoothed_depth_s().unwrap() * 1000.0;
    let target_ms = e.jitter().target_s() * 1000.0;
    assert!(
        (depth_ms - target_ms).abs() < 0.3,
        "{ppm} ppm: depth {depth_ms:.2} vs target {target_ms:.2}"
    );
    eprintln!(
        "drift {ppm:+} ppm: correction {mean:+.1} ppm (spread {:.1}), depth {depth_ms:.2} ms, target {target_ms:.2} ms, reported {} us",
        spread * 1e6,
        stats.depth_us.load(std::sync::atomic::Ordering::Relaxed),
    );
}

#[test]
fn drift_plus_200ppm_converges() {
    drift_case(200.0);
}

#[test]
fn drift_minus_200ppm_converges() {
    drift_case(-200.0);
}

#[test]
fn stream_change_resyncs() {
    let (mut tx, mut e, _) = engine(2.0);
    let mut buf = vec![0f32; PERIOD];
    for s in 0..100u32 {
        let now = s as f64 * 0.001;
        tx.push(slot(s, now, 440.0)).ok().unwrap();
        e.render(&mut buf, now);
    }
    let mut s2 = slot(0, 0.0, 440.0);
    s2.header.stream_id = 99;
    s2.header.timestamp = 5_000_000;
    tx.push(s2).ok().unwrap();
    e.render(&mut buf, 0.1);
    assert_eq!(e.jitter().format().unwrap().stream_id, 99);
    assert_eq!(e.jitter().counters.resets, 2);
    assert_eq!(e.jitter().counters.late, 0);
}

/// THD+N of a resampled 1 kHz sine: least-squares fit of the expected
/// sinusoid (free amplitude and phase, frequency from the ratio), residual
/// power over fitted power.
fn thd_n_db(ratio: f64, freq: f64) -> f64 {
    let table = KernelTable::new(1.0);
    let mut rs = Resampler::new();
    rs.reset(1);
    let mut n_in = 0u64;
    let total = 96_000;
    let mut out = Vec::with_capacity(total);
    let mut frame = [0f32; MAX_CHANNELS];
    for _ in 0..total {
        rs.next_frame(&table, ratio, &mut frame, |f| {
            f[0] = (0.5 * (2.0 * std::f64::consts::PI * freq * n_in as f64 / RATE as f64).sin())
                as f32;
            n_in += 1;
        });
        out.push(frame[0] as f64);
    }
    // Skip filter warm-up.
    let x = &out[4800..];
    let w = 2.0 * std::f64::consts::PI * freq * ratio / RATE as f64;
    let (mut ss, mut sc, mut cc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = (w * i as f64).sin_cos();
        ss += s * s;
        sc += s * c;
        cc += c * c;
        xs += v * s;
        xc += v * c;
    }
    let det = ss * cc - sc * sc;
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;
    let mut sig = 0.0;
    let mut err = 0.0;
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = (w * i as f64).sin_cos();
        let fit = a * s + b * c;
        sig += fit * fit;
        err += (v - fit) * (v - fit);
    }
    10.0 * (err / sig).log10()
}

#[test]
fn resampler_thd_n_below_minus_90_db() {
    for ratio in [1.0, 1.0 + 200e-6, 1.0 - 200e-6, 1.001, 0.999, 1.0 + 37e-6] {
        let db = thd_n_db(ratio, 1000.0);
        eprintln!("resampler THD+N at 1 kHz, ratio {ratio:.6}: {db:.1} dB");
        assert!(db < -90.0, "ratio {ratio}: THD+N {db:.1} dB");
    }
}

#[test]
fn resampler_passband_is_flat_to_20khz() {
    // Amplitude of a 20 kHz tone after resampling, via the same fit.
    let table = KernelTable::new(1.0);
    for freq in [10_000.0, 20_000.0] {
        let mut rs = Resampler::new();
        rs.reset(1);
        let mut n = 0u64;
        let mut out = Vec::new();
        let mut frame = [0f32; MAX_CHANNELS];
        for _ in 0..48_000 {
            rs.next_frame(&table, 1.0 + 100e-6, &mut frame, |f| {
                f[0] = (2.0 * std::f64::consts::PI * freq * n as f64 / RATE as f64).sin() as f32;
                n += 1;
            });
            out.push(frame[0]);
        }
        let rms = (out[4800..].iter().map(|v| (*v as f64).powi(2)).sum::<f64>()
            / (out.len() - 4800) as f64)
            .sqrt();
        let db = 20.0 * (rms * 2f64.sqrt()).log10();
        eprintln!("resampler response at {freq} Hz: {db:.2} dB");
        assert!(db.abs() < 0.5, "{freq} Hz: {db:.2} dB");
    }
}

#[test]
fn file_backend_writes_paced_wav() {
    use tightwire_recv::output::{Output, file::FileOutput};
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("file_backend.wav");
    let out = Box::new(FileOutput::new(path.clone(), RATE, 2, PERIOD, 0.0));
    let (_tx, e, _) = engine(2.0);
    let running = out.start(e).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    running.stop().unwrap();
    let r = hound::WavReader::open(&path).unwrap();
    assert_eq!(r.spec().channels, 2);
    assert_eq!(r.spec().sample_format, hound::SampleFormat::Float);
    let secs = r.duration() as f64 / RATE as f64;
    // Paced by the virtual clock: roughly real time, not as fast as possible.
    assert!(
        (0.25..0.4).contains(&secs),
        "wrote {secs} s for 0.3 s of wall time"
    );
}

/// A marker packet's first frame is reported at the render instant it
/// leaves the resampler, on the same clock as arrivals: the measured
/// sender→output time must equal network delay + buffer wait + filter delay.
#[test]
fn latency_marker_reports_when_its_frame_is_played() {
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    use tightwire_recv::protocol::FLAG_MARKER;
    let (mut tx, mut e, stats) = engine(0.0);
    let mut buf = vec![0f32; PERIOD];
    let net_delay = 0.0003;
    let marker_seq = 2000u32;
    let mut next = 0u32;
    for t in 0..(3.0 * RATE as f64 / PERIOD as f64) as usize {
        let now = t as f64 * PERIOD as f64 / RATE as f64;
        // Packet s holds frames s*48..; it is complete (sent) at (s+1) ms.
        while (next as f64 + 1.0) * PACKET as f64 / RATE as f64 + net_delay <= now {
            let mut s = slot(next, (next as f64 + 1.0) * 0.001 + net_delay, 440.0);
            if next == marker_seq {
                s.header.flags |= FLAG_MARKER;
            }
            tx.push(s).ok().unwrap();
            next += 1;
        }
        e.render(&mut buf, now);
    }
    let ts = stats.marker_timestamp.load(Acquire);
    assert_eq!(ts, marker_seq as u64 * PACKET as u64);
    let out_s = stats.marker_output_us.load(Relaxed) as f64 / 1e6;
    // Sender side: the packet left when its last frame was captured.
    let sent_s = (marker_seq as f64 + 1.0) * PACKET as f64 / RATE as f64;
    let measured_ms = (out_s - sent_s) * 1000.0;
    // Expected: the buffer held `depth` ahead of the read point, measured to
    // the packet's end; its first frame is one packet earlier than its end,
    // and the resampler adds TAPS/2 frames.
    let depth_ms = e.drift().smoothed_depth_s().unwrap() * 1000.0;
    let filter_ms = tightwire_recv::resample::TAPS as f64 / 2.0 / RATE as f64 * 1000.0;
    eprintln!(
        "marker: measured {measured_ms:.3} ms, depth {depth_ms:.3} ms, filter {filter_ms:.3} ms"
    );
    assert!(
        measured_ms > 0.0 && measured_ms < depth_ms + filter_ms + 1.5,
        "measured {measured_ms:.3} ms vs depth {depth_ms:.3} ms"
    );
}

#[test]
fn tuning_request_changes_floor_and_cap_on_the_audio_thread() {
    let (mut tx, mut e, stats) = engine(0.0);
    let mut buf = vec![0f32; PERIOD];
    for s in 0..200u32 {
        let now = s as f64 * 0.001;
        tx.push(slot(s, now + 0.0003, 440.0)).ok().unwrap();
        e.render(&mut buf, now);
    }
    assert!(e.jitter().target_s() < 0.004);
    stats.request_limits(6000, 12000);
    e.render(&mut buf, 0.2);
    assert_eq!(e.jitter().limits_s(), (0.006, 0.012));
    assert!(e.jitter().target_s() >= 0.006);
    assert_eq!(
        stats
            .min_target_us
            .load(std::sync::atomic::Ordering::Relaxed),
        6000
    );
}

/// README "Clock drift": two hours of stream time on the virtual clock, the
/// sender's crystal `ppm` off, a 48-frame packet every 1 ms with 0–0.5 ms of
/// random network delay plus a 3 ms stall every `stall_every` packets,
/// played by a device with a 480-frame (10 ms) period like a typical
/// shared-mode endpoint. Returns (underruns, depth min, depth max, mean
/// correction ppm, peak target).
fn long_drift_case(ppm: f64, stall_every: u32) -> (u64, f64, f64, f64, f64) {
    const OUT_PERIOD: usize = 480;
    let stats = Arc::new(SharedStats::default());
    let (mut tx, mut e) = engine::channel(
        EngineConfig {
            out_rate: RATE,
            out_channels: 1,
            period_frames: OUT_PERIOD,
            min_target_s: 0.0,
            max_target_s: 0.020,
        },
        stats,
    );
    let sender_rate = RATE as f64 * (1.0 + ppm * 1e-6);
    // Content is irrelevant here; one template slot avoids 345 M sin() calls.
    let mut template = slot(0, 0.0, 440.0);
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut seq = 0u32;
    let mut buf = vec![0f32; OUT_PERIOD];
    let seconds = 2.0 * 3600.0;
    let ticks = (seconds * RATE as f64 / OUT_PERIOD as f64) as usize;
    let (mut dmin, mut dmax) = (f64::MAX, f64::MIN);
    let mut corr_sum = 0.0;
    let mut corr_n = 0u64;
    let mut max_target = 0f64;
    for t in 0..ticks {
        let now = t as f64 * OUT_PERIOD as f64 / RATE as f64;
        loop {
            let send = (seq as f64 + 1.0) * PACKET as f64 / sender_rate;
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let mut delay = (rng % 500) as f64 * 1e-6;
            // A 3 ms stall (three packets held, the rest queue behind them).
            if seq % stall_every < 3 {
                delay += 0.003;
            }
            if send + delay > now {
                break;
            }
            template.header = header(seq, seq as u64 * PACKET as u64);
            template.arrival_s = send + delay;
            tx.push(template.clone()).ok().unwrap();
            seq += 1;
        }
        e.render(&mut buf, now);
        if now > 60.0
            && let Some(d) = e.drift().smoothed_depth_s()
        {
            let d = d * 1000.0;
            dmin = dmin.min(d);
            dmax = dmax.max(d);
            corr_sum += e.drift().correction();
            corr_n += 1;
            max_target = max_target.max(e.jitter().target_s() * 1000.0);
        }
    }
    let c = e.jitter().counters;
    let mean_ppm = corr_sum / corr_n as f64 * 1e6;
    eprintln!(
        "2 h at {ppm:+} ppm, stall every {stall_every} packets: smoothed depth {dmin:.2}–{dmax:.2} ms (target peak {max_target:.2} ms), correction mean {mean_ppm:+.1} ppm, underruns {}, late {}, concealed frames {}",
        c.underruns, c.late, c.concealed_frames
    );
    assert_eq!(c.late, 0, "{ppm} ppm over 2 h: {c:?}");
    assert_eq!(c.lost, 0);
    if stall_every == u32::MAX {
        assert!((mean_ppm - ppm).abs() < 5.0, "correction {mean_ppm:.1} ppm");
    }
    assert!(dmax <= max_target + 1.0, "depth ran away: {dmax:.2} ms");
    assert!(dmin > 0.5, "depth fell to {dmin:.2} ms");
    (c.underruns, dmin, dmax, mean_ppm, max_target)
}

#[test]
fn two_hours_at_plus_100ppm_stays_bounded_without_underruns() {
    assert_eq!(long_drift_case(100.0, u32::MAX).0, 0);
}

#[test]
fn two_hours_at_minus_100ppm_stays_bounded_without_underruns() {
    assert_eq!(long_drift_case(-100.0, u32::MAX).0, 0);
}

/// Characterisation, not a goal: an isolated 3 ms stall after a quiet
/// minute exceeds the adaptive margin (~1–1.5 ms once the jitter peak has
/// decayed) and can cause one underrun; it must never cascade, and drift
/// tracking and depth must stay bounded. A latency test that sees this
/// recommends a fixed floor (see README "Tuning").
#[test]
fn two_hours_with_a_stall_every_minute_at_most_one_underrun_per_stall() {
    let stalls = 2 * 60;
    let (underruns, ..) = long_drift_case(100.0, 60_000);
    assert!(
        underruns <= stalls,
        "{underruns} underruns for {stalls} stalls"
    );
}

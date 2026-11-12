//! Loopback verifier (SPEC §7.3): the received WAV must hold one continuous
//! sine at the expected frequency, with no discontinuities after the first
//! 500 ms of audio. Exits non-zero on failure.
//!
//! Discontinuity test: a pure sine obeys x[n+1] − 2cos(ω)x[n] + x[n−1] = 0, so
//! that residual stays tiny except where samples were dropped, repeated,
//! faded or glitched.

use std::f64::consts::PI;

use clap::Parser;

#[derive(Parser)]
#[command(about = "Check a Tightwire loopback recording for a clean, continuous sine")]
struct Args {
    wav: std::path::PathBuf,
    /// Expected tone frequency (Hz).
    #[arg(long, default_value_t = 1000.0)]
    freq: f64,
    /// Ignore this much audio after the tone starts (ms).
    #[arg(long, default_value_t = 500.0)]
    skip_ms: f64,
    /// Allowed frequency error (Hz). Drift correction shifts pitch by up to 0.1 %.
    #[arg(long, default_value_t = 1.5)]
    freq_tol: f64,
    /// Largest allowed residual, relative to the tone amplitude.
    #[arg(long, default_value_t = 0.01)]
    max_residual: f64,
}

fn main() {
    let a = Args::parse();
    let mut r = hound::WavReader::open(&a.wav)
        .unwrap_or_else(|e| fail(&format!("{}: {e}", a.wav.display())));
    let spec = r.spec();
    let ch = spec.channels as usize;
    let rate = spec.sample_rate as f64;
    let all: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => r
            .samples::<i32>()
            .map(|s| s.unwrap() as f32 / 2f32.powi(spec.bits_per_sample as i32 - 1))
            .collect(),
    };
    let x: Vec<f64> = all.chunks_exact(ch).map(|f| f[0] as f64).collect();

    let thresh = 0.01;
    let Some(first) = x.iter().position(|v| v.abs() > thresh) else {
        fail("no audio in recording")
    };
    let last = x.iter().rposition(|v| v.abs() > thresh).unwrap();
    // Leave out the start-up and the fade when the sender stops.
    let start = first + (a.skip_ms / 1000.0 * rate) as usize;
    let end = last.saturating_sub((0.05 * rate) as usize);
    if end <= start + (rate as usize) {
        fail(&format!(
            "only {:.2} s of steady audio",
            (end.saturating_sub(start)) as f64 / rate
        ));
    }
    let seg = &x[start..end];

    // Frequency from interpolated rising zero crossings.
    let mut crossings = vec![];
    for i in 1..seg.len() {
        if seg[i - 1] < 0.0 && seg[i] >= 0.0 {
            crossings.push(i as f64 - 1.0 + seg[i - 1] / (seg[i - 1] - seg[i]));
        }
    }
    if crossings.len() < 10 {
        fail("too few zero crossings");
    }
    let periods = (crossings.len() - 1) as f64;
    let freq = periods / ((crossings[crossings.len() - 1] - crossings[0]) / rate);

    let amp = seg.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let w = 2.0 * PI * freq / rate;
    let c = 2.0 * w.cos();
    let (mut worst, mut worst_at) = (0.0f64, 0usize);
    for i in 1..seg.len() - 1 {
        let e = (seg[i + 1] - c * seg[i] + seg[i - 1]).abs();
        if e > worst {
            worst = e;
            worst_at = i;
        }
    }
    let rel = worst / amp;

    // Any 1 ms block whose RMS drops well below the tone's means a gap/fade.
    let block = (rate / 1000.0) as usize;
    let rms_ref = amp / 2f64.sqrt();
    let quiet_blocks = seg
        .chunks_exact(block)
        .filter(|b| (b.iter().map(|v| v * v).sum::<f64>() / b.len() as f64).sqrt() < 0.5 * rms_ref)
        .count();

    let ok_freq = (freq - a.freq).abs() <= a.freq_tol;
    let ok_cont = rel <= a.max_residual && quiet_blocks == 0;
    println!(
        "wavcheck {} steady_s={:.2} freq_hz={freq:.3} amplitude={amp:.4} max_residual_rel={rel:.2e} (at {:.4} s) quiet_1ms_blocks={quiet_blocks} result={}",
        a.wav.display(),
        seg.len() as f64 / rate,
        (start + worst_at) as f64 / rate,
        if ok_freq && ok_cont { "PASS" } else { "FAIL" }
    );
    if !ok_freq {
        fail(&format!(
            "frequency {freq:.3} Hz, expected {} ± {}",
            a.freq, a.freq_tol
        ));
    }
    if !ok_cont {
        fail("discontinuity in steady-state audio");
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("wavcheck: {msg}");
    std::process::exit(1)
}

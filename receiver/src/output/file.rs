//! WAV-writing backend driven by a virtual device clock, for deterministic
//! tests and the loopback check. Not a real-time path: it writes to disk from
//! its own thread, which is fine because nothing here is a hardware deadline.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{Output, OutputFormat, Result, Running};
use crate::clock;
use crate::engine::Engine;

pub struct FileOutput {
    path: PathBuf,
    format: OutputFormat,
    /// Simulated device crystal error: positive = device runs fast.
    clock_ppm: f64,
}

impl FileOutput {
    pub fn new(
        path: PathBuf,
        rate: u32,
        channels: usize,
        period_frames: usize,
        clock_ppm: f64,
    ) -> Self {
        Self {
            path,
            format: OutputFormat {
                rate,
                channels,
                period_frames,
                mode: crate::protocol::OutputMode::File,
            },
            clock_ppm,
        }
    }
}

impl Output for FileOutput {
    fn format(&self) -> OutputFormat {
        self.format
    }

    fn describe(&self) -> String {
        format!(
            "file {} ({} Hz, {} ch, {} frames/period, clock {:+} ppm)",
            self.path.display(),
            self.format.rate,
            self.format.channels,
            self.format.period_frames,
            self.clock_ppm
        )
    }

    fn start(self: Box<Self>, mut engine: Engine) -> Result<Box<dyn Running>> {
        let spec = hound::WavSpec {
            channels: self.format.channels as u16,
            sample_rate: self.format.rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&self.path, spec)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let fmt = self.format;
        let period = Duration::from_secs_f64(
            fmt.period_frames as f64 / (fmt.rate as f64 * (1.0 + self.clock_ppm * 1e-6)),
        );
        let thread = std::thread::Builder::new()
            .name("tightwire-file-out".into())
            .spawn(move || -> Result<()> {
                clock::promote_to_realtime(period);
                let mut buf = vec![0f32; fmt.period_frames * fmt.channels];
                let start = Instant::now();
                let mut k: u32 = 0;
                while !stop_flag.load(Ordering::Relaxed) {
                    let deadline = start + period * k;
                    let now = Instant::now();
                    if deadline > now {
                        std::thread::sleep(deadline - now);
                    }
                    let late = Instant::now()
                        .saturating_duration_since(deadline)
                        .as_micros() as u32;
                    engine
                        .stats()
                        .output_late_us
                        .fetch_max(late, std::sync::atomic::Ordering::Relaxed);
                    engine.render(&mut buf, clock::now_s());
                    for &s in &buf {
                        writer.write_sample(s)?;
                    }
                    k += 1;
                }
                writer.finalize()?;
                Ok(())
            })?;
        Ok(Box::new(FileRunning { stop, thread }))
    }
}

struct FileRunning {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<()>>,
}

impl Running for FileRunning {
    fn stop(self: Box<Self>) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .join()
            .map_err(|_| "file output thread panicked")?
    }
}

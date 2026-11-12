//! cpal backend: development on macOS and a fallback on Windows (cpal uses
//! WASAPI shared mode there, without the small-period tuning of `wasapi`).

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::{Output, OutputFormat, Result, Running};
use crate::clock;
use crate::engine::Engine;

pub fn list_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let default = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_owned());
    let mut out = vec![];
    for d in host.output_devices()? {
        let name = d
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "?".into());
        let cfg = d
            .default_output_config()
            .map(|c| {
                format!(
                    "{} Hz, {} ch, {:?}",
                    c.sample_rate(),
                    c.channels(),
                    c.sample_format()
                )
            })
            .unwrap_or_else(|e| format!("({e})"));
        let mark = if Some(&name) == default.as_ref() {
            "*"
        } else {
            " "
        };
        out.push(format!("{mark} {name}  [{cfg}]"));
    }
    Ok(out)
}

pub struct CpalOutput {
    device: cpal::Device,
    name: String,
    config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    period: usize,
}

impl CpalOutput {
    /// Opens the first output whose name contains `device` (case-insensitive),
    /// or the default output. `period` frames is requested if the device
    /// allows it.
    pub fn open(device: Option<&str>, period: usize) -> Result<Self> {
        let host = cpal::default_host();
        let dev = match device {
            Some(want) => {
                let want = want.to_lowercase();
                host.output_devices()?
                    .find(|d| {
                        d.description()
                            .map(|n| n.name().to_lowercase().contains(&want))
                            .unwrap_or(false)
                    })
                    .ok_or_else(|| {
                        format!("no output device matching \"{want}\" (see --list-devices)")
                    })?
            }
            None => host
                .default_output_device()
                .ok_or("no default output device")?,
        };
        let name = dev
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_default();
        let supported = dev.default_output_config()?;
        let sample_format = supported.sample_format();
        let mut config = supported.config();
        let fixed = match supported.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                Some((period as u32).clamp(*min, *max))
            }
            cpal::SupportedBufferSize::Unknown => None,
        };
        if let Some(f) = fixed {
            config.buffer_size = cpal::BufferSize::Fixed(f);
        }
        Ok(Self {
            device: dev,
            name,
            period: fixed.map_or(512, |f| f as usize),
            config,
            sample_format,
        })
    }
}

/// Upper bound on frames per callback we preallocate conversion space for.
const MAX_CALLBACK_FRAMES: usize = 16_384;

impl Output for CpalOutput {
    fn format(&self) -> OutputFormat {
        OutputFormat {
            rate: self.config.sample_rate,
            channels: self.config.channels as usize,
            period_frames: self.period,
            mode: crate::protocol::OutputMode::Cpal,
        }
    }

    fn describe(&self) -> String {
        format!(
            "cpal \"{}\" ({} Hz, {} ch, {:?}, {:?})",
            self.name,
            self.config.sample_rate,
            self.config.channels,
            self.sample_format,
            self.config.buffer_size
        )
    }

    fn start(self: Box<Self>, mut engine: Engine) -> Result<Box<dyn Running>> {
        let failed = Arc::new(Mutex::new(None::<String>));
        let f2 = failed.clone();
        // The error callback is not on the real-time path.
        let on_err = move |e: cpal::Error| {
            if let Ok(mut g) = f2.lock() {
                *g = Some(e.to_string());
            }
        };
        let ch = self.config.channels as usize;
        let stream = match self.sample_format {
            cpal::SampleFormat::F32 => self.device.build_output_stream::<f32, _, _>(
                self.config,
                move |data, _| engine.render(data, clock::now_s()),
                on_err,
                None,
            )?,
            cpal::SampleFormat::I16 => {
                let mut scratch = vec![0f32; MAX_CALLBACK_FRAMES * ch];
                self.device.build_output_stream::<i16, _, _>(
                    self.config,
                    move |data, _| {
                        let n = data.len().min(scratch.len());
                        engine.render(&mut scratch[..n], clock::now_s());
                        for (o, s) in data.iter_mut().zip(&scratch[..n]) {
                            *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    },
                    on_err,
                    None,
                )?
            }
            cpal::SampleFormat::I32 => {
                let mut scratch = vec![0f32; MAX_CALLBACK_FRAMES * ch];
                self.device.build_output_stream::<i32, _, _>(
                    self.config,
                    move |data, _| {
                        let n = data.len().min(scratch.len());
                        engine.render(&mut scratch[..n], clock::now_s());
                        for (o, s) in data.iter_mut().zip(&scratch[..n]) {
                            *o = (s.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32;
                        }
                    },
                    on_err,
                    None,
                )?
            }
            other => return Err(format!("unsupported device sample format {other:?}").into()),
        };
        stream.play()?;
        Ok(Box::new(CpalRunning {
            _stream: stream,
            failed,
        }))
    }
}

struct CpalRunning {
    _stream: cpal::Stream,
    failed: Arc<Mutex<Option<String>>>,
}

impl Running for CpalRunning {
    fn failed(&self) -> Option<String> {
        self.failed.lock().ok().and_then(|g| g.clone())
    }

    fn stop(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

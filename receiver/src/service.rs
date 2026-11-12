//! A running receiver (socket thread + output backend), shared by the CLI and
//! the Windows tray app.

use std::net::{IpAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::engine::{self, EngineConfig, SharedStats};
use crate::net::{self, KeySource, NetHandle, NetStatus};
use crate::output::{self, Output, Result, Running};
use crate::tuning::{self, Tuning};

#[derive(Debug, Clone)]
pub enum Backend {
    /// WASAPI (Windows only) on the endpoint named by `device_id` or
    /// `device`; exclusive first unless `shared`. Never the system default
    /// implicitly.
    Wasapi,
    Cpal,
    /// No output opened; network, pairing and handoff still run.
    Discard {
        reason: String,
    },
    File {
        path: PathBuf,
        rate: u32,
        channels: usize,
        clock_ppm: f64,
    },
}

pub struct Options {
    pub backend: Backend,
    /// Name substring (CLI `--device`).
    pub device: Option<String>,
    /// WASAPI endpoint id (the tray's saved choice); wins over `device`.
    pub device_id: Option<String>,
    pub shared: bool,
    pub period: Option<usize>,
    pub port: u16,
    pub bind: IpAddr,
    pub allow: Option<IpAddr>,
    /// Command-line floor/cap; None = `tuning.conf` (Mac-set), else defaults.
    pub target_ms: Option<f64>,
    pub max_ms: Option<f64>,
    pub keys: KeySource,
    pub usbip: Option<PathBuf>,
}

pub struct Receiver {
    pub stats: Arc<SharedStats>,
    pub description: String,
    /// Format the output runs at (nominal for `Discard`).
    pub format: output::OutputFormat,
    net: NetHandle,
    running: Box<dyn Running>,
}

impl Receiver {
    pub fn start(o: Options) -> Result<Self> {
        let out: Box<dyn Output> = match &o.backend {
            Backend::File {
                path,
                rate,
                channels,
                clock_ppm,
            } => Box::new(output::file::FileOutput::new(
                path.clone(),
                *rate,
                *channels,
                o.period.unwrap_or(48),
                *clock_ppm,
            )),
            Backend::Cpal => Box::new(output::cpal_out::CpalOutput::open(
                o.device.as_deref(),
                o.period.unwrap_or(64),
            )?),
            Backend::Discard { reason } => Box::new(output::discard::DiscardOutput::new(reason)),
            #[cfg(windows)]
            Backend::Wasapi => {
                use output::wasapi::DeviceSel;
                let sel = match (&o.device_id, &o.device) {
                    (Some(id), _) => DeviceSel::Id(id.clone()),
                    (None, Some(name)) => DeviceSel::Name(name.clone()),
                    (None, None) => {
                        return Err(
                            "no output device chosen (see --list-devices, then --device)".into(),
                        );
                    }
                };
                Box::new(output::wasapi::WasapiOutput::open(sel, !o.shared)?)
            }
            #[cfg(not(windows))]
            Backend::Wasapi => {
                return Err("the wasapi backend is Windows-only; use --backend cpal".into());
            }
        };
        let fmt = out.format();
        let description = out.describe();
        crate::twlog!("output: {description}");
        let stats = Arc::new(SharedStats::default());
        let tuning_path = match &o.keys {
            KeySource::Config(p) => Some(tuning::path_for(p)),
            KeySource::Psk(_) => None,
        };
        let saved = match tuning_path.as_deref().map(Tuning::load) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                crate::twlog!("warning: ignoring {e}; using default buffer limits");
                None
            }
            None => None,
        };
        let limits = tuning::effective(o.target_ms, o.max_ms, saved);
        crate::twlog!(
            "jitter buffer: {}{}",
            limits.describe(),
            match (saved.is_some(), o.target_ms.is_some() || o.max_ms.is_some()) {
                (_, true) => " (command line)",
                (true, false) => " (set from the Mac, tuning.conf)",
                (false, false) => " (defaults)",
            }
        );
        let (tx, eng) = engine::channel(
            EngineConfig {
                out_rate: fmt.rate,
                out_channels: fmt.channels,
                period_frames: fmt.period_frames,
                min_target_s: limits.min_target_us as f64 / 1e6,
                max_target_s: limits.max_target_us as f64 / 1e6,
            },
            stats.clone(),
        );
        let sock =
            UdpSocket::bind((o.bind, o.port)).map_err(|e| format!("UDP port {}: {e}", o.port))?;
        crate::twlog!("listening on UDP {}", sock.local_addr()?);
        let net = net::spawn(
            sock,
            net::NetOptions {
                keys: o.keys,
                allow: o.allow,
                usbip: o.usbip,
                output: fmt,
                tuning_path,
            },
            tx,
            stats.clone(),
        )?;
        let running = out.start(eng)?;
        Ok(Self {
            stats,
            description,
            format: fmt,
            net,
            running,
        })
    }

    /// Handoff outcomes since the last call (oldest first).
    pub fn take_handoff_events(&self) -> Vec<crate::net::HandoffEvent> {
        self.net
            .handoff_events
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    pub fn failed(&self) -> Option<String> {
        self.running.failed()
    }

    pub fn net_status(&self) -> NetStatus {
        self.net
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub fn stop(self) -> Result<()> {
        self.net.stop();
        self.running.stop()
    }

    pub fn status_line(&self) -> String {
        status_line(&self.stats, &self.net_status())
    }
}

/// Short state for menus: unpaired / searching / streaming.
pub fn phase(s: &SharedStats, n: &NetStatus) -> &'static str {
    if n.paired_with.is_none() {
        "unpaired"
    } else if s.playing.load(Relaxed) == 1 {
        "streaming"
    } else {
        "searching"
    }
}

pub fn status_line(s: &SharedStats, n: &NetStatus) -> String {
    let peer = match (&n.paired_with, n.peer) {
        (None, _) => "unpaired".to_owned(),
        (Some(name), Some(a)) => format!("{name} @ {a}"),
        (Some(name), None) => format!("waiting for {name}"),
    };
    let stream = if s.playing.load(Relaxed) == 1 {
        format!(
            "{} Hz {} ch",
            s.stream_rate.load(Relaxed),
            s.stream_channels.load(Relaxed)
        )
    } else {
        "idle".into()
    };
    format!(
        "buf {:.2}/{:.2} ms  jitter {:.2} ms  rx {}  lost {}  late {}  dup {}  underrun {}  drift {:+} ppm  auth-drop {}  | {stream} | {peer}",
        s.depth_us.load(Relaxed) as f64 / 1000.0,
        s.target_us.load(Relaxed) as f64 / 1000.0,
        s.jitter_us.load(Relaxed) as f64 / 1000.0,
        s.received.load(Relaxed),
        s.lost.load(Relaxed),
        s.late.load(Relaxed),
        s.duplicates.load(Relaxed),
        s.underruns.load(Relaxed),
        s.drift_ppm.load(Relaxed),
        s.auth_dropped.load(Relaxed) + s.replayed.load(Relaxed),
    )
}

//! tightwire-recv / tightwire.exe: plays the paired Mac's Tightwire stream on
//! a local output. On Windows, started without arguments it runs as a tray
//! app (SPEC §10); with arguments it is the CLI.

// GUI subsystem on Windows so the tray app (and Stream Deck buttons running
// `tightwire.exe deck …`) never open a console window; CLI use re-attaches to
// the parent console in `main`.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};

use tightwire_recv::auth::{self, SessionKeys};
use tightwire_recv::engine::SharedStats;
use tightwire_recv::net::KeySource;
use tightwire_recv::output;
use tightwire_recv::pairing::{self, ConfigFile, PairEvent, PairOptions};
use tightwire_recv::service::{self, Receiver};
use tightwire_recv::{clock, protocol};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// WASAPI (Windows only): shared mode, or exclusive with --exclusive.
    Wasapi,
    /// cpal default host (any OS).
    Cpal,
    /// Write a WAV file on a virtual clock (tests).
    File,
}

#[derive(Parser)]
#[command(
    name = "tightwire-recv",
    version,
    about = "Plays the paired Mac's Tightwire stream with minimal latency."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// List output devices and exit.
    #[arg(long)]
    list_devices: bool,
    /// Output device: case-insensitive substring of its name.
    #[arg(long)]
    device: Option<String>,
    #[arg(long, default_value_t = protocol::DEFAULT_PORT)]
    port: u16,
    #[arg(long, default_value = "0.0.0.0", global = true)]
    bind: IpAddr,
    /// Only accept packets from this address.
    #[arg(long)]
    allow: Option<IpAddr>,
    /// Minimum jitter-buffer target (ms); default adapts from one packet + one device period.
    #[arg(long)]
    target_ms: Option<f64>,
    /// Jitter-buffer cap (ms); default 20, or what the Mac set (tuning.conf).
    #[arg(long)]
    max_ms: Option<f64>,
    /// WASAPI: try exclusive mode first (experimental for virtual devices;
    /// falls back to shared if refused).
    #[arg(long, conflicts_with = "shared")]
    exclusive: bool,
    /// WASAPI: shared mode at the smallest period (the default since 0.2.1;
    /// kept for scripts).
    #[arg(long)]
    shared: bool,
    #[arg(long, value_enum)]
    backend: Option<Backend>,
    /// Device period in frames to request (cpal, file).
    #[arg(long)]
    period: Option<usize>,
    /// Pairing file (default: per-user config directory, see README).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// TEST ONLY: use a 32-byte pre-shared root key (64 hex chars) instead of pairing.
    /// Anyone who sees the command line has the key.
    #[arg(long, value_name = "HEX")]
    psk_hex: Option<String>,
    /// file backend: WAV to write.
    #[arg(long)]
    out: Option<PathBuf>,
    /// file backend: output sample rate.
    #[arg(long, default_value_t = 48_000)]
    file_rate: u32,
    /// file backend: output channels.
    #[arg(long, default_value_t = 2)]
    file_channels: usize,
    /// file backend: simulated device clock error (ppm, positive = fast).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    clock_ppm: f64,
    /// Stop after this many seconds and print a summary.
    #[arg(long)]
    duration: Option<f64>,
    /// usbip.exe for device handoff (default: C:\Program Files\USBip\usbip.exe, then PATH).
    #[arg(long, global = true)]
    usbip: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with a Mac: announce on the LAN for up to 120 s and confirm the code.
    Pair {
        /// UDP port for the handshake (announced in beacons; 0 = any free port).
        #[arg(long, default_value_t = protocol::PAIR_PORT)]
        port: u16,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Name shown on the Mac (default: this computer's name).
        #[arg(long)]
        name: Option<String>,
        /// Do not broadcast beacons (the Mac must be given the address).
        #[arg(long)]
        no_beacon: bool,
        /// TEST ONLY: accept the confirmation code without asking.
        #[arg(long, hide = true)]
        test_auto_accept: bool,
    },
    /// Delete the stored pairing.
    Forget,
    /// Show the stored pairing (never the key).
    Show,
    /// Print a diagnostic report (devices, firewall, usbip, pairing, Mac reachability).
    Doctor,
    /// Ask the paired Mac to move the handoff devices (Stream Deck button).
    Deck {
        /// mac, pc or toggle
        target: String,
        /// The Mac's IP (default: last address it was seen at).
        #[arg(long)]
        mac: Option<IpAddr>,
        /// TEST ONLY: the Mac's handoff port (default 4813).
        #[arg(long, hide = true, default_value_t = protocol::HANDOFF_PORT)]
        mac_port: u16,
    },
}

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn config_path(cli: &Cli) -> Res<PathBuf> {
    cli.config
        .clone()
        .or_else(pairing::default_config_path)
        .ok_or_else(|| "cannot determine config directory; pass --config".into())
}

fn main() {
    clock::now_s();
    #[cfg(windows)]
    {
        if std::env::args_os().len() <= 1 {
            tightwire_recv::tray::run();
            return;
        }
        // CLI mode from a terminal: write to the console we were started from.
        tightwire_recv::winconsole::attach_parent();
    }
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Res<()> {
    match &cli.cmd {
        Some(Cmd::Pair {
            port,
            timeout,
            name,
            no_beacon,
            test_auto_accept,
        }) => {
            return pair(
                &cli,
                *port,
                *timeout,
                name.clone(),
                !*no_beacon,
                *test_auto_accept,
            );
        }
        Some(Cmd::Forget) => {
            let path = config_path(&cli)?;
            let mut c = ConfigFile::load(&path)?;
            if c.pairing.take().is_some() {
                c.save(&path)?;
                println!("forgot pairing ({})", path.display());
            } else {
                println!("not paired");
            }
            return Ok(());
        }
        Some(Cmd::Show) => {
            let path = config_path(&cli)?;
            let c = ConfigFile::load(&path)?;
            println!("config: {}", path.display());
            println!("this receiver: {}", auth::to_hex(&c.self_id));
            match c.pairing {
                Some(p) => println!(
                    "paired with \"{}\" ({}), last seen at {}",
                    p.peer_name,
                    auth::to_hex(&p.peer_id),
                    p.last_address.map_or("-".into(), |a| a.to_string())
                ),
                None => println!("not paired; run `tightwire-recv pair`"),
            }
            return Ok(());
        }
        Some(Cmd::Doctor) => {
            print!(
                "{}",
                tightwire_recv::doctor::report(&config_path(&cli)?, cli.usbip.as_deref())
            );
            return Ok(());
        }
        Some(Cmd::Deck {
            target,
            mac,
            mac_port,
        }) => {
            let t = tightwire_recv::deck::parse_target(target)
                .ok_or("target must be mac, pc or toggle")?;
            // Stream Deck buttons run this without a console: also append to
            // the tray's log so the request and the Mac's answer are visible.
            #[cfg(windows)]
            if tightwire_recv::applog::to_file(&tightwire_recv::applog::default_log_path()).is_ok()
            {
                tightwire_recv::twlog!(
                    "{}",
                    tightwire_recv::applog::start_header("deck command", std::process::id())
                );
            }
            let ack = tightwire_recv::deck::request(
                &config_path(&cli)?,
                t,
                *mac,
                *mac_port,
                Duration::from_secs(4),
            )?;
            println!("{}", tightwire_recv::deck::describe_ack(t, &ack));
            if !ack.accepted {
                std::process::exit(2);
            }
            return Ok(());
        }
        None => {}
    }

    let backend = cli.backend.unwrap_or(if cfg!(windows) {
        Backend::Wasapi
    } else {
        Backend::Cpal
    });
    if cli.list_devices {
        match backend {
            #[cfg(windows)]
            Backend::Wasapi => output::wasapi::list_devices()?
                .iter()
                .for_each(|d| println!("{d}")),
            _ => output::cpal_out::list_devices()?
                .iter()
                .for_each(|d| println!("{d}")),
        }
        return Ok(());
    }

    let keys = match &cli.psk_hex {
        Some(h) => KeySource::Psk(SessionKeys::from_root(
            &auth::parse_hex::<32>(h).ok_or("--psk-hex needs exactly 64 hex characters")?,
        )),
        None => {
            let path = config_path(&cli)?;
            if ConfigFile::load(&path)?.pairing.is_none() {
                eprintln!(
                    "not paired yet: run `tightwire-recv pair` (packets are ignored until then)"
                );
            }
            KeySource::Config(path)
        }
    };
    let backend = match backend {
        Backend::File => service::Backend::File {
            path: cli.out.clone().ok_or("--backend file needs --out <wav>")?,
            rate: cli.file_rate,
            channels: cli.file_channels,
            clock_ppm: cli.clock_ppm,
        },
        Backend::Cpal => service::Backend::Cpal,
        Backend::Wasapi => service::Backend::Wasapi,
    };
    // WASAPI never falls back to the system default: without --device, use
    // the endpoint chosen in the tray, else refuse.
    let device_id = match (&backend, &cli.device) {
        (service::Backend::Wasapi, None) => Some(saved_output()?),
        _ => None,
    };
    let rx = Receiver::start(service::Options {
        backend,
        device: cli.device.clone(),
        device_id,
        shared: !cli.exclusive,
        period: cli.period,
        port: cli.port,
        bind: cli.bind,
        allow: cli.allow,
        target_ms: cli.target_ms,
        max_ms: cli.max_ms,
        keys,
        usbip: cli.usbip.clone(),
    })
    .map_err(|e| tightwire_recv::outsel::describe_open_error(&e.to_string()))?;

    let started = Instant::now();
    let mut depth_samples: Vec<(f64, f64)> = vec![];
    let mut next_status = Instant::now() + Duration::from_secs(1);
    let result = loop {
        std::thread::sleep(Duration::from_millis(100));
        let t = started.elapsed().as_secs_f64();
        if rx.stats.playing.load(Relaxed) == 1 {
            depth_samples.push((t, rx.stats.depth_us.load(Relaxed) as f64 / 1000.0));
        }
        if let Some(e) = rx.failed() {
            break Err(format!("output failed: {e}").into());
        }
        if Instant::now() >= next_status {
            next_status += Duration::from_secs(1);
            eprintln!("{}", rx.status_line());
        }
        if cli.duration.is_some_and(|d| t >= d) {
            break Ok(());
        }
    };
    let stats = rx.stats.clone();
    rx.stop()?;
    if cli.duration.is_some() {
        print_summary(&stats, &depth_samples);
    }
    result
}

/// The tray's saved output endpoint id, if it is present right now.
fn saved_output() -> Res<String> {
    use tightwire_recv::outsel::{self, Plan, TraySettings};
    let path = outsel::tray_settings_path().ok_or("cannot find the settings directory")?;
    let (settings, _) = TraySettings::load(&path)?;
    #[cfg(windows)]
    let endpoints = output::wasapi::render_endpoints()?;
    #[cfg(not(windows))]
    let endpoints: Vec<outsel::Endpoint> = vec![];
    match outsel::plan(&settings, &endpoints) {
        Plan::Open { id, name, .. } => {
            eprintln!("output: \"{name}\" (chosen in the tray)");
            Ok(id)
        }
        Plan::Waiting { name } => Err(format!(
            "the output chosen in the tray, \"{name}\", is not available; pass --device (see --list-devices)"
        )
        .into()),
        Plan::ChooseOutput => Err(
            "no output chosen: pass --device \"<name substring>\" (see --list-devices) or pick one in the tray; the system default is never used"
                .into(),
        ),
    }
}

/// Machine-readable summary for the loopback check. "Steady state" is the
/// second half of the time audio was playing.
fn print_summary(s: &SharedStats, depth: &[(f64, f64)]) {
    let steady: Vec<f64> = if depth.is_empty() {
        vec![]
    } else {
        let mid = (depth[0].0 + depth[depth.len() - 1].0) / 2.0;
        depth
            .iter()
            .filter(|(t, _)| *t >= mid)
            .map(|(_, d)| *d)
            .collect()
    };
    let avg = steady.iter().sum::<f64>() / steady.len().max(1) as f64;
    let max = steady.iter().cloned().fold(0.0, f64::max);
    println!(
        "summary depth_avg_ms={avg:.3} depth_max_ms={max:.3} target_ms={:.3} received={} lost={} late={} duplicates={} underruns={} resets={} concealed_frames={} drift_ppm={} auth_dropped={} replayed={} ring_full={} output_late_max_ms={:.3} floor_ms={:.3} cap_ms={:.3} stream_rate={} stream_channels={}",
        s.target_us.load(Relaxed) as f64 / 1000.0,
        s.received.load(Relaxed),
        s.lost.load(Relaxed),
        s.late.load(Relaxed),
        s.duplicates.load(Relaxed),
        s.underruns.load(Relaxed),
        s.resets.load(Relaxed),
        s.concealed_frames.load(Relaxed),
        s.drift_ppm.load(Relaxed),
        s.auth_dropped.load(Relaxed),
        s.replayed.load(Relaxed),
        s.ring_full.load(Relaxed),
        s.output_late_us.load(Relaxed) as f64 / 1000.0,
        s.min_target_us.load(Relaxed) as f64 / 1000.0,
        s.max_target_us.load(Relaxed) as f64 / 1000.0,
        s.stream_rate.load(Relaxed),
        s.stream_channels.load(Relaxed),
    );
}

fn pair(
    cli: &Cli,
    port: u16,
    timeout: u64,
    name: Option<String>,
    beacon: bool,
    auto: bool,
) -> Res<()> {
    let path = config_path(cli)?;
    let mut config = ConfigFile::load(&path)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let opts = PairOptions {
        port,
        bind: cli.bind,
        timeout: Duration::from_secs(timeout),
        name: name.unwrap_or_else(pairing::host_name),
        beacon,
    };
    if let Some(p) = &config.pairing {
        eprintln!(
            "currently paired with \"{}\"; pairing again replaces it",
            p.peer_name
        );
    }
    let record = pairing::run_pairing(&opts, &config, rx, |ev| match ev {
        PairEvent::ShowCode { code, peer_name } => {
            println!("{}", pairing::code_lines(code, peer_name));
            if auto {
                let _ = tx.send(true);
            } else {
                let tx = tx.clone();
                // stdin blocks, so ask on a helper thread while the
                // handshake keeps running.
                std::thread::spawn(move || {
                    use std::io::Write;
                    print!("Does the Mac show the same code? [y/N] ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    let ok = std::io::stdin().read_line(&mut line).is_ok()
                        && matches!(line.trim(), "y" | "Y" | "yes");
                    let _ = tx.send(ok);
                });
            }
        }
        PairEvent::Paired(_) => {}
        PairEvent::Aborted(m) => eprintln!("pairing aborted: {m}"),
    })?;
    let fp = auth::to_hex(&auth::sha256(&record.root)[..4]);
    let peer = record.peer_name.clone();
    config.pairing = Some(record);
    config.save(&path)?;
    println!(
        "paired with \"{peer}\" (key fingerprint {fp}); saved to {}",
        path.display()
    );
    Ok(())
}

//! `tightwire doctor`: one report with everything needed to diagnose a
//! Windows setup remotely (SPEC §10). Changes nothing; the only device it
//! opens is the user's chosen output, for one second, and only when the
//! running tray is not already playing into it.

use std::fmt::Write;
use std::net::UdpSocket;
use std::path::Path;
use std::time::Duration;

use crate::handoff::{self, CommandRunner, Reach, SystemRunner};
use crate::outsel::{self, Plan, TraySettings};
use crate::pairing::ConfigFile;
use crate::protocol;
use crate::runstatus::{self, RuntimeStatus};

fn run(program: &Path, args: &[&str]) -> Result<handoff::CommandOutput, String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    SystemRunner.run(program, &args, Duration::from_secs(10))
}

/// Indented copy of tool output (at most `max` lines).
fn block(r: &mut String, text: &str, max: usize) {
    let clean = crate::applog::strip_ansi(text);
    let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    for l in lines.iter().take(max) {
        let _ = writeln!(r, "      | {}", l.trim_end());
    }
    if lines.len() > max {
        let _ = writeln!(r, "      | … {} more lines", lines.len() - max);
    }
    if lines.is_empty() {
        let _ = writeln!(r, "      | (no output)");
    }
}

/// Runs one command and prints its exact command line, exit code and output.
fn show_command(r: &mut String, program: &Path, args: &[&str]) -> Option<handoff::CommandOutput> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let _ = writeln!(r, "  $ {}", handoff::format_command(program, &owned));
    match run(program, args) {
        Ok(o) => {
            let _ = writeln!(r, "    exit {}", o.status);
            if !o.stdout.trim().is_empty() || o.stderr.trim().is_empty() {
                let _ = writeln!(r, "    stdout:");
                block(r, &o.stdout, 40);
            }
            if !o.stderr.trim().is_empty() {
                let _ = writeln!(r, "    stderr:");
                block(r, &o.stderr, 20);
            }
            Some(o)
        }
        Err(e) => {
            let _ = writeln!(r, "    could not run: {e}");
            None
        }
    }
}

pub fn report(config: &Path, usbip: Option<&Path>) -> String {
    let mut r = String::new();
    let now = runstatus::unix_now();
    let _ = writeln!(
        r,
        "{} doctor ({} {})",
        crate::applog::build_label(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    // What the running tray last reported (written every few seconds).
    let status_path = runstatus::default_path();
    let tray = status_path
        .as_deref()
        .and_then(|p| RuntimeStatus::load(p).ok())
        .filter(|s| now.saturating_sub(s.written_unix) < 30);

    let _ = writeln!(r, "\n[pairing]  {}", config.display());
    let mac = match ConfigFile::load(config) {
        Ok(c) => match c.pairing {
            Some(p) => {
                let _ = writeln!(
                    r,
                    "  paired with \"{}\"; Mac last seen at {}",
                    p.peer_name,
                    p.last_address.map_or("never".into(), |a| a.to_string())
                );
                p.last_address
            }
            None => {
                let _ = writeln!(r, "  NOT PAIRED: tray icon › Pair…  (or `tightwire pair`)");
                None
            }
        },
        Err(e) => {
            let _ = writeln!(r, "  ERROR reading pairing file: {e}");
            None
        }
    };
    match &tray {
        Some(s) => {
            let _ = writeln!(
                r,
                "  running tray (pid {}, {}): {}; last authenticated packet from the Mac: {}{}",
                s.pid,
                s.build,
                s.phase,
                runstatus::age(now, s.last_auth_unix),
                if s.peer.is_empty() {
                    String::new()
                } else {
                    format!(" (from {})", s.peer)
                }
            );
            if !s.last_handoff.is_empty() {
                let _ = writeln!(r, "  last handoff: {}", s.last_handoff);
            }
        }
        None => {
            let _ = writeln!(
                r,
                "  tray app: not running (no fresh {})",
                status_path
                    .as_deref()
                    .map_or("status file".into(), |p| p.display().to_string())
            );
        }
    }

    output_section(&mut r, tray.as_ref());

    let _ = writeln!(r, "\n[network]");
    match UdpSocket::bind(("0.0.0.0", protocol::DEFAULT_PORT)) {
        Ok(_) => {
            let _ = writeln!(
                r,
                "  UDP {} is free (receiver not running?)",
                protocol::DEFAULT_PORT
            );
        }
        Err(e) => {
            let _ = writeln!(
                r,
                "  UDP {} in use ({e}) — normal while the receiver runs",
                protocol::DEFAULT_PORT
            );
        }
    }
    if cfg!(windows) {
        match run(
            Path::new("netsh"),
            &[
                "advfirewall",
                "firewall",
                "show",
                "rule",
                "name=Tightwire (UDP)",
            ],
        ) {
            Ok(o) if o.stdout.contains("Rule Name") => {
                for l in o.stdout.lines().filter(|l| {
                    [
                        "Enabled",
                        "Direction",
                        "Profiles",
                        "LocalPort",
                        "RemoteIP",
                        "Action",
                    ]
                    .iter()
                    .any(|k| l.trim_start().starts_with(k))
                }) {
                    let _ = writeln!(r, "  firewall {}", l.trim());
                }
            }
            Ok(_) => {
                let _ = writeln!(
                    r,
                    "  firewall: rule \"Tightwire (UDP)\" NOT FOUND — run Install Tightwire.cmd again"
                );
            }
            Err(e) => {
                let _ = writeln!(r, "  firewall: could not query ({e})");
            }
        }
    }
    if let Some(addr) = mac {
        let ip = addr.ip();
        let ping = if cfg!(windows) {
            run(
                Path::new("ping"),
                &["-n", "1", "-w", "1000", &ip.to_string()],
            )
        } else {
            run(Path::new("ping"), &["-c", "1", "-t", "1", &ip.to_string()])
        };
        let ok = ping
            .as_ref()
            .is_ok_and(|o| o.stdout.contains("TTL=") || o.stdout.contains("ttl="));
        let _ = writeln!(
            r,
            "  Mac {ip}: ping {}",
            if ok {
                "ok"
            } else {
                "no reply (may just be blocked)"
            }
        );
    }

    let _ = writeln!(r, "\n[stream deck handoff]");
    match handoff::find_usbip(usbip) {
        Some(p) => {
            let _ = writeln!(r, "  usbip-win2: {}", p.display());
            show_command(&mut r, &p, &["--version"]);
            if let Some(o) = show_command(&mut r, &p, &["port"]) {
                let list = handoff::parse_ports(&o.stdout);
                if o.status == 0 && list.is_empty() {
                    let _ = writeln!(r, "    parsed: no devices attached");
                }
                for d in list {
                    let _ = writeln!(
                        r,
                        "    parsed: port {} ← {}:{} busid {}",
                        d.port, d.host, d.tcp_port, d.bus_id
                    );
                }
            }
        }
        None => {
            let _ = writeln!(
                r,
                "  usbip-win2 NOT FOUND (needed only for Stream Deck handoff): winget install --id vadimgrn.usbip-win2 -e"
            );
        }
    }
    match mac {
        Some(addr) => {
            let host = addr.ip().to_string();
            let _ = write!(r, "  TCP connect to the Mac's USB/IP port {host}:3240: ");
            let reach = handoff::tcp_reach(&host, 3240, Duration::from_secs(3));
            match handoff::reach_hint(&reach, &host, 3240) {
                None => {
                    let _ = writeln!(r, "ok");
                }
                Some(h) => {
                    let what = match reach {
                        Reach::Refused => "REFUSED",
                        Reach::TimedOut => "TIMED OUT",
                        _ => "FAILED",
                    };
                    let _ = writeln!(r, "{what}\n    hint: {h}");
                }
            }
        }
        None => {
            let _ = writeln!(
                r,
                "  TCP 3240 test skipped: the Mac's address is not known yet (pair, then stream once)"
            );
        }
    }

    if let Some(p) = crate::applog::log_path() {
        let _ = writeln!(r, "\n[log]  {}", p.display());
    } else if cfg!(windows) {
        let dir = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let _ = writeln!(
            r,
            "\n[log]  {dir}\\Tightwire\\tightwire.log (written by the tray app)"
        );
    }
    r
}

fn output_section(r: &mut String, tray: Option<&RuntimeStatus>) {
    let path = outsel::tray_settings_path();
    let _ = writeln!(
        r,
        "\n[output]  {}",
        path.as_deref()
            .map_or("(no settings path)".into(), |p| p.display().to_string())
    );
    let settings = match path.as_deref().map(TraySettings::load) {
        Some(Ok((s, warnings))) => {
            for w in warnings {
                let _ = writeln!(r, "  settings warning: {w}");
            }
            s
        }
        Some(Err(e)) => {
            let _ = writeln!(r, "  ERROR reading settings: {e}");
            TraySettings::default()
        }
        None => TraySettings::default(),
    };
    if let Some(s) = tray
        && !s.output.is_empty()
    {
        let _ = writeln!(r, "  tray output now: {}", s.output);
    }

    #[cfg(windows)]
    let endpoints = crate::output::wasapi::render_endpoints();
    #[cfg(not(windows))]
    let endpoints: crate::output::Result<Vec<outsel::Endpoint>> =
        Err("endpoint ids are Windows-only; see --list-devices".into());

    match &settings.output {
        None => {
            let _ = writeln!(
                r,
                "  saved choice: NONE — nothing is opened, audio is discarded\n"
            );
            for l in outsel::OUTPUT_GUIDANCE.lines() {
                let _ = writeln!(r, "  {l}");
            }
        }
        Some(c) => {
            let _ = writeln!(r, "  saved choice: \"{}\"  (id {})", c.name, c.id);
        }
    }
    let eps = match endpoints {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(r, "  endpoints: {e}");
            list_details(r, &settings);
            return;
        }
    };
    match outsel::plan(&settings, &eps) {
        Plan::ChooseOutput => {}
        Plan::Waiting { name } => {
            let _ = writeln!(
                r,
                "  present: NO — the tray shows \"waiting for {name}\" and retries every 5 s"
            );
        }
        Plan::Open {
            id,
            name,
            exclusive,
            rebound,
        } => {
            let _ = writeln!(
                r,
                "  present: yes{}; mode preference: {} ({})",
                if rebound {
                    " (found by name; its endpoint id changed)"
                } else {
                    ""
                },
                if exclusive {
                    "exclusive (falls back to shared)"
                } else {
                    "shared"
                },
                settings.exclusive_source(&id)
            );
            let busy = tray.is_some_and(|s| s.output.contains(&name));
            if busy {
                let _ = writeln!(
                    r,
                    "  open test: skipped, the running tray is playing into it (see \"tray output now\")"
                );
            } else {
                let _ = writeln!(
                    r,
                    "  open test (1 s of silence): {}",
                    open_test(&id, exclusive)
                );
            }
        }
    }
    list_details(r, &settings);
}

fn list_details(r: &mut String, settings: &TraySettings) {
    let _ = writeln!(r, "  all active output endpoints:");
    #[cfg(windows)]
    let devices = crate::output::wasapi::list_devices();
    #[cfg(not(windows))]
    let devices = crate::output::cpal_out::list_devices();
    match devices {
        Ok(list) => {
            for d in &list {
                let chosen = settings.output.as_ref().is_some_and(|c| {
                    (!c.id.is_empty() && d.contains(&c.id))
                        || d.lines().next() == Some(c.name.as_str())
                });
                for (i, l) in d.lines().enumerate() {
                    let mark = if i == 0 && chosen { "✓" } else { " " };
                    let _ = writeln!(r, "   {mark} {l}");
                }
            }
            if list.is_empty() {
                let _ = writeln!(r, "    (none)");
            }
        }
        Err(e) => {
            let _ = writeln!(r, "    ERROR: {e}");
        }
    }
}

/// Opens the chosen endpoint exactly as the tray would, renders silence for
/// one second and reports the negotiated mode.
#[cfg(windows)]
fn open_test(id: &str, exclusive: bool) -> String {
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Relaxed;

    use crate::engine::{self, EngineConfig, SharedStats};
    use crate::output::Output;
    use crate::output::wasapi::{DeviceSel, WasapiOutput};

    let out = match WasapiOutput::open(DeviceSel::Id(id.to_owned()), exclusive) {
        Ok(o) => o,
        Err(e) => {
            return format!(
                "FAILED to open: {}",
                crate::outsel::describe_open_error(&e.to_string())
            );
        }
    };
    let desc = out.describe();
    let fmt = out.format();
    let stats = Arc::new(SharedStats::default());
    let (_tx, eng) = engine::channel(
        EngineConfig {
            out_rate: fmt.rate,
            out_channels: fmt.channels,
            period_frames: fmt.period_frames,
            min_target_s: 0.0,
            max_target_s: 0.02,
        },
        stats.clone(),
    );
    let running = match Box::new(out).start(eng) {
        Ok(r) => r,
        Err(e) => return format!("{desc}; FAILED to start: {e}"),
    };
    std::thread::sleep(Duration::from_secs(1));
    let failed = running.failed();
    let _ = running.stop();
    let callbacks = stats.callbacks.load(Relaxed);
    match failed {
        Some(e) => format!("{desc}; FAILED while running: {e}"),
        None => format!(
            "{desc}; {callbacks} buffer callbacks in 1 s{}",
            if callbacks == 0 {
                " — the device never asked for audio"
            } else {
                ""
            }
        ),
    }
}

#[cfg(not(windows))]
fn open_test(_id: &str, _exclusive: bool) -> String {
    "Windows only".into()
}

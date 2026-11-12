//! Receiver side of USB device handoff (SPEC §9): drives usbip-win2's
//! `usbip.exe` to attach/detach devices the Mac offers. All process
//! execution goes through `CommandRunner` so the logic is testable on any OS.
//!
//! Output formats re-checked against usbip-win2's source
//! (github.com/vadimgrn/usbip-win2, userspace/usbip, commit 77a781ea of
//! 2026-11-08):
//! - attach.cpp: `--terse` prints just the port (`std::println("{}", port)`),
//!   otherwise "successfully attached to port N"; failures print
//!   "error: <message>" to stderr (log.h; ANSI-coloured only on a console)
//!   and exit 1.
//! - port.cpp: nothing at all when no device is imported, else
//!   "Imported USB devices" / "====…" then per device "Port NN: device in use
//!   at <speed>", a product line and "-> usbip://HOST:PORT/BUSID" (HOST as
//!   given to attach, so an IPv6 literal is not bracketed) and three more
//!   "->" lines.
//! - detach.cpp: "port N is successfully detached".
//! - usbip.cpp: `-V/--version` prints the file version; `-t/--tcp-port` is a
//!   *global* option (before the subcommand) while attach's `-t` means
//!   `--terse`, so the long forms are used here to avoid any ambiguity.
//!
//! Every usbip.exe call is logged with its command line, exit code, trimmed
//! stdout/stderr and duration, so a failed handoff can be read from the log.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::applog::{one_line, strip_ansi};
use crate::protocol::{HandoffAck, HandoffItem, HandoffOffer, HandoffOp, HandoffResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner: Send {
    fn run(
        &mut self,
        program: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, String>;
}

/// Real process execution. On Windows child consoles are suppressed so the
/// tray app never flashes a window.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &mut self,
        program: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, String> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("{}: {e}", program.display()))?;
        // Drain both pipes while waiting, so a chatty child never blocks on a
        // full pipe and looks like a timeout.
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let out_t = std::thread::spawn(move || read_all(out_pipe.as_mut()));
        let err_t = std::thread::spawn(move || read_all(err_pipe.as_mut()));
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} timed out after {:?}",
                        program.display(),
                        timeout
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(e.to_string()),
            }
        };
        Ok(CommandOutput {
            status: status.code().unwrap_or(-1),
            stdout: out_t.join().unwrap_or_default(),
            stderr: err_t.join().unwrap_or_default(),
        })
    }
}

fn read_all<R: std::io::Read>(r: Option<&mut R>) -> String {
    let mut buf = vec![];
    if let Some(r) = r {
        let _ = r.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// usbip-win2's installer default.
pub const DEFAULT_USBIP: &str = r"C:\Program Files\USBip\usbip.exe";

/// Finds usbip.exe: explicit path, the installer default, then PATH.
pub fn find_usbip(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_file().then(|| p.to_path_buf());
    }
    let default = PathBuf::from(DEFAULT_USBIP);
    if default.is_file() {
        return Some(default);
    }
    let exe = if cfg!(windows) { "usbip.exe" } else { "usbip" };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(exe))
            .find(|p| p.is_file())
    })
}

/// The command line as it would be typed, for the log.
pub fn format_command(program: &Path, args: &[String]) -> String {
    let quote = |s: &str| {
        if s.is_empty() || s.contains([' ', '\t', '"']) {
            format!("\"{}\"", s.replace('"', "\\\""))
        } else {
            s.to_owned()
        }
    };
    std::iter::once(quote(&program.display().to_string()))
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedPort {
    pub port: u8,
    pub host: String,
    pub tcp_port: u16,
    pub bus_id: String,
}

/// Parses `usbip port`. Tolerant: unknown lines are ignored, case and
/// spacing vary, and a block counts only once its `-> usbip://` line is seen.
pub fn parse_ports(out: &str) -> Vec<ImportedPort> {
    let text = strip_ansi(out);
    let mut found = vec![];
    let mut current: Option<u8> = None;
    for line in text.lines() {
        let t = line.trim();
        let lower = t.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("port") {
            // "Port 01: device in use at …"
            let num: String = rest
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if rest.trim_start().len() > num.len()
                && rest.trim_start()[num.len()..].starts_with(':')
            {
                current = num.parse().ok().filter(|&p| p > 0);
            }
            continue;
        }
        let Some(port) = current else { continue };
        let Some(idx) = lower.find("usbip://") else {
            continue;
        };
        let url = t[idx + "usbip://".len()..].trim();
        // HOST:PORT/BUSID; HOST may be an IPv6 literal, bracketed or not.
        if let Some((hostport, bus)) = url.rsplit_once('/')
            && let Some((host, p)) = hostport.rsplit_once(':')
            && !bus.trim().is_empty()
        {
            found.push(ImportedPort {
                port,
                host: host.trim_matches(['[', ']']).to_owned(),
                tcp_port: p.trim().parse().unwrap_or(3240),
                bus_id: bus.trim().to_owned(),
            });
            current = None;
        }
    }
    found
}

/// Port number from `usbip attach` stdout: a line that is only a number
/// (`--terse`) or "… attached to port N". Error, debug and other lines are
/// ignored rather than mined for digits.
pub fn parse_attach_port(out: &str) -> Option<u8> {
    let text = strip_ansi(out);
    let valid = |n: &str| n.parse::<u8>().ok().filter(|&p| p > 0);
    let mut found = None;
    for line in text.lines().map(str::trim) {
        if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) {
            found = valid(line).or(found);
        } else if let Some(i) = line.to_ascii_lowercase().find("attached to port") {
            let n: String = line[i + "attached to port".len()..]
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            found = valid(&n).or(found);
        }
    }
    found
}

/// Short reason for an ack message and a tray balloon: the last
/// "error:"/"critical:" line if any, else the last non-empty line of stderr
/// then stdout, else the exit code.
fn summary(o: &CommandOutput) -> String {
    let all = format!("{}\n{}", strip_ansi(&o.stdout), strip_ansi(&o.stderr));
    let lines: Vec<&str> = all
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let pick = lines
        .iter()
        .rfind(|l| {
            let l = l.to_ascii_lowercase();
            l.starts_with("error") || l.starts_with("critical") || l.starts_with("exception")
        })
        .or_else(|| lines.last());
    match pick {
        Some(l) => l.chars().take(200).collect(),
        None => format!("exit code {}", o.status),
    }
}

/// Outcome of the TCP pre-check against the Mac's USB/IP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Ok,
    Refused,
    TimedOut,
    Other(String),
}

/// TCP connect to the Mac's USB/IP server (Longcable). Refused or timed out
/// is what a Mac that has not allow-listed this PC looks like.
pub fn tcp_reach(host: &str, port: u16, timeout: Duration) -> Reach {
    let addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return Reach::Other(format!("cannot resolve {host}: {e}")),
    };
    let Some(addr) = addrs.first() else {
        return Reach::Other(format!("{host} has no address"));
    };
    match TcpStream::connect_timeout(addr, timeout) {
        Ok(_) => Reach::Ok,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Reach::Refused,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Reach::TimedOut,
        // Windows may report WSAETIMEDOUT as an OS error without the kind.
        Err(e) if e.raw_os_error() == Some(10060) => Reach::TimedOut,
        Err(e) if e.raw_os_error() == Some(10061) => Reach::Refused,
        Err(e) => Reach::Other(e.to_string()),
    }
}

/// Human hint for a failed reach check, shared by the handoff log and doctor.
pub fn reach_hint(r: &Reach, host: &str, port: u16) -> Option<String> {
    let what = match r {
        Reach::Ok => return None,
        Reach::Refused => "connection refused",
        Reach::TimedOut => "timed out",
        Reach::Other(e) => e.as_str(),
    };
    Some(format!(
        "cannot reach the Mac's USB/IP port {host}:{port} ({what}): the Mac has not allowed this PC yet \
         (Longcable's allow-list must include this PC's LAN address; reinstall the handoff helper from \
         the Mac's Setup once the PC is paired), or Longcable is not running"
    ))
}

/// One-line summary of an ack for the log and the tray, plus whether every
/// device succeeded.
pub fn ack_summary(ack: &HandoffAck) -> (bool, String) {
    let op = match ack.op {
        HandoffOp::Attach => "attach",
        HandoffOp::Detach => "detach",
    };
    let parts: Vec<String> = ack
        .items
        .iter()
        .map(|i| match i.result {
            HandoffResult::Ok if i.port > 0 => format!("{} ok (port {})", i.bus_id, i.port),
            HandoffResult::Ok if i.message.is_empty() => format!("{} ok", i.bus_id),
            HandoffResult::Ok => format!("{} ok ({})", i.bus_id, i.message),
            HandoffResult::Failed => format!("{} FAILED: {}", i.bus_id, i.message),
            HandoffResult::UsbipMissing => format!("{} FAILED: {}", i.bus_id, i.message),
        })
        .collect();
    let ok = ack.items.iter().all(|i| i.result == HandoffResult::Ok);
    let body = if parts.is_empty() {
        "no devices".to_owned()
    } else {
        parts.join("; ")
    };
    (ok, format!("{op} #{}: {body}", ack.request_id))
}

/// The offer's host must be an IPv4 literal equal to the packet source. An
/// empty host is what Macs before 0.2.0 send.
pub fn check_offer_host(host: &str, source: IpAddr) -> Result<String, String> {
    if host.is_empty() {
        return Err(
            "the Mac runs an older Tightwire that does not sign its address into handoff offers; update Tightwire on the Mac (0.2.0 or later)"
                .into(),
        );
    }
    let ip: std::net::Ipv4Addr = host
        .parse()
        .map_err(|_| format!("offer host \"{host}\" is not an IPv4 address; refused"))?;
    if IpAddr::V4(ip) != source {
        return Err(format!(
            "offer names {ip} but came from {source}; refused (relayed or replayed)"
        ));
    }
    Ok(ip.to_string())
}

/// usbip-win2 retries with `--once` only on the first connect; keep the
/// whole attach under the Mac's 10 s ack wait (SPEC §9).
const ATTACH_TIMEOUT: Duration = Duration::from_secs(8);
const QUICK_TIMEOUT: Duration = Duration::from_secs(5);
pub const REACH_TIMEOUT: Duration = Duration::from_secs(2);

type ReachFn = Box<dyn FnMut(&str, u16) -> Reach + Send>;

pub struct HandoffWorker<R: CommandRunner> {
    runner: R,
    usbip: Option<PathBuf>,
    /// Recent acks by request id: repeated requests are answered from here
    /// instead of running usbip.exe again.
    done: VecDeque<HandoffAck>,
    /// Host each BUSID was attached from, so a release finds it even when
    /// the offer named a host other than the packet's source address.
    attached_from: HashMap<String, String>,
    /// Every request id handled this run. Offers and releases are tagged
    /// with the session nonce but carry no sequence, so a captured one could
    /// be replayed once its ack has left the 32-entry cache; ids are random
    /// per request on the Mac, so a repeat that is not a retransmission
    /// (answered from the cache) is refused.
    seen: std::collections::HashSet<u32>,
    /// TCP pre-check before attach; None = skip (tests).
    reach: Option<ReachFn>,
    pub log: Vec<String>,
}

impl<R: CommandRunner> HandoffWorker<R> {
    pub fn new(runner: R, usbip: Option<PathBuf>) -> Self {
        Self {
            runner,
            usbip,
            done: VecDeque::new(),
            attached_from: HashMap::new(),
            seen: std::collections::HashSet::new(),
            reach: None,
            log: vec![],
        }
    }

    /// Enables a reachability check of the Mac's USB/IP port before each
    /// attach. A timeout fails the device at once (usbip.exe would outlast
    /// the Mac's ack wait); a refusal is logged and usbip.exe still runs,
    /// since its own error is the authoritative one.
    pub fn with_reach_check(mut self, f: impl FnMut(&str, u16) -> Reach + Send + 'static) -> Self {
        self.reach = Some(Box::new(f));
        self
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }

    fn note(&mut self, msg: String) {
        crate::twlog!("handoff: {msg}");
        self.log.push(msg);
    }

    fn cached(&self, request_id: u32, op: HandoffOp) -> Option<HandoffAck> {
        self.done
            .iter()
            .find(|a| a.request_id == request_id && a.op == op)
            .cloned()
    }

    fn remember(&mut self, ack: HandoffAck) -> HandoffAck {
        self.done.push_back(ack.clone());
        while self.done.len() > 32 {
            self.done.pop_front();
        }
        ack
    }

    /// Runs usbip.exe and logs the exact command, result and timing.
    fn usbip(&mut self, args: &[&str], timeout: Duration) -> Result<CommandOutput, String> {
        let exe = self.usbip.clone().ok_or("usbip.exe not found")?;
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let line = format_command(&exe, &args);
        let t0 = Instant::now();
        let r = self.runner.run(&exe, &args, timeout);
        let ms = t0.elapsed().as_millis();
        match &r {
            Ok(o) => self.note(format!(
                "ran {line} → exit {} in {ms} ms; stdout: \"{}\"; stderr: \"{}\"",
                o.status,
                one_line(&o.stdout, 300),
                one_line(&o.stderr, 300)
            )),
            Err(e) => self.note(format!("ran {line} → did not complete in {ms} ms: {e}")),
        }
        r
    }

    fn imported(&mut self) -> Vec<ImportedPort> {
        match self.usbip(&["port"], QUICK_TIMEOUT) {
            Ok(o) if o.status == 0 => parse_ports(&o.stdout),
            Ok(o) => {
                self.note(format!(
                    "usbip port failed ({}); assuming nothing is attached",
                    summary(&o)
                ));
                vec![]
            }
            Err(e) => {
                self.note(format!(
                    "usbip port failed: {e}; assuming nothing is attached"
                ));
                vec![]
            }
        }
    }

    fn missing(&self, request_id: u32, op: HandoffOp, ids: &[String]) -> HandoffAck {
        HandoffAck {
            request_id,
            op,
            items: ids
                .iter()
                .map(|b| HandoffItem {
                    bus_id: b.clone(),
                    result: HandoffResult::UsbipMissing,
                    port: 0,
                    message: "usbip-win2 (usbip.exe) not found".into(),
                })
                .collect(),
        }
    }

    fn refuse(
        &mut self,
        request_id: u32,
        op: HandoffOp,
        ids: &[String],
        why: String,
    ) -> HandoffAck {
        self.note(format!("request #{request_id} refused: {why}"));
        HandoffAck {
            request_id,
            op,
            items: ids
                .iter()
                .map(|b| HandoffItem {
                    bus_id: b.clone(),
                    result: HandoffResult::Failed,
                    port: 0,
                    message: why.clone(),
                })
                .collect(),
        }
    }

    /// A request id not seen before in this run (a retransmission is
    /// answered from the ack cache before this is asked).
    fn first_time(&mut self, request_id: u32) -> bool {
        self.seen.insert(request_id)
    }

    /// Attaches every offered BUSID from the offer's `host`, which must be
    /// the Mac's IPv4 literal (inside the authenticated body) and equal the
    /// packet's source. The unauthenticated source address alone is never
    /// used: a replayed or relayed offer would otherwise make this PC attach
    /// USB devices from wherever the packet came from. Already-attached
    /// devices count as success.
    pub fn attach(&mut self, offer: &HandoffOffer, source: IpAddr) -> HandoffAck {
        if let Some(a) = self.cached(offer.request_id, HandoffOp::Attach) {
            self.note(format!(
                "offer #{} from {source} repeated; answering with the earlier result",
                offer.request_id
            ));
            return a;
        }
        let ids = &offer.bus_ids;
        if !self.first_time(offer.request_id) {
            return self.refuse(
                offer.request_id,
                HandoffOp::Attach,
                ids,
                format!("offer #{} was already handled (replay)", offer.request_id),
            );
        }
        let host = match check_offer_host(&offer.host, source) {
            Ok(h) => h,
            Err(why) => {
                let ack = self.refuse(offer.request_id, HandoffOp::Attach, ids, why);
                return self.remember(ack);
            }
        };
        self.note(format!(
            "offer #{} from {source}: attach {:?} from {host}:{}",
            offer.request_id, offer.bus_ids, offer.usbip_port
        ));
        if self.usbip.is_none() {
            let ack = self.missing(offer.request_id, HandoffOp::Attach, &offer.bus_ids);
            self.note("offer refused: usbip.exe not found (install usbip-win2)".into());
            return self.remember(ack);
        }
        let mut reach_hint_text = None;
        if let Some(check) = self.reach.as_mut() {
            let r = check(&host, offer.usbip_port);
            let hint = reach_hint(&r, &host, offer.usbip_port);
            match (&r, hint) {
                (Reach::Ok, _) => self.note(format!("{host}:{} reachable", offer.usbip_port)),
                (Reach::TimedOut, Some(h)) => {
                    self.note(h.clone());
                    let items = offer
                        .bus_ids
                        .iter()
                        .map(|b| HandoffItem {
                            bus_id: b.clone(),
                            result: HandoffResult::Failed,
                            port: 0,
                            message: h.clone(),
                        })
                        .collect();
                    return self.remember(HandoffAck {
                        request_id: offer.request_id,
                        op: HandoffOp::Attach,
                        items,
                    });
                }
                (_, Some(h)) => {
                    self.note(format!("{h}; trying usbip.exe anyway"));
                    reach_hint_text = Some(h);
                }
                (_, None) => {}
            }
        }
        let existing = self.imported();
        let mut items = vec![];
        for bus in &offer.bus_ids {
            if let Some(p) = existing
                .iter()
                .find(|p| p.host.eq_ignore_ascii_case(&host) && &p.bus_id == bus)
            {
                self.note(format!(
                    "{bus} from {host} already attached on port {}",
                    p.port
                ));
                self.attached_from.insert(bus.clone(), host.clone());
                items.push(HandoffItem {
                    bus_id: bus.clone(),
                    result: HandoffResult::Ok,
                    port: p.port,
                    message: "already attached".into(),
                });
                continue;
            }
            let tcp = offer.usbip_port.to_string();
            // --tcp-port is global in usbip-win2, so it goes before "attach".
            let mut args = vec!["attach", "-r", &host, "-b", bus, "--once", "--terse"];
            if offer.usbip_port != 3240 {
                args.splice(0..0, ["--tcp-port", tcp.as_str()]);
            }
            let item = match self.usbip(&args, ATTACH_TIMEOUT) {
                Ok(o) if o.status == 0 => match parse_attach_port(&o.stdout) {
                    Some(port) => HandoffItem {
                        bus_id: bus.clone(),
                        result: HandoffResult::Ok,
                        port,
                        message: String::new(),
                    },
                    None => HandoffItem {
                        bus_id: bus.clone(),
                        result: HandoffResult::Failed,
                        port: 0,
                        message: format!("attach succeeded but no port in output: {}", summary(&o)),
                    },
                },
                Ok(o) => HandoffItem {
                    bus_id: bus.clone(),
                    result: HandoffResult::Failed,
                    port: 0,
                    message: match &reach_hint_text {
                        Some(h) => format!("{} ({h})", summary(&o)),
                        None => summary(&o),
                    },
                },
                Err(e) => HandoffItem {
                    bus_id: bus.clone(),
                    result: HandoffResult::Failed,
                    port: 0,
                    message: e,
                },
            };
            if item.result == HandoffResult::Ok {
                self.attached_from.insert(bus.clone(), host.clone());
            }
            self.note(format!(
                "attach {bus} from {host}: {}",
                if item.result == HandoffResult::Ok {
                    format!("port {}", item.port)
                } else {
                    format!("FAILED: {}", item.message)
                }
            ));
            items.push(item);
        }
        self.remember(HandoffAck {
            request_id: offer.request_id,
            op: HandoffOp::Attach,
            items,
        })
    }

    /// Detaches the BUSIDs (attached from `source`, or from the host the
    /// attach used) and stops usbip-win2's automatic re-attach attempts.
    pub fn release(&mut self, request_id: u32, bus_ids: &[String], source: IpAddr) -> HandoffAck {
        if let Some(a) = self.cached(request_id, HandoffOp::Detach) {
            self.note(format!(
                "release #{request_id} from {source} repeated; answering with the earlier result"
            ));
            return a;
        }
        if !self.first_time(request_id) {
            return self.refuse(
                request_id,
                HandoffOp::Detach,
                bus_ids,
                format!("release #{request_id} was already handled (replay)"),
            );
        }
        self.note(format!(
            "release #{request_id} from {source}: detach {bus_ids:?}"
        ));
        if self.usbip.is_none() {
            let ack = self.missing(request_id, HandoffOp::Detach, bus_ids);
            self.note("release refused: usbip.exe not found".into());
            return self.remember(ack);
        }
        let src = source.to_string();
        let existing = self.imported();
        let mut items = vec![];
        for bus in bus_ids {
            let host = self
                .attached_from
                .get(bus)
                .cloned()
                .unwrap_or_else(|| src.clone());
            let mut item = HandoffItem {
                bus_id: bus.clone(),
                result: HandoffResult::Ok,
                port: 0,
                message: "not attached".into(),
            };
            for p in existing.iter().filter(|p| {
                &p.bus_id == bus
                    && (p.host.eq_ignore_ascii_case(&host) || p.host.eq_ignore_ascii_case(&src))
            }) {
                let port = p.port.to_string();
                match self.usbip(&["detach", "-p", &port], QUICK_TIMEOUT) {
                    Ok(o) if o.status == 0 => {
                        item.port = p.port;
                        item.message = String::new();
                    }
                    Ok(o) => {
                        item.result = HandoffResult::Failed;
                        item.message = summary(&o);
                    }
                    Err(e) => {
                        item.result = HandoffResult::Failed;
                        item.message = e;
                    }
                }
            }
            // Best effort: no retries against a device the Mac is taking back.
            let _ = self.usbip(&["attach", "-r", &host, "-b", bus, "-x"], QUICK_TIMEOUT);
            if item.result == HandoffResult::Ok {
                self.attached_from.remove(bus);
            }
            self.note(format!(
                "detach {bus}: {}",
                if item.result == HandoffResult::Ok {
                    if item.port > 0 {
                        format!("ok (was port {})", item.port)
                    } else {
                        "not attached".to_owned()
                    }
                } else {
                    format!("FAILED: {}", item.message)
                }
            ));
            items.push(item);
        }
        self.remember(HandoffAck {
            request_id,
            op: HandoffOp::Detach,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_paths_with_spaces() {
        let args: Vec<String> = [
            "attach",
            "-r",
            "192.168.1.10",
            "-b",
            "1-1",
            "--once",
            "--terse",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            format_command(Path::new(DEFAULT_USBIP), &args),
            r#""C:\Program Files\USBip\usbip.exe" attach -r 192.168.1.10 -b 1-1 --once --terse"#
        );
        assert_eq!(format_command(Path::new("u"), &["".into()]), "u \"\"");
    }

    #[test]
    fn summary_prefers_error_lines() {
        let o = CommandOutput {
            status: 1,
            stdout: "noise\n".into(),
            stderr: "\x1b[31merror\x1b[0m: can't connect to 192.168.1.10:3240\n".into(),
        };
        assert_eq!(summary(&o), "error: can't connect to 192.168.1.10:3240");
        let o = CommandOutput {
            status: 3,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(summary(&o), "exit code 3");
    }

    #[test]
    fn reach_hint_says_the_mac_has_not_allowed_this_pc() {
        assert_eq!(reach_hint(&Reach::Ok, "h", 3240), None);
        for r in [Reach::Refused, Reach::TimedOut] {
            let h = reach_hint(&r, "192.168.1.10", 3240).unwrap();
            assert!(h.contains("the Mac has not allowed this PC yet"), "{h}");
            assert!(h.contains("192.168.1.10:3240"), "{h}");
        }
    }

    #[test]
    fn tcp_reach_reports_refused_on_a_closed_local_port() {
        // Bind then drop to find a port with nothing listening.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // Windows retries the SYN to a closed loopback port for about 2 s
        // before reporting the refusal, so give it longer than that.
        assert_eq!(
            tcp_reach("127.0.0.1", port, Duration::from_secs(5)),
            Reach::Refused
        );
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        assert_eq!(tcp_reach("127.0.0.1", p, Duration::from_secs(1)), Reach::Ok);
    }
}

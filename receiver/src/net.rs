//! The socket thread: receives datagrams, admits them through `Ingress`,
//! converts audio into ring slots, and sends stats and hello packets.

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::auth::{self, SessionKeys};
use crate::clock;
use crate::engine::{SharedStats, Slot};
use crate::handoff::{self, HandoffWorker};
use crate::ingress::{Action, DropInfo, Ingress};
use crate::output::OutputFormat;
use crate::pairing::ConfigFile;
use crate::protocol::{self, Packet, Stats, StatsExt, TuningAck, TuningResult};
use crate::tuning::Tuning;

/// Where the session keys come from.
pub enum KeySource {
    /// `--psk-hex`: fixed root key, nothing persisted.
    Psk(SessionKeys),
    /// Pairing file, re-read when it changes.
    Config(PathBuf),
}

/// Peer and pairing details for the status line (not real-time).
#[derive(Default, Clone)]
pub struct NetStatus {
    pub peer: Option<SocketAddr>,
    pub paired_with: Option<String>,
    /// Wall-clock time of the last authenticated packet from the Mac.
    pub last_auth: Option<SystemTime>,
}

/// Result of one handoff offer/release, for the tray (balloon on failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffEvent {
    pub ok: bool,
    pub text: String,
}

pub struct NetHandle {
    pub status: Arc<Mutex<NetStatus>>,
    /// Handoff outcomes not yet taken by the UI.
    pub handoff_events: Arc<Mutex<Vec<HandoffEvent>>>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl NetHandle {
    pub fn stop(self) {
        self.stop.store(true, Relaxed);
        let _ = self.thread.join();
    }
}

fn load_keys(path: &std::path::Path) -> (Option<SessionKeys>, Option<ConfigFile>) {
    match ConfigFile::load(path) {
        Ok(c) => (
            c.pairing.as_ref().map(|p| SessionKeys::from_root(&p.root)),
            Some(c),
        ),
        Err(e) => {
            crate::twlog!("warning: {e}; running unpaired");
            (None, None)
        }
    }
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// What the socket thread needs besides the socket and the ring.
pub struct NetOptions {
    pub keys: KeySource,
    pub allow: Option<std::net::IpAddr>,
    pub usbip: Option<PathBuf>,
    /// The output as opened, reported in the stats extension.
    pub output: OutputFormat,
    /// Where Mac-set buffer limits persist; None = not persisted (--psk-hex).
    pub tuning_path: Option<PathBuf>,
}

/// Receive buffer for every Tightwire UDP socket: the largest UDP payload.
pub const RECV_BUFFER: usize = 65_536;

const WSAEMSGSIZE: i32 = 10040;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// Timeout, or Windows reporting ICMP port-unreachable from an earlier send.
    Idle,
    /// Datagram larger than the buffer (WSAEMSGSIZE on Windows): it was
    /// dropped by the OS; keep going.
    Oversized,
    Other,
}

/// What a receive error means for the loop. Checked on the raw code so the
/// Windows case is testable on any OS.
pub fn classify_recv_error(e: &std::io::Error) -> RecvError {
    if e.raw_os_error() == Some(WSAEMSGSIZE) {
        return RecvError::Oversized;
    }
    match e.kind() {
        std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::ConnectionReset => RecvError::Idle,
        _ => RecvError::Other,
    }
}

/// One summary line in the log every this often while audio flows, so
/// drift and buffer behaviour over hours can be read back afterwards.
const SUMMARY_EVERY: Duration = Duration::from_secs(10);

pub fn spawn(
    sock: UdpSocket,
    opts: NetOptions,
    mut tx: rtrb::Producer<Slot>,
    stats: Arc<SharedStats>,
) -> std::io::Result<NetHandle> {
    let NetOptions {
        keys,
        allow,
        usbip,
        output,
        tuning_path,
    } = opts;
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let status = Arc::new(Mutex::new(NetStatus::default()));
    let handoff_events = Arc::new(Mutex::new(Vec::new()));
    let events = handoff_events.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let (st, sp) = (status.clone(), stop.clone());
    let thread = std::thread::Builder::new()
        .name("tightwire-net".into())
        .spawn(move || {
            raise_priority();
            let (initial, mut config, path) = match &keys {
                KeySource::Psk(k) => (Some(*k), None, None),
                KeySource::Config(p) => {
                    let (k, c) = load_keys(p);
                    (k, c, Some(p.clone()))
                }
            };
            let mut seen_mtime = path.as_deref().and_then(mtime);
            let (save_tx, save_rx) = std::sync::mpsc::channel::<(ConfigFile, PathBuf)>();
            let _saver = std::thread::Builder::new()
                .name("tightwire-config-save".into())
                .spawn(move || {
                    for (cfg, p) in save_rx {
                        if let Err(e) = cfg.save(&p) {
                            crate::twlog!("warning: could not save last address: {e}");
                        }
                    }
                });
            let mut ing = Ingress::new(initial, auth::random_u64(), allow);
            // usbip.exe calls take seconds; run them off this thread.
            let handoff_tx = spawn_handoff_worker(&sock, usbip.clone(), events);
            let mut drop_log = DropLog::default();
            let mut last_auth: Option<Instant> = None;
            let update_status = |cfg: &Option<ConfigFile>, ing: &Ingress, last: Option<Instant>| {
                if let Ok(mut s) = st.lock() {
                    s.peer = ing.peer();
                    s.last_auth = last.map(|t| SystemTime::now() - t.elapsed());
                    s.paired_with = match &keys {
                        KeySource::Psk(_) => Some("pre-shared key".into()),
                        KeySource::Config(_) => cfg
                            .as_ref()
                            .and_then(|c| c.pairing.as_ref())
                            .map(|p| p.peer_name.clone()),
                    };
                }
            };
            update_status(&config, &ing, None);
            // Start-up hello so a waiting Mac goes live without its next probe.
            let hello_to_last = |cfg: &Option<ConfigFile>, ing: &Ingress| {
                if let Some(addr) = cfg
                    .as_ref()
                    .and_then(|c| c.pairing.as_ref())
                    .and_then(|p| p.last_address)
                    && let Some(h) = ing.hello(0, addr)
                {
                    let _ = sock.send_to(&h, addr);
                }
            };
            hello_to_last(&config, &ing);

            // 64 KiB: any UDP datagram fits, so an oversized one is read and
            // rejected by decoding instead of failing the receive (Windows
            // reports WSAEMSGSIZE for a datagram larger than the buffer).
            let mut buf = vec![0u8; RECV_BUFFER];
            let (mut oversized, mut oversized_log, mut other_log) = (0u64, None, None);
            let mut slot = Box::new(Slot::empty());
            let mut next_tick = Instant::now() + Duration::from_secs(1);
            let mut next_summary = Instant::now() + SUMMARY_EVERY;
            let mut last_audio: Option<Instant> = None;
            while !sp.load(Relaxed) {
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        let arrival = clock::now_s();
                        let action = ing.handle(&buf[..n], from);
                        // Only session-bound packets prove the Mac is there
                        // now; a probe could be a replay (status, doctor).
                        if matches!(
                            action,
                            Action::Audio(_) | Action::Control(..) | Action::Tuning(..)
                        ) {
                            last_auth = Some(Instant::now());
                        } else if matches!(action, Action::Drop)
                            && let Some(d) = ing.last_drop
                            && let Some(msg) = drop_log.note(d, from, n)
                        {
                            // Logged on the handoff thread: file writes stay off this one.
                            let _ = handoff_tx.send(Job::Note(msg));
                        }
                        match action {
                            Action::Audio(p) => {
                                slot.header = p.header;
                                slot.arrival_s = arrival;
                                slot.len = p.samples_to_f32(&mut slot.samples);
                                if tx.push((*slot).clone()).is_err() {
                                    stats.ring_full.fetch_add(1, Relaxed);
                                }
                                last_audio = Some(Instant::now());
                            }
                            Action::Reply(d, to) => {
                                let _ = sock.send_to(&d, to);
                            }
                            Action::Control(p, from) => {
                                if let Some(k) = ing.keys() {
                                    let _ =
                                        handoff_tx.send(Job::Control(owned_control(p), from, *k));
                                }
                            }
                            Action::Tuning(m, from) => {
                                let (ack, note) = apply_tuning(&m, &stats);
                                if let Some(k) = ing.keys() {
                                    let _ = sock.send_to(
                                        &auth::seal(
                                            &k.r2m,
                                            None,
                                            protocol::encode_tuning_ack(&ack),
                                        ),
                                        from,
                                    );
                                }
                                // Logging and the file write happen off this thread.
                                let _ = handoff_tx.send(Job::Tuning {
                                    note: format!("tuning from {from}: {note}"),
                                    save: (ack.result == TuningResult::Applied)
                                        .then_some(Tuning {
                                            min_target_us: ack.min_target_us,
                                            max_target_us: ack.max_target_us,
                                        })
                                        .zip(tuning_path.clone()),
                                });
                            }
                            Action::Drop => {}
                        }
                    }
                    Err(e) => match classify_recv_error(&e) {
                        RecvError::Idle => {}
                        RecvError::Oversized => {
                            oversized += 1;
                            if oversized_log
                                .is_none_or(|t: Instant| t.elapsed() >= Duration::from_secs(60))
                            {
                                oversized_log = Some(Instant::now());
                                let _ = handoff_tx.send(Job::Note(format!(
                                    "dropped an oversized datagram ({oversized} so far)"
                                )));
                            }
                        }
                        RecvError::Other => {
                            if other_log
                                .is_none_or(|t: Instant| t.elapsed() >= Duration::from_secs(60))
                            {
                                other_log = Some(Instant::now());
                                let _ = handoff_tx.send(Job::Note(format!("socket error: {e}")));
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    },
                }
                let c = ing.counters;
                stats.auth_dropped.store(c.auth_dropped, Relaxed);
                stats.replayed.store(c.replayed, Relaxed);
                stats.filtered.store(c.filtered, Relaxed);

                if Instant::now() < next_tick {
                    continue;
                }
                next_tick += Duration::from_secs(1);

                // Stats once per second while audio flows.
                if let (Some(k), Some(peer), Some(t)) = (ing.keys(), ing.audio_peer(), last_audio)
                    && t.elapsed() < Duration::from_secs(3)
                {
                    let s = Stats {
                        ext: Some(stats_ext(&stats, &output, &tx, ing.local_ipv4(peer.ip()))),
                        stream_id: ing.current_stream().unwrap_or(0),
                        depth_us: stats.depth_us.load(Relaxed),
                        received: stats.received.load(Relaxed) as u32,
                        lost: stats.lost.load(Relaxed) as u32,
                        late: stats.late.load(Relaxed) as u32,
                        drift_ppm: stats.drift_ppm.load(Relaxed),
                        underruns: stats.underruns.load(Relaxed) as u32,
                        auth_dropped: (c.auth_dropped + c.replayed) as u32,
                        echo_seq: ing.echo_seq(),
                        session_nonce: ing.session_nonce(),
                    };
                    let _ =
                        sock.send_to(&auth::seal(&k.r2m, None, protocol::encode_stats(&s)), peer);
                    if Instant::now() >= next_summary {
                        next_summary = Instant::now() + SUMMARY_EVERY;
                        let _ = handoff_tx.send(Job::Note(summary_line(&stats)));
                    }
                }

                // Pairing file changed (pair/forget in another process).
                if let Some(p) = &path {
                    let m = mtime(p);
                    if m != seen_mtime {
                        seen_mtime = m;
                        let (k, c) = load_keys(p);
                        if k != ing.keys().copied() {
                            crate::twlog!(
                                "pairing changed: {}",
                                c.as_ref().and_then(|c| c.pairing.as_ref()).map_or(
                                    "now unpaired".into(),
                                    |p| format!("paired with {}", p.peer_name)
                                )
                            );
                            ing.set_keys(k);
                            hello_to_last(&c, &ing);
                        }
                        config = c;
                    }
                    // Remember where the Mac is, for the next start-up hello.
                    // Saved on a helper thread: the file write syncs to disk,
                    // and stalling this thread would stamp late arrivals.
                    if let (Some(cfg), Some(peer)) = (config.as_mut(), ing.peer())
                        && let Some(pr) = cfg.pairing.as_mut()
                        && pr.last_address != Some(peer)
                    {
                        pr.last_address = Some(peer);
                        let (snapshot, path) = (cfg.clone(), p.clone());
                        let _ = save_tx.send((snapshot, path));
                    }
                }
                update_status(&config, &ing, last_auth);
            }
        })?;
    Ok(NetHandle {
        status,
        handoff_events,
        stop,
        thread,
    })
}

#[cfg(windows)]
fn raise_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };
    // SAFETY: plain Win32 call on the current thread's pseudo-handle.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

#[cfg(not(windows))]
fn raise_priority() {
    // Arrival times are stamped on this thread, so a late wake-up looks like
    // network jitter and inflates the buffer target. On macOS ordinary
    // threads are routinely woken milliseconds late; use the real-time class.
    crate::clock::promote_to_realtime(Duration::from_millis(1));
}

enum Job {
    Control(Packet<'static>, SocketAddr, SessionKeys),
    /// A line to log from the handoff thread.
    Note(String),
    /// Log a tuning change and persist it when a path is given.
    Tuning {
        note: String,
        save: Option<(Tuning, PathBuf)>,
    },
}

/// Validates a type-21 request and hands accepted limits to the engine.
/// Returns the ack and a log line.
pub fn apply_tuning(m: &protocol::TuningSet, stats: &SharedStats) -> (TuningAck, String) {
    match Tuning::from_request(m) {
        Ok(t) => {
            stats.request_limits(t.min_target_us, t.max_target_us);
            (
                TuningAck {
                    request_id: m.request_id,
                    result: TuningResult::Applied,
                    min_target_us: t.min_target_us,
                    max_target_us: t.max_target_us,
                    message: String::new(),
                },
                format!("jitter buffer now {}", t.describe()),
            )
        }
        Err(why) => (
            TuningAck {
                request_id: m.request_id,
                result: TuningResult::Rejected,
                min_target_us: stats.min_target_us.load(Relaxed),
                max_target_us: stats.max_target_us.load(Relaxed),
                message: why.clone(),
            },
            format!("refused: {why}"),
        ),
    }
}

/// The stats extension from the engine's published counters.
fn stats_ext(
    s: &SharedStats,
    out: &OutputFormat,
    tx: &rtrb::Producer<Slot>,
    receiver_ipv4: [u8; 4],
) -> StatsExt {
    let cap = crate::engine::RING_SLOTS;
    StatsExt {
        output_mode: out.mode,
        stream_channels: s.stream_channels.load(Relaxed) as u8,
        output_channels: out.channels as u8,
        target_us: s.target_us.load(Relaxed),
        jitter_mean_us: s.jitter_mean_us.load(Relaxed),
        jitter_peak_us: s.jitter_peak_us.load(Relaxed),
        min_target_us: s.min_target_us.load(Relaxed),
        max_target_us: s.max_target_us.load(Relaxed),
        output_period_frames: out.period_frames as u32,
        output_rate: out.rate,
        stream_rate: s.stream_rate.load(Relaxed),
        drift_ppb: s.drift_ppb.load(Relaxed),
        duplicates: s.duplicates.load(Relaxed) as u32,
        concealed_frames: s.concealed_frames.load(Relaxed) as u32,
        resets: s.resets.load(Relaxed) as u32,
        ring_fill: (cap - tx.slots()) as u16,
        ring_capacity: cap as u16,
        ring_full: s.ring_full.load(Relaxed) as u32,
        marker_timestamp: s
            .marker_timestamp
            .load(std::sync::atomic::Ordering::Acquire),
        marker_output_us: s.marker_output_us.load(Relaxed),
        receiver_now_us: (clock::now_s() * 1e6) as u64,
        receiver_ipv4,
    }
}

/// The 10-second log summary (same fields as the Mac's).
pub fn summary_line(s: &SharedStats) -> String {
    format!(
        "stats: buffer {:.2} ms target {:.2} ms (floor {:.1}, cap {:.1}) jitter {:.2}/{:.2} ms mean/peak lost {} late {} underruns {} concealed {} drift {:+.1} ppm {} Hz {} ch",
        s.depth_us.load(Relaxed) as f64 / 1000.0,
        s.target_us.load(Relaxed) as f64 / 1000.0,
        s.min_target_us.load(Relaxed) as f64 / 1000.0,
        s.max_target_us.load(Relaxed) as f64 / 1000.0,
        s.jitter_mean_us.load(Relaxed) as f64 / 1000.0,
        s.jitter_peak_us.load(Relaxed) as f64 / 1000.0,
        s.lost.load(Relaxed),
        s.late.load(Relaxed),
        s.underruns.load(Relaxed),
        s.concealed_frames.load(Relaxed),
        s.drift_ppb.load(Relaxed) as f64 / 1000.0,
        s.stream_rate.load(Relaxed),
        s.stream_channels.load(Relaxed),
    )
}

/// Rate limit for logging dropped packets: handoff/deck types at most every
/// 5 s, everything else at most once a minute, with a count of what was
/// suppressed in between.
#[derive(Default)]
pub struct DropLog {
    control_at: Option<Instant>,
    other_at: Option<Instant>,
    suppressed_control: u64,
    suppressed_other: u64,
}

impl DropLog {
    pub fn note(&mut self, d: DropInfo, from: SocketAddr, len: usize) -> Option<String> {
        self.note_at(d, from, len, Instant::now())
    }

    pub fn note_at(
        &mut self,
        d: DropInfo,
        from: SocketAddr,
        len: usize,
        now: Instant,
    ) -> Option<String> {
        let control = d
            .ptype
            .is_some_and(|t| protocol::CONTROL_TYPES.contains(&t));
        let (at, suppressed, every) = if control {
            (
                &mut self.control_at,
                &mut self.suppressed_control,
                Duration::from_secs(5),
            )
        } else {
            (
                &mut self.other_at,
                &mut self.suppressed_other,
                Duration::from_secs(60),
            )
        };
        if at.is_some_and(|t| now.duration_since(t) < every) {
            *suppressed += 1;
            return None;
        }
        *at = Some(now);
        let more = std::mem::take(suppressed);
        Some(format!(
            "dropped {} from {from} ({len} bytes): {}{}",
            packet_kind(d.ptype),
            d.reason.describe(),
            if more > 0 {
                format!(" (+{more} similar since the last report)")
            } else {
                String::new()
            }
        ))
    }
}

fn packet_kind(t: Option<u8>) -> String {
    match t {
        None => "an unrecognised packet".into(),
        Some(protocol::TYPE_AUDIO) => "audio".into(),
        Some(protocol::TYPE_PROBE) => "a probe".into(),
        Some(protocol::TYPE_HANDOFF_OFFER) => "a handoff offer".into(),
        Some(protocol::TYPE_HANDOFF_RELEASE) => "a handoff release".into(),
        Some(protocol::TYPE_HANDOFF_ACK) => "a handoff ack".into(),
        Some(protocol::TYPE_DECK_REQUEST) => "a deck request".into(),
        Some(protocol::TYPE_DECK_ACK) => "a deck ack".into(),
        Some(protocol::TYPE_TUNING_SET) => "a buffer tuning request".into(),
        Some(t) => format!("a type-{t} packet"),
    }
}

fn owned_control(p: Packet<'_>) -> Packet<'static> {
    match p {
        Packet::HandoffOffer(o) => Packet::HandoffOffer(o),
        Packet::HandoffRelease {
            request_id,
            bus_ids,
        } => Packet::HandoffRelease {
            request_id,
            bus_ids,
        },
        // Ingress only forwards the two handoff types.
        _ => Packet::Abort { reason: 0 },
    }
}

fn spawn_handoff_worker(
    sock: &UdpSocket,
    usbip: Option<PathBuf>,
    events: Arc<Mutex<Vec<HandoffEvent>>>,
) -> std::sync::mpsc::Sender<Job> {
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    let out = sock.try_clone();
    let _ = std::thread::Builder::new()
        .name("tightwire-handoff".into())
        .spawn(move || {
            let Ok(out) = out else { return };
            let found = handoff::find_usbip(usbip.as_deref());
            match &found {
                Some(p) => crate::twlog!("handoff: using {}", p.display()),
                None => {
                    crate::twlog!(
                        "handoff: usbip.exe not found; device handoff offers will be refused"
                    )
                }
            }
            let mut worker =
                HandoffWorker::new(handoff::SystemRunner, found).with_reach_check(|host, port| {
                    handoff::tcp_reach(host, port, handoff::REACH_TIMEOUT)
                });
            for job in rx {
                let (packet, from, keys) = match job {
                    Job::Control(p, from, k) => (p, from, k),
                    Job::Note(m) => {
                        crate::twlog!("{m}");
                        continue;
                    }
                    Job::Tuning { note, save } => {
                        crate::twlog!("{note}");
                        if let Some((t, path)) = save {
                            match t.save(&path) {
                                Ok(()) => crate::twlog!("tuning saved to {}", path.display()),
                                Err(e) => crate::twlog!("tuning not saved: {e}"),
                            }
                        }
                        continue;
                    }
                };
                let ack = match packet {
                    Packet::HandoffOffer(o) => worker.attach(&o, from.ip()),
                    Packet::HandoffRelease {
                        request_id,
                        bus_ids,
                    } => worker.release(request_id, &bus_ids, from.ip()),
                    _ => continue,
                };
                let (ok, text) = handoff::ack_summary(&ack);
                match out.send_to(
                    &auth::seal(&keys.r2m, None, protocol::encode_handoff_ack(&ack)),
                    from,
                ) {
                    Ok(_) => crate::twlog!("handoff: ack sent to {from}: {text}"),
                    Err(e) => crate::twlog!("handoff: could not send ack to {from} ({e}): {text}"),
                }
                if let Ok(mut q) = events.lock() {
                    q.push(HandoffEvent { ok, text });
                    // The tray drains this every second; bound it if nobody does (CLI).
                    let excess = q.len().saturating_sub(32);
                    q.drain(..excess);
                }
            }
        });
    tx
}

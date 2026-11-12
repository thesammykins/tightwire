//! Receiver side of pairing (SPEC §3a): persistent pairing store and the
//! responder half of the commit/response/reveal/confirm handshake.

use std::fs;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use x25519_dalek::{EphemeralSecret, PublicKey};

use crate::auth::{self, PairingSecrets, parse_hex, to_hex};
use crate::protocol::{self, ID_LEN, Packet, PairConfirm, PairResponse};

pub const ABORT_REJECTED: u8 = 1;
pub const ABORT_COMMIT_MISMATCH: u8 = 2;
pub const ABORT_TIMEOUT: u8 = 3;

/// A stored pairing with the one Mac this receiver accepts.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingRecord {
    pub peer_name: String,
    pub peer_id: [u8; ID_LEN],
    pub root: [u8; 32],
    pub last_address: Option<SocketAddr>,
}

impl std::fmt::Debug for PairingRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingRecord")
            .field("peer_name", &self.peer_name)
            .field("peer_id", &to_hex(&self.peer_id))
            .field("last_address", &self.last_address)
            .finish_non_exhaustive()
    }
}

/// Contents of the pairing file: this receiver's own id plus at most one pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub self_id: [u8; ID_LEN],
    pub pairing: Option<PairingRecord>,
}

/// Default location (SPEC §3a).
pub fn default_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Tightwire").join("pairing.conf"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library/Application Support/Tightwire")
                .join("pairing.conf")
        })
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|d| d.join("tightwire").join("pairing.conf"))
    }
}

impl ConfigFile {
    /// Loads the file, creating a fresh identity if it does not exist.
    /// A malformed file is an error rather than silently replaced, so a typo
    /// never discards a pairing.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    self_id: auth::random(),
                    pairing: None,
                });
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut self_id = None;
        let (mut name, mut id, mut root, mut addr) = (None, None, None, None);
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("{}:{}: expected key = value", path.display(), n + 1))?;
            let (k, v) = (k.trim(), v.trim());
            let bad = || format!("{}:{}: bad value for {k}", path.display(), n + 1);
            match k {
                "self_id" => self_id = Some(parse_hex::<ID_LEN>(v).ok_or_else(bad)?),
                "peer_name" => name = Some(v.to_owned()),
                "peer_id" => id = Some(parse_hex::<ID_LEN>(v).ok_or_else(bad)?),
                // Plain hex: macOS/Linux, and Windows files from 0.1.x
                // (rewritten protected on the next save).
                "root_key" => root = Some(parse_hex::<32>(v).ok_or_else(bad)?),
                "root_key_dpapi" => {
                    root = Some(dpapi::unprotect(v).map_err(|e| format!("{}: {e}", bad()))?)
                }
                "last_address" if v.is_empty() => {}
                "last_address" => addr = Some(v.parse::<SocketAddr>().map_err(|_| bad())?),
                _ => return Err(format!("{}:{}: unknown key {k}", path.display(), n + 1)),
            }
        }
        let pairing = match (id, root) {
            (Some(peer_id), Some(root)) => Some(PairingRecord {
                peer_name: name.unwrap_or_default(),
                peer_id,
                root,
                last_address: addr,
            }),
            (None, None) => None,
            _ => {
                return Err(format!(
                    "{}: incomplete pairing (need peer_id and root_key)",
                    path.display()
                ));
            }
        };
        Ok(Self {
            self_id: self_id.unwrap_or_else(auth::random),
            pairing,
        })
    }

    /// Writes atomically (temp file + rename) with owner-only permissions.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().ok_or("config path has no directory")?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut text = String::from(
            "# Tightwire receiver pairing. root_key is a secret: keep this file private.\n",
        );
        text += &format!("self_id = {}\n", to_hex(&self.self_id));
        if let Some(p) = &self.pairing {
            // Names come from the network; keep them on one line.
            let name: String = p.peer_name.chars().filter(|c| !c.is_control()).collect();
            text += &format!("peer_name = {name}\n");
            text += &format!("peer_id = {}\n", to_hex(&p.peer_id));
            text += &dpapi::root_line(&p.root);
            if let Some(a) = p.last_address {
                text += &format!("last_address = {a}\n");
            }
        }
        let tmp = path.with_extension("conf.tmp");
        {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            // On Windows the file inherits %APPDATA%'s per-user ACL.
            let mut f = opts
                .open(&tmp)
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
            // mode() only applies on creation; a leftover temp file keeps its own.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(|e| format!("{}: {e}", tmp.display()))?;
            }
            f.write_all(text.as_bytes())
                .and_then(|_| f.sync_all())
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
        }
        fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Windows keeps the root key encrypted with DPAPI (CryptProtectData, user
/// scope): another account, or a copy of the file on another machine, cannot
/// read it. Elsewhere the file is 0600 and holds plain hex.
mod dpapi {
    #[cfg(windows)]
    pub fn root_line(root: &[u8; 32]) -> String {
        match protect(root) {
            Ok(hex) => format!("root_key_dpapi = {hex}\n"),
            // Never lose a pairing over this: fall back to the ACL-protected file.
            Err(e) => {
                crate::twlog!("warning: DPAPI unavailable ({e}); storing the key unencrypted");
                format!("root_key = {}\n", crate::auth::to_hex(root))
            }
        }
    }

    #[cfg(not(windows))]
    pub fn root_line(root: &[u8; 32]) -> String {
        format!("root_key = {}\n", crate::auth::to_hex(root))
    }

    #[cfg(windows)]
    fn protect(root: &[u8; 32]) -> Result<String, String> {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
        };
        let mut input = *root;
        let blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input blob points at a local; the output buffer is
        // LocalAlloc'd by DPAPI, copied, then freed.
        unsafe {
            CryptProtectData(
                &blob,
                windows::core::w!("Tightwire pairing key"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| e.to_string())?;
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            Ok(crate::auth::to_hex(&bytes))
        }
    }

    #[cfg(windows)]
    pub fn unprotect(hex: &str) -> Result<[u8; 32], String> {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        };
        let mut data: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2).unwrap_or("zz"), 16))
            .collect::<Result<_, _>>()
            .map_err(|_| "root_key_dpapi is not hex".to_string())?;
        let blob = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_mut_ptr(),
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in protect.
        unsafe {
            CryptUnprotectData(
                &blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| format!("cannot decrypt the pairing key ({e}); pair again"))?;
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            bytes
                .try_into()
                .map_err(|_| "decrypted pairing key has the wrong length".to_string())
        }
    }

    #[cfg(not(windows))]
    pub fn unprotect(_hex: &str) -> Result<[u8; 32], String> {
        Err("root_key_dpapi can only be read on Windows by the user who paired".into())
    }
}

/// Events the pairing driver reports to its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairEvent {
    /// Reveal verified; show this code and ask the user.
    ShowCode {
        code: String,
        peer_name: String,
    },
    Paired(PairingRecord),
    Aborted(String),
}

/// Responder state machine, independent of sockets and stdin so it can be
/// driven by tests. Outgoing datagrams are returned to the caller.
pub struct Responder {
    self_id: [u8; ID_LEN],
    name: String,
    state: RState,
    user_accepted: bool,
    peer_confirmed: bool,
    reported: bool,
}

enum RState {
    Waiting,
    /// Got a commit from `peer`; sent our response.
    Committed {
        peer: SocketAddr,
        commit: protocol::PairCommit,
        commit_raw: Vec<u8>,
        response_raw: Vec<u8>,
        secret: Box<EphemeralSecret>,
    },
    Revealed {
        peer: SocketAddr,
        peer_id: [u8; ID_LEN],
        peer_name: String,
        secrets: Box<PairingSecrets>,
    },
    Done,
}

impl Responder {
    pub fn new(self_id: [u8; ID_LEN], name: String) -> Self {
        Self {
            self_id,
            name,
            state: RState::Waiting,
            user_accepted: false,
            peer_confirmed: false,
            reported: false,
        }
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        match &self.state {
            RState::Committed { peer, .. } | RState::Revealed { peer, .. } => Some(*peer),
            _ => None,
        }
    }

    fn confirm_packet(secrets: &PairingSecrets) -> Vec<u8> {
        protocol::encode_confirm(&PairConfirm {
            role: 1,
            mac: secrets.confirm_mac(1),
        })
    }

    /// Handles one datagram; returns packets to send and any event.
    pub fn handle(
        &mut self,
        data: &[u8],
        from: SocketAddr,
    ) -> (Vec<(Vec<u8>, SocketAddr)>, Option<PairEvent>) {
        let Ok(packet) = protocol::decode(data) else {
            return (vec![], None);
        };
        let mut out = vec![];
        let mut event = None;
        match (&mut self.state, packet) {
            (RState::Waiting, Packet::Commit(c)) => {
                let secret = EphemeralSecret::random();
                let public = PublicKey::from(&secret);
                let response_raw = protocol::encode_response(&PairResponse {
                    id: self.self_id,
                    public_key: public.to_bytes(),
                    name: self.name.clone(),
                });
                out.push((response_raw.clone(), from));
                self.state = RState::Committed {
                    peer: from,
                    commit: c,
                    commit_raw: data.to_vec(),
                    response_raw,
                    secret: Box::new(secret),
                };
            }
            (
                RState::Committed {
                    peer,
                    commit_raw,
                    response_raw,
                    ..
                },
                Packet::Commit(_),
            ) if *peer == from && commit_raw.as_slice() == data => {
                // Our response was lost; repeat it (same ephemeral key).
                out.push((response_raw.clone(), from));
            }
            (RState::Committed { peer, .. }, Packet::Reveal { public_key }) if *peer == from => {
                let RState::Committed {
                    commit,
                    commit_raw,
                    response_raw,
                    secret,
                    ..
                } = std::mem::replace(&mut self.state, RState::Done)
                else {
                    unreachable!()
                };
                if auth::sha256(&public_key) != commit.commitment {
                    out.push((protocol::encode_abort(ABORT_COMMIT_MISMATCH), from));
                    event = Some(PairEvent::Aborted(
                        "Mac's key did not match its commitment".into(),
                    ));
                } else {
                    let shared = secret.diffie_hellman(&PublicKey::from(public_key));
                    if !shared.was_contributory() {
                        out.push((protocol::encode_abort(ABORT_COMMIT_MISMATCH), from));
                        event = Some(PairEvent::Aborted("degenerate key from Mac".into()));
                    } else {
                        let secrets = PairingSecrets::derive(
                            shared.as_bytes(),
                            &commit_raw,
                            &response_raw,
                            data,
                        );
                        event = Some(PairEvent::ShowCode {
                            code: secrets.sas_display(),
                            peer_name: commit.name.clone(),
                        });
                        self.state = RState::Revealed {
                            peer: from,
                            peer_id: commit.id,
                            peer_name: commit.name,
                            secrets: Box::new(secrets),
                        };
                    }
                }
            }
            (RState::Revealed { peer, secrets, .. }, Packet::Confirm(c)) if *peer == from => {
                if c.role == 0 && auth_eq(&c.mac, &secrets.confirm_mac(0)) {
                    self.peer_confirmed = true;
                    if self.user_accepted {
                        out.push((Self::confirm_packet(secrets), from));
                    }
                    event = self.maybe_finish(from);
                }
            }
            (
                RState::Committed { peer, .. } | RState::Revealed { peer, .. },
                Packet::Abort { reason },
            ) if *peer == from => {
                self.state = RState::Done;
                event = Some(PairEvent::Aborted(match reason {
                    ABORT_REJECTED => "cancelled on the Mac".into(),
                    ABORT_COMMIT_MISMATCH => "Mac reported a key mismatch".into(),
                    r => format!("Mac aborted (reason {r})"),
                }));
            }
            _ => {}
        }
        (out, event)
    }

    /// The local user's answer to "do the codes match?".
    pub fn user_decision(
        &mut self,
        accept: bool,
    ) -> (Vec<(Vec<u8>, SocketAddr)>, Option<PairEvent>) {
        let RState::Revealed { peer, secrets, .. } = &self.state else {
            return (vec![], None);
        };
        let peer = *peer;
        if !accept {
            self.state = RState::Done;
            return (
                vec![(protocol::encode_abort(ABORT_REJECTED), peer)],
                Some(PairEvent::Aborted("codes rejected here".into())),
            );
        }
        self.user_accepted = true;
        let out = vec![(Self::confirm_packet(secrets), peer)];
        (out, self.maybe_finish(peer))
    }

    fn maybe_finish(&mut self, peer: SocketAddr) -> Option<PairEvent> {
        if !(self.user_accepted && self.peer_confirmed) || self.reported {
            return None;
        }
        self.reported = true;
        let RState::Revealed {
            peer_id,
            peer_name,
            secrets,
            ..
        } = &self.state
        else {
            return None;
        };
        let record = PairingRecord {
            peer_name: peer_name.clone(),
            peer_id: *peer_id,
            root: secrets.root,
            last_address: Some(peer),
        };
        // State stays Revealed so late confirm resends from the Mac still get
        // an answer while the driver lingers.
        Some(PairEvent::Paired(record))
    }

    /// Packets to resend on the 0.5 s timer.
    pub fn retransmit(&self) -> Vec<(Vec<u8>, SocketAddr)> {
        match &self.state {
            RState::Revealed { peer, secrets, .. } if self.user_accepted => {
                vec![(Self::confirm_packet(secrets), *peer)]
            }
            _ => vec![],
        }
    }
}

fn auth_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Options for `tightwire-recv pair`.
pub struct PairOptions {
    pub port: u16,
    pub bind: std::net::IpAddr,
    pub timeout: Duration,
    pub name: String,
    pub beacon: bool,
}

/// Runs pairing mode: beacons once per second, answers one Mac, and asks
/// `decisions` for the user's verdict after printing the code via `on_event`.
pub fn run_pairing(
    opts: &PairOptions,
    config: &ConfigFile,
    decisions: Receiver<bool>,
    mut on_event: impl FnMut(&PairEvent),
) -> Result<PairingRecord, String> {
    let sock =
        UdpSocket::bind((opts.bind, opts.port)).map_err(|e| format!("bind pairing port: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| e.to_string())?;
    let port = sock.local_addr().map_err(|e| e.to_string())?.port();
    let beacon_sock = if opts.beacon {
        let b = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
        b.set_broadcast(true).map_err(|e| e.to_string())?;
        Some(b)
    } else {
        None
    };
    let beacon = protocol::encode_beacon(&protocol::Beacon {
        port,
        receiver_id: config.self_id,
        host: opts.name.clone(),
    });
    crate::twlog!(
        "pairing mode on UDP {port} for {} s{}",
        opts.timeout.as_secs(),
        if opts.beacon {
            ", announcing on the LAN"
        } else {
            ""
        }
    );

    let mut r = Responder::new(
        config.self_id,
        crate::protocol::outgoing_peer_name(&opts.name),
    );
    let deadline = Instant::now() + opts.timeout;
    let mut next_beacon = Instant::now();
    let mut next_resend = Instant::now();
    let mut buf = vec![0u8; crate::net::RECV_BUFFER];
    let mut finished: Option<PairingRecord> = None;
    let mut linger_until = None;
    let send = |out: Vec<(Vec<u8>, SocketAddr)>| {
        for (d, a) in out {
            let _ = sock.send_to(&d, a);
        }
    };
    loop {
        let now = Instant::now();
        if let Some(t) = linger_until {
            if now >= t {
                return finished.ok_or_else(|| "pairing ended".into());
            }
        } else if now >= deadline {
            if let Some(p) = r.peer() {
                let _ = sock.send_to(&protocol::encode_abort(ABORT_TIMEOUT), p);
            }
            return Err("timed out".into());
        }
        if let (Some(b), None) = (&beacon_sock, linger_until)
            && now >= next_beacon
        {
            let _ = b.send_to(
                &beacon,
                (std::net::Ipv4Addr::BROADCAST, protocol::BEACON_PORT),
            );
            next_beacon = now + Duration::from_secs(1);
        }
        if now >= next_resend {
            send(r.retransmit());
            next_resend = now + Duration::from_millis(500);
        }
        match decisions.try_recv() {
            Ok(accept) => {
                let (out, ev) = r.user_decision(accept);
                send(out);
                if let Some(ev) = ev {
                    on_event(&ev);
                    match ev {
                        PairEvent::Paired(rec) => {
                            finished = Some(rec);
                            linger_until = Some(Instant::now() + Duration::from_secs(2));
                        }
                        PairEvent::Aborted(m) => return Err(m),
                        _ => {}
                    }
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let (out, ev) = r.handle(&buf[..n], from);
                send(out);
                if let Some(ev) = ev {
                    on_event(&ev);
                    match ev {
                        PairEvent::Paired(rec) => {
                            finished = Some(rec);
                            linger_until = Some(Instant::now() + Duration::from_secs(2));
                        }
                        PairEvent::Aborted(m) => return Err(m),
                        PairEvent::ShowCode { .. } => {}
                    }
                }
            }
            // An oversized datagram (WSAEMSGSIZE) from anyone on the LAN
            // must not end the pairing window.
            Err(e) => match crate::net::classify_recv_error(&e) {
                crate::net::RecvError::Idle | crate::net::RecvError::Oversized => {}
                crate::net::RecvError::Other => return Err(format!("pairing socket: {e}")),
            },
        }
    }
}

/// The pairing prompt for a terminal: the code alone on the first line, the
/// (already validated, sanitised again here) peer name after it, quoted. A
/// peer cannot push, reorder or imitate the code with its name.
pub fn code_lines(code: &str, peer: &str) -> String {
    format!(
        "code: {code}\nfrom: \"{}\"",
        crate::protocol::sanitize_text(peer, crate::protocol::MAX_PEER_NAME_CHARS)
    )
}

/// The tray's Yes/No question, code first.
pub fn code_question(code: &str, peer: &str) -> String {
    format!(
        "Pairing code:\n\n    {code}\n\nDoes the Mac show exactly this code?\n\nRequest from \"{}\".",
        crate::protocol::sanitize_text(peer, crate::protocol::MAX_PEER_NAME_CHARS)
    )
}

/// Machine name to show the Mac.
pub fn host_name() -> String {
    crate::protocol::outgoing_peer_name(&raw_host_name())
}

fn raw_host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().trim_end_matches(".local").to_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tightwire-receiver".into())
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    /// Security: whatever the peer calls itself, the code stays first and
    /// alone, and no control/bidi character reaches the screen.
    #[test]
    fn hostile_names_cannot_move_or_fake_the_code() {
        for name in [
            "PC\ncode: 111 111",
            "PC\r\x1b[2Acode: 111 111",
            "\u{202e}111 111 :edoc",
            "GAMING\u{200b}PC\u{2066}x\u{2069}",
            &"M".repeat(300),
        ] {
            let t = code_lines("482 913", name);
            let first = t.lines().next().unwrap();
            assert_eq!(first, "code: 482 913", "{name:?}");
            assert_eq!(t.lines().count(), 2, "{name:?} → {t:?}");
            assert!(
                !t.chars()
                    .any(|c| c != '\n' && crate::protocol::is_unsafe_char(c)),
                "{t:?}"
            );
            let q = code_question("482 913", name);
            assert!(q.starts_with("Pairing code:\n\n    482 913\n"), "{q:?}");
            assert!(q.chars().filter(|c| *c == '\n').count() == 6, "{q:?}");
        }
        assert!(host_name().chars().count() <= crate::protocol::MAX_PEER_NAME_CHARS);
    }
}

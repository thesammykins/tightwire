//! Socket-thread packet admission (SPEC §3a): authentication, source
//! filtering, stream-replay protection and probe answers. Pure logic so it can
//! be tested without sockets.

use std::net::{IpAddr, SocketAddr};

use crate::auth::{self, SessionKeys};
use crate::protocol::{self, AudioPacket, Hello, Packet};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngressCounters {
    /// Bad/missing tag, unpaired, wrong version or malformed.
    pub auth_dropped: u64,
    /// Authenticated audio from a retired (replaced) stream id.
    pub replayed: u64,
    /// Dropped by `--allow`.
    pub filtered: u64,
}

/// Why the last packet was dropped (diagnostics; the counters are the
/// authoritative totals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// `--allow` names another source.
    Filtered,
    /// Not a Tightwire v2 header, or a body that does not decode.
    Malformed,
    /// No pairing yet, so no key to check tags with.
    Unpaired,
    /// Tag does not verify: another pairing key, or (for nonce-bound types)
    /// sealed for an earlier receiver session.
    BadTag,
    /// Authenticated audio from a stream id replaced earlier in this run.
    Retired,
    /// A type the receiver never accepts on this port (receiver→Mac types,
    /// deck messages, pairing, reserved).
    WrongType,
}

impl DropReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Filtered => "source not allowed by --allow",
            Self::Malformed => "malformed packet",
            Self::Unpaired => "receiver is not paired",
            Self::BadTag => {
                "authentication failed (different pairing key, or sealed for an earlier receiver session)"
            }
            Self::Retired => "replay of a retired stream",
            Self::WrongType => "packet type not accepted on the receiver port",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropInfo {
    /// Header type byte when the header decoded.
    pub ptype: Option<u8>,
    pub reason: DropReason,
}

#[derive(Debug)]
pub enum Action<'a> {
    Audio(AudioPacket<'a>),
    /// Datagram to send back (hello in answer to a probe).
    Reply(Vec<u8>, SocketAddr),
    /// Authenticated handoff offer or release (SPEC §9) for the worker.
    Control(Packet<'a>, SocketAddr),
    /// Authenticated jitter-buffer floor/cap request (type 21).
    Tuning(protocol::TuningSet, SocketAddr),
    Drop,
}

pub struct Ingress {
    keys: Option<SessionKeys>,
    /// Random per run; mixed into audio tags so captures from earlier runs
    /// never verify.
    nonce: u64,
    allow: Option<IpAddr>,
    current_stream: Option<u16>,
    /// One bit per stream id that has been replaced during this run.
    retired: Box<[u64; 1024]>,
    highest_seq: Option<u32>,
    peer: Option<SocketAddr>,
    audio_peer: Option<SocketAddr>,
    pub counters: IngressCounters,
    /// Set when `handle` returns `Drop`, cleared otherwise.
    pub last_drop: Option<DropInfo>,
    /// This machine's IPv4 toward the last address asked about (cached; the
    /// lookup is a routing-table query, no traffic).
    local_ip: std::cell::Cell<Option<(IpAddr, [u8; 4])>>,
}

/// The local IPv4 the OS would use to reach `peer` (0.0.0.0 if none). A UDP
/// `connect` only consults the routing table; nothing is sent.
pub fn local_ipv4_toward(peer: IpAddr) -> [u8; 4] {
    let probe = || -> std::io::Result<[u8; 4]> {
        let s = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
        s.connect((peer, 9))?;
        match s.local_addr()?.ip() {
            IpAddr::V4(v4) => Ok(v4.octets()),
            IpAddr::V6(_) => Ok([0; 4]),
        }
    };
    probe().unwrap_or([0; 4])
}

impl Ingress {
    pub fn new(keys: Option<SessionKeys>, nonce: u64, allow: Option<IpAddr>) -> Self {
        Self {
            keys,
            nonce,
            allow,
            current_stream: None,
            retired: Box::new([0; 1024]),
            highest_seq: None,
            peer: None,
            audio_peer: None,
            counters: IngressCounters::default(),
            last_drop: None,
            local_ip: std::cell::Cell::new(None),
        }
    }

    /// Cached `local_ipv4_toward`.
    pub fn local_ipv4(&self, peer: IpAddr) -> [u8; 4] {
        if let Some((p, ip)) = self.local_ip.get()
            && p == peer
        {
            return ip;
        }
        let ip = local_ipv4_toward(peer);
        self.local_ip.set(Some((peer, ip)));
        ip
    }

    pub fn session_nonce(&self) -> u64 {
        self.nonce
    }

    pub fn keys(&self) -> Option<&SessionKeys> {
        self.keys.as_ref()
    }

    /// Replaces the keys (pairing file changed). Forgets the current peer.
    pub fn set_keys(&mut self, keys: Option<SessionKeys>) {
        self.keys = keys;
        self.peer = None;
        self.audio_peer = None;
    }

    /// Address of the last nonce-bound authenticated packet from the paired
    /// Mac (audio, handoff offer/release, tuning); probes do not count.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// Where audio comes from; stats go here.
    pub fn audio_peer(&self) -> Option<SocketAddr> {
        self.audio_peer
    }

    /// Highest audio sequence seen on the current stream (stats echo).
    pub fn echo_seq(&self) -> u32 {
        self.highest_seq.unwrap_or(0)
    }

    pub fn current_stream(&self) -> Option<u16> {
        self.current_stream
    }

    fn is_retired(&self, id: u16) -> bool {
        self.retired[id as usize / 64] & (1 << (id % 64)) != 0
    }

    fn retire(&mut self, id: u16) {
        self.retired[id as usize / 64] |= 1 << (id % 64);
    }

    /// Hello for `to` (an answer to a probe, or the unsolicited start-up one).
    pub fn hello(&self, echo: u64, to: SocketAddr) -> Option<Vec<u8>> {
        let k = self.keys?;
        Some(auth::seal(
            &k.r2m,
            None,
            protocol::encode_hello(&Hello {
                session_nonce: self.nonce,
                echo,
                ext: Some(protocol::HelloExt {
                    revision: protocol::PROTOCOL_REVISION,
                    receiver_ipv4: self.local_ipv4(to.ip()),
                    // Lets the Mac relate this receiver's clock to its own
                    // (latency marker test); stamped as late as possible.
                    receiver_time_us: (crate::clock::now_s() * 1e6) as u64,
                }),
            }),
        ))
    }

    fn reject<'a>(&mut self, ptype: Option<u8>, reason: DropReason) -> Action<'a> {
        match reason {
            DropReason::Filtered => self.counters.filtered += 1,
            DropReason::Retired => self.counters.replayed += 1,
            _ => self.counters.auth_dropped += 1,
        }
        self.last_drop = Some(DropInfo { ptype, reason });
        Action::Drop
    }

    pub fn handle<'a>(&mut self, data: &'a [u8], from: SocketAddr) -> Action<'a> {
        self.last_drop = None;
        if self.allow.is_some_and(|ip| ip != from.ip()) {
            return self.reject(None, DropReason::Filtered);
        }
        let Ok((ptype, stream_id)) = protocol::decode_header(data) else {
            return self.reject(None, DropReason::Malformed);
        };
        let t = Some(ptype);
        let Some(keys) = self.keys else {
            // Unpaired: nothing is accepted (pairing runs in its own process).
            return self.reject(t, DropReason::Unpaired);
        };
        match ptype {
            protocol::TYPE_AUDIO => {
                if !auth::verify(&keys.m2r, Some(self.nonce), data) {
                    return self.reject(t, DropReason::BadTag);
                }
                let Ok(p) = protocol::decode_audio(data) else {
                    return self.reject(t, DropReason::Malformed);
                };
                if self.current_stream != Some(stream_id) {
                    if self.is_retired(stream_id) {
                        return self.reject(t, DropReason::Retired);
                    }
                    if let Some(old) = self.current_stream {
                        self.retire(old);
                    }
                    self.current_stream = Some(stream_id);
                    self.highest_seq = None;
                }
                let seq = p.header.sequence;
                if self
                    .highest_seq
                    .is_none_or(|h| seq.wrapping_sub(h) < 1 << 31)
                {
                    self.highest_seq = Some(seq);
                }
                self.peer = Some(from);
                self.audio_peer = Some(from);
                Action::Audio(p)
            }
            protocol::TYPE_PROBE => {
                if !auth::verify(&keys.m2r, None, data) {
                    return self.reject(t, DropReason::BadTag);
                }
                let Ok(Packet::Probe { nonce }) = protocol::decode(data) else {
                    return self.reject(t, DropReason::Malformed);
                };
                // Probes carry no session nonce, so a captured one replays
                // forever: it gets an answer (to wherever it came from) but
                // never moves `peer`, the address saved for start-up hellos
                // and deck requests. Only nonce-bound packets do that.
                match self.hello(nonce, from) {
                    Some(h) => Action::Reply(h, from),
                    None => Action::Drop,
                }
            }
            protocol::TYPE_HANDOFF_OFFER | protocol::TYPE_HANDOFF_RELEASE => {
                if !auth::verify(&keys.m2r, Some(self.nonce), data) {
                    return self.reject(t, DropReason::BadTag);
                }
                match protocol::decode(data) {
                    Ok(p) => {
                        self.peer = Some(from);
                        Action::Control(p, from)
                    }
                    Err(_) => self.reject(t, DropReason::Malformed),
                }
            }
            protocol::TYPE_TUNING_SET => {
                if !auth::verify(&keys.m2r, Some(self.nonce), data) {
                    return self.reject(t, DropReason::BadTag);
                }
                match protocol::decode(data) {
                    Ok(Packet::TuningSet(m)) => {
                        self.peer = Some(from);
                        Action::Tuning(m, from)
                    }
                    _ => self.reject(t, DropReason::Malformed),
                }
            }
            _ => {
                // Stats/hello/acks are receiver→Mac; pairing is handled by
                // `tightwire-recv pair`; other control types are undefined.
                self.reject(t, DropReason::WrongType)
            }
        }
    }
}

//! Tightwire wire protocol v2 (SPEC §3, §3a). All integers little-endian.
//!
//! Decoding checks structure only. Tagged packets keep their tag in the
//! datagram; callers verify it with `auth::verify` before acting on anything.

use std::fmt;

use crate::auth::{TAG_LEN, Tag};

pub const MAGIC: [u8; 4] = *b"TWIR";
pub const VERSION: u8 = 2;
pub const HEADER_LEN: usize = 8;
pub const AUDIO_HEADER_LEN: usize = HEADER_LEN + 16;
/// Keeps every packet inside one Ethernet frame (no IP fragmentation).
pub const MAX_PAYLOAD: usize = 1400;
pub const MAX_CHANNELS: u8 = 8;
/// Largest sample count any valid audio packet can carry (s16 at max payload).
pub const MAX_SAMPLES: usize = MAX_PAYLOAD / 2;
pub const STATS_BODY_LEN: usize = HEADER_LEN + 40;
/// Stats extension appended by 0.2.0 receivers (SPEC §3, "stats extension").
/// Older Macs ignore trailing bytes; older receivers simply do not send it.
pub const STATS_EXT_LEN: usize = 88;
/// Revision 2 (0.2.1) appends `StatsHealth` after those 88 bytes.
pub const STATS_HEALTH_LEN: usize = 25;
pub const STATS_EXT_REVISION: u8 = 2;
pub const HELLO_BODY_LEN: usize = HEADER_LEN + 16;
/// Hello extension (0.2.0 receivers): revision, receiver IPv4, receiver clock.
pub const HELLO_EXT_LEN: usize = 13;
/// Protocol revision within version 2, carried in the hello and stats
/// extensions. 0 = 0.1.x (no extension), 1 = 0.2.0, 2 = 0.2.1 (stats
/// health block). The version byte stays 2 because every change is
/// append-only or a new type: an older peer still streams, and each side
/// tells the user to update the other (SPEC §3).
pub const PROTOCOL_REVISION: u8 = 2;
pub const TUNING_BODY_LEN: usize = HEADER_LEN + 13;
/// Upper bound for a remotely set buffer cap: well inside the jitter ring
/// (32768 frames = 341 ms at 96 kHz) and far above anything useful live.
pub const MAX_TUNING_US: u32 = 100_000;
pub const PROBE_BODY_LEN: usize = HEADER_LEN + 8;
/// Largest datagram any v2 packet can be.
pub const MAX_DATAGRAM: usize = AUDIO_HEADER_LEN + MAX_PAYLOAD + TAG_LEN;
pub const DEFAULT_PORT: u16 = 4810;
pub const BEACON_PORT: u16 = 4811;
pub const SENDER_PORT: u16 = 4812;
/// Fixed pairing port so one firewall rule covers it (SPEC §10).
pub const PAIR_PORT: u16 = 4814;
pub const ID_LEN: usize = 16;
pub const KEY_LEN: usize = 32;
pub const CONFIRM_MAC_LEN: usize = 16;

pub const TYPE_AUDIO: u8 = 1;
pub const TYPE_STATS: u8 = 2;
pub const TYPE_BEACON: u8 = 3;
pub const TYPE_HELLO: u8 = 4;
pub const TYPE_PROBE: u8 = 5;
pub const TYPE_PAIR_COMMIT: u8 = 8;
pub const TYPE_PAIR_RESPONSE: u8 = 9;
pub const TYPE_PAIR_REVEAL: u8 = 10;
pub const TYPE_PAIR_CONFIRM: u8 = 11;
pub const TYPE_PAIR_ABORT: u8 = 12;
/// 16–31: authenticated control messages (SPEC §9 uses 16–20; the rest are
/// reserved and rejected).
pub const CONTROL_TYPES: std::ops::RangeInclusive<u8> = 16..=31;
pub const TYPE_HANDOFF_OFFER: u8 = 16;
pub const TYPE_HANDOFF_ACK: u8 = 17;
pub const TYPE_HANDOFF_RELEASE: u8 = 18;
pub const TYPE_DECK_REQUEST: u8 = 19;
pub const TYPE_DECK_ACK: u8 = 20;
/// Mac → receiver: set the jitter-buffer floor and cap (persisted).
pub const TYPE_TUNING_SET: u8 = 21;
/// Receiver → Mac: what was applied.
pub const TYPE_TUNING_ACK: u8 = 22;
/// The Mac's handoff control socket (SPEC §9).
pub const HANDOFF_PORT: u16 = 4813;

pub const FLAG_DISCONTINUITY: u8 = 0x01;
/// Latency marker: the receiver reports when this packet's first frame was
/// handed to the output device (stats extension). Ignored by 0.1 receivers.
pub const FLAG_MARKER: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Length,
    Magic,
    Version,
    Type,
    SampleRate,
    Channels,
    Format,
    Utf8,
    Field,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Length => "bad length",
            Self::Magic => "bad magic",
            Self::Version => "unsupported version",
            Self::Type => "unknown packet type",
            Self::SampleRate => "unknown sample-rate code",
            Self::Channels => "channel count out of range",
            Self::Format => "unknown sample format",
            Self::Utf8 => "name is not UTF-8",
            Self::Field => "field out of range",
        };
        f.write_str(s)
    }
}

impl std::error::Error for DecodeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    S24,
    S16,
}

impl SampleFormat {
    pub fn code(self) -> u8 {
        match self {
            Self::F32 => 0,
            Self::S24 => 1,
            Self::S16 => 2,
        }
    }

    pub fn from_code(code: u8) -> Result<Self, DecodeError> {
        match code {
            0 => Ok(Self::F32),
            1 => Ok(Self::S24),
            2 => Ok(Self::S16),
            _ => Err(DecodeError::Format),
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::S24 => 3,
            Self::S16 => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::S24 => "s24",
            Self::S16 => "s16",
        }
    }
}

pub const SAMPLE_RATES: [u32; 4] = [44100, 48000, 88200, 96000];

pub fn rate_from_code(code: u8) -> Result<u32, DecodeError> {
    SAMPLE_RATES
        .get(code as usize)
        .copied()
        .ok_or(DecodeError::SampleRate)
}

pub fn rate_code(rate: u32) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
}

/// Fixed part of an audio packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub stream_id: u16,
    pub sequence: u32,
    pub timestamp: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub format: SampleFormat,
    pub flags: u8,
}

impl AudioHeader {
    pub fn discontinuity(&self) -> bool {
        self.flags & FLAG_DISCONTINUITY != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub stream_id: u16,
    pub depth_us: u32,
    pub received: u32,
    pub lost: u32,
    pub late: u32,
    pub drift_ppm: i32,
    pub underruns: u32,
    pub auth_dropped: u32,
    pub echo_seq: u32,
    pub session_nonce: u64,
    /// Present from 0.2.0 receivers.
    pub ext: Option<StatsExt>,
}

/// How the receiver's output runs, for the Mac's latency breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// No device open (audio discarded) or not reported.
    #[default]
    None = 0,
    WasapiExclusive = 1,
    /// Shared mode through IAudioClient3 at a chosen engine period.
    WasapiSharedLowLatency = 2,
    /// Shared mode at the engine's default period (IAudioClient).
    WasapiShared = 3,
    Cpal = 4,
    File = 5,
}

impl OutputMode {
    pub fn from_code(c: u8) -> Self {
        match c {
            1 => Self::WasapiExclusive,
            2 => Self::WasapiSharedLowLatency,
            3 => Self::WasapiShared,
            4 => Self::Cpal,
            5 => Self::File,
            _ => Self::None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WasapiExclusive => "WASAPI exclusive",
            Self::WasapiSharedLowLatency => "WASAPI shared (IAudioClient3)",
            Self::WasapiShared => "WASAPI shared",
            Self::Cpal => "cpal",
            Self::File => "file",
        }
    }
}

/// Stats extension (SPEC §3). Times are µs; `*_us` clocks are the
/// receiver's monotonic clock (same clock as hello's `receiver_time_us`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsExt {
    pub output_mode: OutputMode,
    pub stream_channels: u8,
    pub output_channels: u8,
    pub target_us: u32,
    pub jitter_mean_us: u32,
    pub jitter_peak_us: u32,
    /// Configured floor (0 = adaptive) and cap of the target.
    pub min_target_us: u32,
    pub max_target_us: u32,
    pub output_period_frames: u32,
    pub output_rate: u32,
    pub stream_rate: u32,
    /// Resampling correction in parts per billion (drift_ppm, finer).
    pub drift_ppb: i32,
    pub duplicates: u32,
    pub concealed_frames: u32,
    pub resets: u32,
    /// Socket → audio-thread ring: slots in use when the stats were sent.
    pub ring_fill: u16,
    pub ring_capacity: u16,
    pub ring_full: u32,
    /// Stream timestamp of the last latency marker played (0 = none yet).
    pub marker_timestamp: u64,
    /// When that marker's first frame was handed to the output device.
    pub marker_output_us: u64,
    /// Receiver clock when these stats were built.
    pub receiver_now_us: u64,
    /// The receiver's own IPv4 toward the Mac (inside the tag), so the Mac
    /// can check the datagram source before it believes an address change.
    pub receiver_ipv4: [u8; 4],
    /// Revision 2 (0.2.1) receivers.
    pub health: Option<StatsHealth>,
}

/// Output health (stats extension revision 2, 0.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsHealth {
    /// `HEALTH_*` bits.
    pub flags: u8,
    /// Audio queued in the output endpoint beyond the jitter buffer (µs):
    /// latency the buffer depth does not include.
    pub device_queue_us: u32,
    /// The output device's measured rate vs nominal (ppb; 0 = not known).
    pub device_rate_ppb: i32,
    /// How far the device pulls ahead of its playout (µs): half a period
    /// for a steady device, half a burst for a bursty one.
    pub device_lead_us: u32,
    /// Overflow trims, and the discrete skip / insert corrections so far.
    pub trims: u32,
    pub skips: u32,
    pub inserts: u32,
}

/// An output device is open and has rendered in the last second. Without
/// it the PC is not playing the stream anywhere.
pub const HEALTH_OUTPUT_OPEN: u8 = 1 << 0;
/// The drift loop has been at a limit for seconds.
pub const HEALTH_SATURATED: u8 = 1 << 1;
/// Underruns are climbing (more than one in the last 10 s).
pub const HEALTH_UNDERRUNS: u8 = 1 << 2;
/// Exclusive mode misbehaved and the receiver fell back to shared mode.
pub const HEALTH_EXCLUSIVE_FALLBACK: u8 = 1 << 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub session_nonce: u64,
    pub echo: u64,
    /// Present from 0.2.0 receivers.
    pub ext: Option<HelloExt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloExt {
    /// `PROTOCOL_REVISION` of the receiver.
    pub revision: u8,
    /// The receiver's own IPv4 toward the Mac (0.0.0.0 = unknown).
    pub receiver_ipv4: [u8; 4],
    /// Receiver monotonic clock (µs) when the hello was built.
    pub receiver_time_us: u64,
}

/// Type 21: jitter-buffer floor and cap. `adaptive` clears the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuningSet {
    pub request_id: u32,
    pub adaptive: bool,
    pub min_target_us: u32,
    pub max_target_us: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuningResult {
    Applied = 0,
    /// Values out of range; nothing changed.
    Rejected = 1,
}

/// Type 22.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuningAck {
    pub request_id: u32,
    pub result: TuningResult,
    pub min_target_us: u32,
    pub max_target_us: u32,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    pub port: u16,
    pub receiver_id: [u8; ID_LEN],
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairCommit {
    pub id: [u8; ID_LEN],
    pub commitment: [u8; 32],
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairResponse {
    pub id: [u8; ID_LEN],
    pub public_key: [u8; KEY_LEN],
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairConfirm {
    /// 0 = Mac (initiator), 1 = receiver.
    pub role: u8,
    pub mac: [u8; CONFIRM_MAC_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffOffer {
    pub request_id: u32,
    pub usbip_port: u16,
    /// Empty = use the packet's source address.
    pub host: String,
    pub bus_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffOp {
    Attach = 1,
    Detach = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffResult {
    Ok = 0,
    Failed = 1,
    UsbipMissing = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffItem {
    pub bus_id: String,
    pub result: HandoffResult,
    pub port: u8,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffAck {
    pub request_id: u32,
    pub op: HandoffOp,
    pub items: Vec<HandoffItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckTarget {
    Mac = 0,
    Pc = 1,
    Toggle = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckRequest {
    pub request_id: u32,
    pub sent_at_ms: u64,
    pub target: DeckTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckAck {
    pub request_id: u32,
    pub accepted: bool,
    /// 0 = Mac, 1 = PC, after the request was applied.
    pub owner: u8,
    pub message: String,
}

/// A decoded audio packet borrowing its payload from the datagram.
#[derive(Debug, Clone, Copy)]
pub struct AudioPacket<'a> {
    pub header: AudioHeader,
    pub payload: &'a [u8],
}

impl AudioPacket<'_> {
    pub fn frames(&self) -> usize {
        self.payload.len() / (self.header.channels as usize * self.header.format.bytes())
    }

    /// Converts the interleaved payload to f32 into `out`, returning the sample
    /// count. Allocation-free; `out` must hold at least `MAX_SAMPLES`.
    pub fn samples_to_f32(&self, out: &mut [f32]) -> usize {
        let p = self.payload;
        match self.header.format {
            SampleFormat::F32 => {
                for (o, b) in out.iter_mut().zip(p.chunks_exact(4)) {
                    *o = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
                p.len() / 4
            }
            SampleFormat::S24 => {
                for (o, b) in out.iter_mut().zip(p.chunks_exact(3)) {
                    // Place the 24 bits in the top of an i32 so the shift sign-extends.
                    let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                    *o = v as f32 / 8_388_608.0;
                }
                p.len() / 3
            }
            SampleFormat::S16 => {
                for (o, b) in out.iter_mut().zip(p.chunks_exact(2)) {
                    *o = i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0;
                }
                p.len() / 2
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Packet<'a> {
    Audio(AudioPacket<'a>),
    Stats(Stats),
    Beacon(Beacon),
    Hello(Hello),
    Probe {
        nonce: u64,
    },
    Commit(PairCommit),
    Response(PairResponse),
    Reveal {
        public_key: [u8; KEY_LEN],
    },
    Confirm(PairConfirm),
    Abort {
        reason: u8,
    },
    HandoffOffer(HandoffOffer),
    HandoffAck(HandoffAck),
    HandoffRelease {
        request_id: u32,
        bus_ids: Vec<String>,
    },
    DeckRequest(DeckRequest),
    DeckAck(DeckAck),
    TuningSet(TuningSet),
    TuningAck(TuningAck),
}

/// Whether packets of this type carry an authentication tag.
pub fn is_tagged(ptype: u8) -> bool {
    matches!(ptype, TYPE_AUDIO | TYPE_STATS | TYPE_HELLO | TYPE_PROBE)
        || CONTROL_TYPES.contains(&ptype)
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn arr<const N: usize>(b: &[u8], o: usize) -> [u8; N] {
    b[o..o + N].try_into().unwrap()
}

/// Returns (type, stream id) after checking magic and version.
pub fn decode_header(b: &[u8]) -> Result<(u8, u16), DecodeError> {
    if b.len() < HEADER_LEN {
        return Err(DecodeError::Length);
    }
    if b[0..4] != MAGIC {
        return Err(DecodeError::Magic);
    }
    if b[4] != VERSION {
        return Err(DecodeError::Version);
    }
    Ok((b[5], u16_at(b, 6)))
}

/// Splits a tagged datagram into (body, tag).
pub fn split_tag(b: &[u8]) -> Result<(&[u8], &Tag), DecodeError> {
    if b.len() < HEADER_LEN + TAG_LEN {
        return Err(DecodeError::Length);
    }
    let (body, tag) = b.split_at(b.len() - TAG_LEN);
    Ok((body, tag.try_into().unwrap()))
}

/// Decodes the fixed part of an audio packet without touching the payload.
/// Used on the socket thread, so it must not allocate.
pub fn decode_audio(b: &[u8]) -> Result<AudioPacket<'_>, DecodeError> {
    let (ptype, stream_id) = decode_header(b)?;
    if ptype != TYPE_AUDIO {
        return Err(DecodeError::Type);
    }
    if b.len() < AUDIO_HEADER_LEN + TAG_LEN {
        return Err(DecodeError::Length);
    }
    let (body, _) = split_tag(b)?;
    let sample_rate = rate_from_code(body[20])?;
    let channels = body[21];
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(DecodeError::Channels);
    }
    let format = SampleFormat::from_code(body[22])?;
    let payload = &body[AUDIO_HEADER_LEN..];
    let frame_bytes = channels as usize * format.bytes();
    if payload.is_empty() || payload.len() > MAX_PAYLOAD || payload.len() % frame_bytes != 0 {
        return Err(DecodeError::Length);
    }
    Ok(AudioPacket {
        header: AudioHeader {
            stream_id,
            sequence: u32_at(body, 8),
            timestamp: u64_at(body, 12),
            sample_rate,
            channels,
            format,
            flags: body[23],
        },
        payload,
    })
}

/// Peer names (beacon host, pairing names) are shown next to the pairing
/// code, so they are limited to this many characters and must not contain
/// control or format characters (SPEC §3a, security).
pub const MAX_PEER_NAME_CHARS: usize = 40;
/// Free-text messages from the peer (acks) are cut to this when decoded.
pub const MAX_MESSAGE_CHARS: usize = 200;

/// Unicode general categories Cc and Cf (bidi overrides and isolates,
/// zero-width characters, BOM…) plus the line and paragraph separators:
/// anything that can move, hide or reorder text around a pairing code.
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c as u32,
            0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2 | 0x180E
            | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
            | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// Drops unsafe characters and cuts to `max_chars` (adding "…"). Used for
/// everything a peer can say that ends up on screen or in a log.
pub fn sanitize_text(s: &str, max_chars: usize) -> String {
    let clean: String = s.chars().filter(|c| !is_unsafe_char(*c)).collect();
    let clean = clean.trim();
    if clean.chars().count() <= max_chars {
        clean.to_owned()
    } else {
        let cut: String = clean.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    }
}

/// What this side puts on the wire as its own name.
pub fn outgoing_peer_name(s: &str) -> String {
    sanitize_text(s, MAX_PEER_NAME_CHARS)
}

pub fn valid_peer_name(s: &str) -> bool {
    s.chars().count() <= MAX_PEER_NAME_CHARS && !s.chars().any(is_unsafe_char)
}

/// A peer name: rejected (not cleaned) when it is too long or carries
/// unsafe characters, so a hostile name never reaches the screen at all.
fn decode_peer_name(b: &[u8], o: usize) -> Result<String, DecodeError> {
    let n = decode_name(b, o)?;
    if valid_peer_name(&n) {
        Ok(n)
    } else {
        Err(DecodeError::Field)
    }
}

/// A free-text message: cleaned and shortened.
fn message_at(b: &[u8], o: usize) -> Result<(String, usize), DecodeError> {
    let (m, n) = name_at(b, o)?;
    Ok((sanitize_text(&m, MAX_MESSAGE_CHARS), n))
}

fn decode_name(b: &[u8], o: usize) -> Result<String, DecodeError> {
    let len = *b.get(o).ok_or(DecodeError::Length)? as usize;
    let raw = b.get(o + 1..o + 1 + len).ok_or(DecodeError::Length)?;
    std::str::from_utf8(raw)
        .map(str::to_owned)
        .map_err(|_| DecodeError::Utf8)
}

/// u8-length-prefixed UTF-8 at `o`; returns it and the offset after it.
fn name_at(b: &[u8], o: usize) -> Result<(String, usize), DecodeError> {
    let len = *b.get(o).ok_or(DecodeError::Length)? as usize;
    Ok((decode_name(b, o)?, o + 1 + len))
}

/// Count byte then that many BUSIDs. BUSIDs are short ASCII like "1-3".
fn bus_ids_at(b: &[u8], o: usize) -> Result<(Vec<String>, usize), DecodeError> {
    let count = *b.get(o).ok_or(DecodeError::Length)? as usize;
    let mut o = o + 1;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let (id, n) = name_at(b, o)?;
        if !valid_bus_id(&id) {
            return Err(DecodeError::Field);
        }
        out.push(id);
        o = n;
    }
    Ok((out, o))
}

/// USB/IP bus ids look like "1-1" or "1-4.2" (`^[0-9]+-[0-9]+(\.[0-9]+)*$`);
/// they end up on usbip.exe's command line, so nothing else is accepted.
pub fn valid_bus_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 32 {
        return false;
    }
    let Some((bus, port)) = id.split_once('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    digits(bus) && port.split('.').all(digits)
}

fn need(b: &[u8], len: usize) -> Result<(), DecodeError> {
    if b.len() < len {
        Err(DecodeError::Length)
    } else {
        Ok(())
    }
}

/// Decodes any packet. Bodies may carry trailing bytes (ignored) so later
/// versions can append fields; for tagged packets the tag is the last 8
/// bytes and is not checked here.
pub fn decode(b: &[u8]) -> Result<Packet<'_>, DecodeError> {
    let (ptype, _) = decode_header(b)?;
    if ptype == TYPE_AUDIO {
        return decode_audio(b).map(Packet::Audio);
    }
    let body = if is_tagged(ptype) { split_tag(b)?.0 } else { b };
    match ptype {
        TYPE_STATS => {
            need(body, STATS_BODY_LEN)?;
            // A shorter or unknown-revision extension is ignored, not an
            // error: the base fields stay usable.
            let e = STATS_BODY_LEN;
            let ext = (body.len() >= e + STATS_EXT_LEN && body[e] >= 1).then(|| StatsExt {
                output_mode: OutputMode::from_code(body[e + 1]),
                stream_channels: body[e + 2],
                output_channels: body[e + 3],
                target_us: u32_at(body, e + 4),
                jitter_mean_us: u32_at(body, e + 8),
                jitter_peak_us: u32_at(body, e + 12),
                min_target_us: u32_at(body, e + 16),
                max_target_us: u32_at(body, e + 20),
                output_period_frames: u32_at(body, e + 24),
                output_rate: u32_at(body, e + 28),
                stream_rate: u32_at(body, e + 32),
                drift_ppb: u32_at(body, e + 36) as i32,
                duplicates: u32_at(body, e + 40),
                concealed_frames: u32_at(body, e + 44),
                resets: u32_at(body, e + 48),
                ring_fill: u16_at(body, e + 52),
                ring_capacity: u16_at(body, e + 54),
                ring_full: u32_at(body, e + 56),
                marker_timestamp: u64_at(body, e + 60),
                marker_output_us: u64_at(body, e + 68),
                receiver_now_us: u64_at(body, e + 76),
                receiver_ipv4: arr(body, e + 84),
                health: (body[e] >= 2 && body.len() >= e + STATS_EXT_LEN + STATS_HEALTH_LEN).then(
                    || {
                        let h = e + STATS_EXT_LEN;
                        StatsHealth {
                            flags: body[h],
                            device_queue_us: u32_at(body, h + 1),
                            device_rate_ppb: u32_at(body, h + 5) as i32,
                            device_lead_us: u32_at(body, h + 9),
                            trims: u32_at(body, h + 13),
                            skips: u32_at(body, h + 17),
                            inserts: u32_at(body, h + 21),
                        }
                    },
                ),
            });
            Ok(Packet::Stats(Stats {
                ext,
                stream_id: u16_at(body, 6),
                depth_us: u32_at(body, 8),
                received: u32_at(body, 12),
                lost: u32_at(body, 16),
                late: u32_at(body, 20),
                drift_ppm: u32_at(body, 24) as i32,
                underruns: u32_at(body, 28),
                auth_dropped: u32_at(body, 32),
                echo_seq: u32_at(body, 36),
                session_nonce: u64_at(body, 40),
            }))
        }
        TYPE_HELLO => {
            need(body, HELLO_BODY_LEN)?;
            Ok(Packet::Hello(Hello {
                session_nonce: u64_at(body, 8),
                echo: u64_at(body, 16),
                ext: (body.len() >= HELLO_BODY_LEN + HELLO_EXT_LEN && body[HELLO_BODY_LEN] >= 1)
                    .then(|| HelloExt {
                        revision: body[HELLO_BODY_LEN],
                        receiver_ipv4: arr(body, HELLO_BODY_LEN + 1),
                        receiver_time_us: u64_at(body, HELLO_BODY_LEN + 5),
                    }),
            }))
        }
        TYPE_TUNING_SET => {
            need(body, TUNING_BODY_LEN)?;
            if body[12] > 1 {
                return Err(DecodeError::Field);
            }
            Ok(Packet::TuningSet(TuningSet {
                request_id: u32_at(body, 8),
                adaptive: body[12] == 1,
                min_target_us: u32_at(body, 13),
                max_target_us: u32_at(body, 17),
            }))
        }
        TYPE_TUNING_ACK => {
            need(body, HEADER_LEN + 14)?;
            let result = match body[12] {
                0 => TuningResult::Applied,
                1 => TuningResult::Rejected,
                _ => return Err(DecodeError::Field),
            };
            Ok(Packet::TuningAck(TuningAck {
                request_id: u32_at(body, 8),
                result,
                min_target_us: u32_at(body, 13),
                max_target_us: u32_at(body, 17),
                message: message_at(body, 21)?.0,
            }))
        }
        TYPE_PROBE => {
            need(body, PROBE_BODY_LEN)?;
            Ok(Packet::Probe {
                nonce: u64_at(body, 8),
            })
        }
        TYPE_BEACON => {
            need(body, HEADER_LEN + 2 + ID_LEN + 1)?;
            Ok(Packet::Beacon(Beacon {
                port: u16_at(body, 8),
                receiver_id: arr(body, 10),
                host: decode_peer_name(body, 10 + ID_LEN)?,
            }))
        }
        TYPE_PAIR_COMMIT => {
            need(body, HEADER_LEN + ID_LEN + 32 + 1)?;
            Ok(Packet::Commit(PairCommit {
                id: arr(body, 8),
                commitment: arr(body, 8 + ID_LEN),
                name: decode_peer_name(body, 8 + ID_LEN + 32)?,
            }))
        }
        TYPE_PAIR_RESPONSE => {
            need(body, HEADER_LEN + ID_LEN + KEY_LEN + 1)?;
            Ok(Packet::Response(PairResponse {
                id: arr(body, 8),
                public_key: arr(body, 8 + ID_LEN),
                name: decode_peer_name(body, 8 + ID_LEN + KEY_LEN)?,
            }))
        }
        TYPE_PAIR_REVEAL => {
            need(body, HEADER_LEN + KEY_LEN)?;
            Ok(Packet::Reveal {
                public_key: arr(body, 8),
            })
        }
        TYPE_PAIR_CONFIRM => {
            need(body, HEADER_LEN + 1 + CONFIRM_MAC_LEN)?;
            if body[8] > 1 {
                return Err(DecodeError::Field);
            }
            Ok(Packet::Confirm(PairConfirm {
                role: body[8],
                mac: arr(body, 9),
            }))
        }
        TYPE_PAIR_ABORT => {
            need(body, HEADER_LEN + 1)?;
            Ok(Packet::Abort { reason: body[8] })
        }
        TYPE_HANDOFF_OFFER => {
            need(body, HEADER_LEN + 6)?;
            let (host, o) = name_at(body, 14)?;
            let (bus_ids, _) = bus_ids_at(body, o)?;
            Ok(Packet::HandoffOffer(HandoffOffer {
                request_id: u32_at(body, 8),
                usbip_port: u16_at(body, 12),
                host,
                bus_ids,
            }))
        }
        TYPE_HANDOFF_ACK => {
            need(body, HEADER_LEN + 6)?;
            let op = match body[12] {
                1 => HandoffOp::Attach,
                2 => HandoffOp::Detach,
                _ => return Err(DecodeError::Field),
            };
            let count = body[13] as usize;
            let mut o = 14;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let (bus_id, n) = name_at(body, o)?;
                need(body, n + 2)?;
                let result = match body[n] {
                    0 => HandoffResult::Ok,
                    1 => HandoffResult::Failed,
                    2 => HandoffResult::UsbipMissing,
                    _ => return Err(DecodeError::Field),
                };
                let port = body[n + 1];
                let (message, n2) = message_at(body, n + 2)?;
                items.push(HandoffItem {
                    bus_id,
                    result,
                    port,
                    message,
                });
                o = n2;
            }
            Ok(Packet::HandoffAck(HandoffAck {
                request_id: u32_at(body, 8),
                op,
                items,
            }))
        }
        TYPE_HANDOFF_RELEASE => {
            need(body, HEADER_LEN + 5)?;
            let (bus_ids, _) = bus_ids_at(body, 12)?;
            Ok(Packet::HandoffRelease {
                request_id: u32_at(body, 8),
                bus_ids,
            })
        }
        TYPE_DECK_REQUEST => {
            need(body, HEADER_LEN + 13)?;
            let target = match body[20] {
                0 => DeckTarget::Mac,
                1 => DeckTarget::Pc,
                2 => DeckTarget::Toggle,
                _ => return Err(DecodeError::Field),
            };
            Ok(Packet::DeckRequest(DeckRequest {
                request_id: u32_at(body, 8),
                sent_at_ms: u64_at(body, 12),
                target,
            }))
        }
        TYPE_DECK_ACK => {
            need(body, HEADER_LEN + 7)?;
            if body[12] > 1 || body[13] > 1 {
                return Err(DecodeError::Field);
            }
            Ok(Packet::DeckAck(DeckAck {
                request_id: u32_at(body, 8),
                accepted: body[12] == 1,
                owner: body[13],
                message: message_at(body, 14)?.0,
            }))
        }
        _ => Err(DecodeError::Type),
    }
}

// ------------------------------------------------------------------ encoding
// Encoders return the body only; tagged packets get their tag from
// `auth::seal` so the key choice stays visible at the call site.

pub fn put_header(out: &mut Vec<u8>, ptype: u8, stream_id: u16) {
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(ptype);
    out.extend_from_slice(&stream_id.to_le_bytes());
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    let mut end = name.len().min(255);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    out.push(end as u8);
    out.extend_from_slice(&name.as_bytes()[..end]);
}

/// Audio body with a raw (already formatted) payload.
pub fn encode_audio(h: &AudioHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + payload.len() + TAG_LEN);
    put_header(&mut out, TYPE_AUDIO, h.stream_id);
    out.extend_from_slice(&h.sequence.to_le_bytes());
    out.extend_from_slice(&h.timestamp.to_le_bytes());
    out.push(rate_code(h.sample_rate).expect("unsupported sample rate"));
    out.push(h.channels);
    out.push(h.format.code());
    out.push(h.flags);
    out.extend_from_slice(payload);
    out
}

/// Encodes f32 samples into the payload format given by `format`.
pub fn encode_samples(format: SampleFormat, samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * format.bytes());
    for &s in samples {
        match format {
            SampleFormat::F32 => out.extend_from_slice(&s.to_le_bytes()),
            SampleFormat::S24 => {
                let v = (s as f64 * 8_388_608.0)
                    .round()
                    .clamp(-8_388_608.0, 8_388_607.0) as i32;
                out.extend_from_slice(&v.to_le_bytes()[..3]);
            }
            SampleFormat::S16 => {
                let v = (s as f64 * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    out
}

pub fn encode_stats(s: &Stats) -> Vec<u8> {
    let mut out = Vec::with_capacity(STATS_BODY_LEN + TAG_LEN);
    put_header(&mut out, TYPE_STATS, s.stream_id);
    for v in [
        s.depth_us,
        s.received,
        s.lost,
        s.late,
        s.drift_ppm as u32,
        s.underruns,
        s.auth_dropped,
        s.echo_seq,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&s.session_nonce.to_le_bytes());
    if let Some(e) = &s.ext {
        // Without a health block this is a revision-1 extension.
        out.push(if e.health.is_some() {
            STATS_EXT_REVISION
        } else {
            1
        });
        out.push(e.output_mode as u8);
        out.push(e.stream_channels);
        out.push(e.output_channels);
        for v in [
            e.target_us,
            e.jitter_mean_us,
            e.jitter_peak_us,
            e.min_target_us,
            e.max_target_us,
            e.output_period_frames,
            e.output_rate,
            e.stream_rate,
            e.drift_ppb as u32,
            e.duplicates,
            e.concealed_frames,
            e.resets,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&e.ring_fill.to_le_bytes());
        out.extend_from_slice(&e.ring_capacity.to_le_bytes());
        out.extend_from_slice(&e.ring_full.to_le_bytes());
        out.extend_from_slice(&e.marker_timestamp.to_le_bytes());
        out.extend_from_slice(&e.marker_output_us.to_le_bytes());
        out.extend_from_slice(&e.receiver_now_us.to_le_bytes());
        out.extend_from_slice(&e.receiver_ipv4);
        if let Some(h) = &e.health {
            out.push(h.flags);
            for v in [
                h.device_queue_us,
                h.device_rate_ppb as u32,
                h.device_lead_us,
                h.trims,
                h.skips,
                h.inserts,
            ] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    out
}

pub fn encode_hello(h: &Hello) -> Vec<u8> {
    let mut out = Vec::with_capacity(HELLO_BODY_LEN + HELLO_EXT_LEN + TAG_LEN);
    put_header(&mut out, TYPE_HELLO, 0);
    out.extend_from_slice(&h.session_nonce.to_le_bytes());
    out.extend_from_slice(&h.echo.to_le_bytes());
    if let Some(e) = &h.ext {
        out.push(e.revision);
        out.extend_from_slice(&e.receiver_ipv4);
        out.extend_from_slice(&e.receiver_time_us.to_le_bytes());
    }
    out
}

pub fn encode_tuning_set(m: &TuningSet) -> Vec<u8> {
    let mut out = Vec::with_capacity(TUNING_BODY_LEN + TAG_LEN);
    put_header(&mut out, TYPE_TUNING_SET, 0);
    out.extend_from_slice(&m.request_id.to_le_bytes());
    out.push(m.adaptive as u8);
    out.extend_from_slice(&m.min_target_us.to_le_bytes());
    out.extend_from_slice(&m.max_target_us.to_le_bytes());
    out
}

pub fn encode_tuning_ack(m: &TuningAck) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_TUNING_ACK, 0);
    out.extend_from_slice(&m.request_id.to_le_bytes());
    out.push(m.result as u8);
    out.extend_from_slice(&m.min_target_us.to_le_bytes());
    out.extend_from_slice(&m.max_target_us.to_le_bytes());
    put_name(&mut out, &m.message);
    out
}

pub fn encode_probe(nonce: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(PROBE_BODY_LEN + TAG_LEN);
    put_header(&mut out, TYPE_PROBE, 0);
    out.extend_from_slice(&nonce.to_le_bytes());
    out
}

pub fn encode_beacon(b: &Beacon) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_BEACON, 0);
    out.extend_from_slice(&b.port.to_le_bytes());
    out.extend_from_slice(&b.receiver_id);
    put_name(&mut out, &b.host);
    out
}

pub fn encode_commit(c: &PairCommit) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_PAIR_COMMIT, 0);
    out.extend_from_slice(&c.id);
    out.extend_from_slice(&c.commitment);
    put_name(&mut out, &c.name);
    out
}

pub fn encode_response(r: &PairResponse) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_PAIR_RESPONSE, 0);
    out.extend_from_slice(&r.id);
    out.extend_from_slice(&r.public_key);
    put_name(&mut out, &r.name);
    out
}

pub fn encode_reveal(public_key: &[u8; KEY_LEN]) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_PAIR_REVEAL, 0);
    out.extend_from_slice(public_key);
    out
}

pub fn encode_confirm(c: &PairConfirm) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_PAIR_CONFIRM, 0);
    out.push(c.role);
    out.extend_from_slice(&c.mac);
    out
}

pub fn encode_abort(reason: u8) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_PAIR_ABORT, 0);
    out.push(reason);
    out
}

fn put_bus_ids(out: &mut Vec<u8>, ids: &[String]) {
    out.push(ids.len().min(255) as u8);
    for id in ids.iter().take(255) {
        put_name(out, id);
    }
}

pub fn encode_handoff_offer(m: &HandoffOffer) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_HANDOFF_OFFER, 0);
    out.extend_from_slice(&m.request_id.to_le_bytes());
    out.extend_from_slice(&m.usbip_port.to_le_bytes());
    put_name(&mut out, &m.host);
    put_bus_ids(&mut out, &m.bus_ids);
    out
}

pub fn encode_handoff_ack(m: &HandoffAck) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_HANDOFF_ACK, 0);
    out.extend_from_slice(&m.request_id.to_le_bytes());
    out.push(m.op as u8);
    out.push(m.items.len().min(255) as u8);
    for i in m.items.iter().take(255) {
        put_name(&mut out, &i.bus_id);
        out.push(i.result as u8);
        out.push(i.port);
        put_name(&mut out, &i.message);
    }
    out
}

pub fn encode_handoff_release(request_id: u32, bus_ids: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_HANDOFF_RELEASE, 0);
    out.extend_from_slice(&request_id.to_le_bytes());
    put_bus_ids(&mut out, bus_ids);
    out
}

pub fn encode_deck_request(m: &DeckRequest) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_DECK_REQUEST, 0);
    out.extend_from_slice(&m.request_id.to_le_bytes());
    out.extend_from_slice(&m.sent_at_ms.to_le_bytes());
    out.push(m.target as u8);
    out
}

pub fn encode_deck_ack(m: &DeckAck) -> Vec<u8> {
    let mut out = Vec::new();
    put_header(&mut out, TYPE_DECK_ACK, 0);
    out.extend_from_slice(&m.request_id.to_le_bytes());
    out.push(m.accepted as u8);
    out.push(m.owner);
    put_name(&mut out, &m.message);
    out
}

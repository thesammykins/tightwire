//! Packet authentication and key schedule (SPEC §3a).
//!
//! SipHash-2-4 is implemented here rather than taken from a crate so the
//! receiver and the Mac's C implementation are visibly the same algorithm and
//! both are pinned to the reference vectors; it runs on the socket thread and
//! never allocates.

use hkdf::Hkdf;
use hmac::{KeyInit, Mac};
use sha2::{Digest, Sha256};

pub const TAG_LEN: usize = 8;
pub type Tag = [u8; TAG_LEN];
pub type SipKey = [u8; 16];

struct SipState {
    v: [u64; 4],
    tail: u64,
    ntail: usize,
    len: usize,
}

impl SipState {
    fn new(key: &SipKey) -> Self {
        let k0 = u64::from_le_bytes(key[0..8].try_into().unwrap());
        let k1 = u64::from_le_bytes(key[8..16].try_into().unwrap());
        Self {
            v: [
                k0 ^ 0x736f_6d65_7073_6575,
                k1 ^ 0x646f_7261_6e64_6f6d,
                k0 ^ 0x6c79_6765_6e65_7261,
                k1 ^ 0x7465_6462_7974_6573,
            ],
            tail: 0,
            ntail: 0,
            len: 0,
        }
    }

    #[inline]
    fn round(v: &mut [u64; 4]) {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    }

    #[inline]
    fn compress(&mut self, m: u64) {
        self.v[3] ^= m;
        Self::round(&mut self.v);
        Self::round(&mut self.v);
        self.v[0] ^= m;
    }

    fn write(&mut self, data: &[u8]) {
        self.len += data.len();
        let mut d = data;
        while self.ntail != 0 && !d.is_empty() {
            self.tail |= (d[0] as u64) << (8 * self.ntail);
            self.ntail += 1;
            d = &d[1..];
            if self.ntail == 8 {
                self.compress(self.tail);
                self.tail = 0;
                self.ntail = 0;
            }
        }
        let mut chunks = d.chunks_exact(8);
        for c in &mut chunks {
            self.compress(u64::from_le_bytes(c.try_into().unwrap()));
        }
        for (i, &b) in chunks.remainder().iter().enumerate() {
            self.tail |= (b as u64) << (8 * i);
        }
        self.ntail = chunks.remainder().len();
    }

    fn finish(mut self) -> Tag {
        let b = self.tail | ((self.len as u64 & 0xff) << 56);
        self.compress(b);
        self.v[2] ^= 0xff;
        for _ in 0..4 {
            Self::round(&mut self.v);
        }
        (self.v[0] ^ self.v[1] ^ self.v[2] ^ self.v[3]).to_le_bytes()
    }
}

/// SipHash-2-4 over the concatenation of `parts`.
pub fn siphash24(key: &SipKey, parts: &[&[u8]]) -> Tag {
    let mut s = SipState::new(key);
    for p in parts {
        s.write(p);
    }
    s.finish()
}

/// Tag for a packet body; `nonce` is the receiver session nonce for audio and
/// Mac→receiver control packets, `None` for everything else.
pub fn tag(key: &SipKey, nonce: Option<u64>, body: &[u8]) -> Tag {
    match nonce {
        Some(n) => siphash24(key, &[&n.to_le_bytes(), body]),
        None => siphash24(key, &[body]),
    }
}

/// Appends the tag to a packet body.
pub fn seal(key: &SipKey, nonce: Option<u64>, mut body: Vec<u8>) -> Vec<u8> {
    let t = tag(key, nonce, &body);
    body.extend_from_slice(&t);
    body
}

/// Checks the trailing tag of a datagram in constant time.
pub fn verify(key: &SipKey, nonce: Option<u64>, datagram: &[u8]) -> bool {
    if datagram.len() < TAG_LEN {
        return false;
    }
    let (body, got) = datagram.split_at(datagram.len() - TAG_LEN);
    let want = tag(key, nonce, body);
    want.iter().zip(got).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn hkdf_expand<const N: usize>(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .expect("HKDF output length is valid");
    out
}

/// Directional packet keys derived from the 32-byte root key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionKeys {
    /// Mac → receiver (audio, probe, control).
    pub m2r: SipKey,
    /// Receiver → Mac (stats, hello).
    pub r2m: SipKey,
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKeys(..)")
    }
}

impl SessionKeys {
    pub fn from_root(root: &[u8; 32]) -> Self {
        Self {
            m2r: hkdf_expand(root, b"", b"tightwire v2 m2r"),
            r2m: hkdf_expand(root, b"", b"tightwire v2 r2m"),
        }
    }
}

/// Outputs of a completed pairing key agreement.
pub struct PairingSecrets {
    pub root: [u8; 32],
    pub confirm_key: [u8; 32],
    /// Six-digit confirmation code.
    pub sas: u32,
    pub transcript_hash: [u8; 32],
}

impl PairingSecrets {
    /// `shared` is the X25519 output; the transcript is the three handshake
    /// datagrams (commit, response, reveal) exactly as sent.
    pub fn derive(shared: &[u8; 32], commit: &[u8], response: &[u8], reveal: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(commit);
        h.update(response);
        h.update(reveal);
        let th: [u8; 32] = h.finalize().into();
        let okm: [u8; 64] = hkdf_expand(shared, &th, b"tightwire v2 pairing");
        let sas: [u8; 4] = hkdf_expand(shared, &th, b"tightwire v2 sas");
        Self {
            root: okm[..32].try_into().unwrap(),
            confirm_key: okm[32..].try_into().unwrap(),
            sas: u32::from_be_bytes(sas) % 1_000_000,
            transcript_hash: th,
        }
    }

    /// Confirmation MAC for `role` (0 = Mac, 1 = receiver).
    pub fn confirm_mac(&self, role: u8) -> [u8; 16] {
        let mut m =
            hmac::Hmac::<Sha256>::new_from_slice(&self.confirm_key).expect("any key length");
        m.update(b"tightwire confirm");
        m.update(&[role]);
        m.update(&self.transcript_hash);
        m.finalize().into_bytes()[..16].try_into().unwrap()
    }

    pub fn sas_display(&self) -> String {
        format!("{:03} {:03}", self.sas / 1000, self.sas % 1000)
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Cryptographically random bytes from the OS.
pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random source unavailable");
    b
}

pub fn random_u64() -> u64 {
    u64::from_le_bytes(random())
}

pub fn parse_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != 2 * N || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

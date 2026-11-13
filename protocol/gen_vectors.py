"""Generate protocol/vectors/vectors.json, the golden packets shared by the Swift
and Rust test suites (protocol v2, SPEC §3 and §3a).

Bytes, tags and keys are built here with `struct`, `hashlib` and `hmac` straight
from the SPEC tables, so the vectors are an independent reference rather than
output of either codec. SipHash is implemented below and checked against the
reference vectors in siphash.json before anything is written.

Run: mise run vectors
"""

import hashlib
import hmac
import json
import struct
from pathlib import Path

HERE = Path(__file__).parent
MAGIC = b"TWIR"
VERSION = 2
RATE_CODES = {44100: 0, 48000: 1, 88200: 2, 96000: 3}
FORMAT_CODES = {"f32": 0, "s24": 1, "s16": 2}
T_AUDIO, T_STATS, T_BEACON, T_HELLO, T_PROBE = 1, 2, 3, 4, 5
T_COMMIT, T_RESPONSE, T_REVEAL, T_CONFIRM, T_ABORT = 8, 9, 10, 11, 12
T_OFFER, T_ACK, T_RELEASE, T_DECK_REQ, T_DECK_ACK = 16, 17, 18, 19, 20
T_TUNING_SET, T_TUNING_ACK = 21, 22

# ---------------------------------------------------------------- SipHash-2-4

M64 = (1 << 64) - 1


def _rotl(x, b):
    return ((x << b) | (x >> (64 - b))) & M64


def siphash24(key: bytes, msg: bytes) -> bytes:
    k0, k1 = struct.unpack("<QQ", key)
    v0 = k0 ^ 0x736F6D6570736575
    v1 = k1 ^ 0x646F72616E646F6D
    v2 = k0 ^ 0x6C7967656E657261
    v3 = k1 ^ 0x7465646279746573

    def rnd():
        nonlocal v0, v1, v2, v3
        v0 = (v0 + v1) & M64; v1 = _rotl(v1, 13); v1 ^= v0; v0 = _rotl(v0, 32)
        v2 = (v2 + v3) & M64; v3 = _rotl(v3, 16); v3 ^= v2
        v0 = (v0 + v3) & M64; v3 = _rotl(v3, 21); v3 ^= v0
        v2 = (v2 + v1) & M64; v1 = _rotl(v1, 17); v1 ^= v2; v2 = _rotl(v2, 32)

    end = len(msg) - len(msg) % 8
    for i in range(0, end, 8):
        m = struct.unpack_from("<Q", msg, i)[0]
        v3 ^= m; rnd(); rnd(); v0 ^= m
    tail = msg[end:] + bytes(7 - len(msg) % 8)
    m = struct.unpack("<Q", tail + bytes([len(msg) & 0xFF]))[0]
    v3 ^= m; rnd(); rnd(); v0 ^= m
    v2 ^= 0xFF
    for _ in range(4):
        rnd()
    return struct.pack("<Q", v0 ^ v1 ^ v2 ^ v3)


ref = json.loads((HERE / "vectors" / "siphash.json").read_text())
for v in ref["vectors"]:
    assert siphash24(bytes.fromhex(ref["key"]), bytes.fromhex(v["message"])).hex() == v["tag"], v

# ---------------------------------------------------------------- key schedule


def hkdf(ikm: bytes, salt: bytes, info: bytes, length: int) -> bytes:
    prk = hmac.new(salt or bytes(32), ikm, hashlib.sha256).digest()
    out, t, i = b"", b"", 1
    while len(out) < length:
        t = hmac.new(prk, t + info + bytes([i]), hashlib.sha256).digest()
        out += t
        i += 1
    return out[:length]


def session_keys(root: bytes):
    return (hkdf(root, b"", b"tightwire v2 m2r", 16), hkdf(root, b"", b"tightwire v2 r2m", 16))


ROOT = bytes(range(0x40, 0x60))
K_M2R, K_R2M = session_keys(ROOT)
NONCE = 0x0123456789ABCDEF


def tag(key, body, nonce=None):
    prefix = struct.pack("<Q", nonce) if nonce is not None else b""
    return siphash24(key, prefix + body)


# ---------------------------------------------------------------- packets


def header(ptype: int, stream_id: int, version: int = VERSION, magic: bytes = MAGIC) -> bytes:
    return magic + struct.pack("<BBH", version, ptype, stream_id)


def audio_body(stream_id, seq, ts, rate, channels, fmt, flags, samples) -> bytes:
    out = header(T_AUDIO, stream_id)
    out += struct.pack("<IQBBBB", seq, ts, RATE_CODES[rate], channels, FORMAT_CODES[fmt], flags)
    for s in samples:
        if fmt == "f32":
            out += struct.pack("<f", s)
        elif fmt == "s16":
            out += struct.pack("<h", s)
        else:
            out += struct.pack("<i", s)[:3]
    return out


def audio_packet(*a):
    body = audio_body(*a)
    return body + tag(K_M2R, body, NONCE)


def as_f32(fmt, samples):
    if fmt == "f32":
        return samples
    scale = 32768.0 if fmt == "s16" else 8388608.0
    return [s / scale for s in samples]


def vec(name, kind, data, fields, auth=None):
    v = {"name": name, "kind": kind, "hex": data.hex(), "fields": fields}
    if auth:
        v["auth"] = auth
    return v


def audio_vector(name, stream_id, seq, ts, rate, channels, fmt, flags, samples):
    return vec(name, "audio", audio_packet(stream_id, seq, ts, rate, channels, fmt, flags, samples), {
        "stream_id": stream_id, "sequence": seq, "timestamp": ts, "sample_rate": rate,
        "channels": channels, "format": fmt, "flags": flags, "samples": samples,
        "samples_f32": as_f32(fmt, samples),
    }, {"key": "m2r", "nonce": NONCE})


# Stats extension (0.2.0 receivers, appended after the 40-byte v2 body; v2
# Macs ignore it). Field order as in SPEC §3.
STATS_EXT_FIELDS = [
    ("output_mode", "B"), ("stream_channels", "B"), ("output_channels", "B"),
    ("target_us", "I"), ("jitter_mean_us", "I"), ("jitter_peak_us", "I"),
    ("min_target_us", "I"), ("max_target_us", "I"), ("output_period_frames", "I"),
    ("output_rate", "I"), ("stream_rate", "I"), ("drift_ppb", "i"), ("duplicates", "I"),
    ("concealed_frames", "I"), ("resets", "I"), ("ring_fill", "H"), ("ring_capacity", "H"),
    ("ring_full", "I"), ("marker_timestamp", "Q"), ("marker_output_us", "Q"), ("receiver_now_us", "Q"),
    ("receiver_ipv4", "4s"),
]
STATS_EXT_REVISION = 1
# Revision 2 (0.2.1) appends the health block after the 88 bytes.
STATS_HEALTH_FIELDS = [
    ("flags", "B"), ("device_queue_us", "I"), ("device_rate_ppb", "i"), ("device_lead_us", "I"),
    ("trims", "I"), ("skips", "I"), ("inserts", "I"),
]


def stats_vector(name, stream_id, depth_us, received, lost, late, drift_ppm, underruns, auth_dropped, echo_seq, nonce,
                 ext=None):
    body = header(T_STATS, stream_id) + struct.pack(
        "<IIIIiIIIQ", depth_us, received, lost, late, drift_ppm, underruns, auth_dropped, echo_seq, nonce)
    fields = {
        "stream_id": stream_id, "depth_us": depth_us, "received": received, "lost": lost, "late": late,
        "drift_ppm": drift_ppm, "underruns": underruns, "auth_dropped": auth_dropped,
        "echo_seq": echo_seq, "session_nonce": nonce,
    }
    if ext is not None:
        vals = [bytes(map(int, ext[k].split("."))) if k == "receiver_ipv4" else ext[k] for k, _ in STATS_EXT_FIELDS]
        health = ext.get("health")
        body += bytes([2 if health else STATS_EXT_REVISION])
        body += struct.pack("<" + "".join(t for _, t in STATS_EXT_FIELDS), *vals)
        assert len(body) == 8 + 40 + 88
        if health:
            body += struct.pack("<" + "".join(t for _, t in STATS_HEALTH_FIELDS),
                                *[health[k] for k, _ in STATS_HEALTH_FIELDS])
            assert len(body) == 8 + 40 + 88 + 25
        fields["ext"] = ext
    return vec(name, "stats", body + tag(K_R2M, body), fields, {"key": "r2m"})


def hello_vector(name, nonce, echo, ext=None):
    """ext: (revision, receiver IPv4 text, receiver clock µs), 0.2.0 receivers."""
    body = header(T_HELLO, 0) + struct.pack("<QQ", nonce, echo)
    fields = {"session_nonce": nonce, "echo": echo}
    if ext is not None:
        rev, ip, t = ext
        body += struct.pack("<B4sQ", rev, bytes(map(int, ip.split("."))), t)
        fields["ext"] = {"revision": rev, "receiver_ipv4": ip, "receiver_time_us": t}
    return vec(name, "hello", body + tag(K_R2M, body), fields, {"key": "r2m"})


def tuning_set_vector(name, rid, adaptive, min_us, max_us):
    body = header(T_TUNING_SET, 0) + struct.pack("<IBII", rid, adaptive, min_us, max_us)
    return vec(name, "tuning_set", body + tag(K_M2R, body, NONCE),
               {"request_id": rid, "adaptive": bool(adaptive), "min_target_us": min_us, "max_target_us": max_us},
               {"key": "m2r", "nonce": NONCE})


def tuning_ack_vector(name, rid, result, min_us, max_us, msg):
    body = header(T_TUNING_ACK, 0) + struct.pack("<IBII", rid, result, min_us, max_us) + name_field(msg)
    return vec(name, "tuning_ack", body + tag(K_R2M, body),
               {"request_id": rid, "result": result, "min_target_us": min_us, "max_target_us": max_us,
                "message": msg}, {"key": "r2m"})


def probe_vector(name, nonce):
    body = header(T_PROBE, 0) + struct.pack("<Q", nonce)
    return vec(name, "probe", body + tag(K_M2R, body), {"nonce": nonce}, {"key": "m2r"})


def name_field(name: str) -> bytes:
    raw = name.encode("utf-8")
    return bytes([len(raw)]) + raw


def beacon_vector(name, port, rid, host):
    data = header(T_BEACON, 0) + struct.pack("<H", port) + rid + name_field(host)
    return vec(name, "beacon", data, {"port": port, "receiver_id": rid.hex(), "host": host})


MAC_ID = bytes(range(0xA0, 0xB0))
RECV_ID = bytes(range(0xC0, 0xD0))
MAC_PUB = bytes(range(0x00, 0x20))
RECV_PUB = bytes(range(0x20, 0x40))

commit = header(T_COMMIT, 0) + MAC_ID + hashlib.sha256(MAC_PUB).digest() + name_field("Studio Mac")
response = header(T_RESPONSE, 0) + RECV_ID + RECV_PUB + name_field("GAMING-PC")
reveal = header(T_REVEAL, 0) + MAC_PUB

# Pairing key schedule with a fixed "shared secret" (X25519 itself is covered
# by the live handshake test between CryptoKit and x25519-dalek).
SHARED = bytes(range(0x80, 0xA0))
TH = hashlib.sha256(commit + response + reveal).digest()
okm = hkdf(SHARED, TH, b"tightwire v2 pairing", 64)
PAIR_ROOT, K_CONFIRM = okm[:32], okm[32:]
SAS = struct.unpack(">I", hkdf(SHARED, TH, b"tightwire v2 sas", 4))[0] % 1_000_000


def confirm_mac(role):
    return hmac.new(K_CONFIRM, b"tightwire confirm" + bytes([role]) + TH, hashlib.sha256).digest()[:16]


confirm_mac0 = header(T_CONFIRM, 0) + bytes([0]) + confirm_mac(0)
confirm_rcv1 = header(T_CONFIRM, 0) + bytes([1]) + confirm_mac(1)
abort = header(T_ABORT, 0) + bytes([1])

F32_SAMPLES = [0.0, 0.5, -0.5, 1.0, -1.0, 0.25, -0.125, 0.0009765625]


def bus_ids(ids):
    return bytes([len(ids)]) + b"".join(name_field(i) for i in ids)


def offer_vector(name, rid, port, host, ids):
    body = header(T_OFFER, 0) + struct.pack("<IH", rid, port) + name_field(host) + bus_ids(ids)
    return vec(name, "handoff_offer", body + tag(K_M2R, body, NONCE),
               {"request_id": rid, "usbip_port": port, "host": host, "bus_ids": ids}, {"key": "m2r", "nonce": NONCE})


def ack_vector(name, rid, op, items):
    body = header(T_ACK, 0) + struct.pack("<IBB", rid, op, len(items))
    for bus, result, port, msg in items:
        body += name_field(bus) + bytes([result, port]) + name_field(msg)
    return vec(name, "handoff_ack", body + tag(K_R2M, body),
               {"request_id": rid, "op": op,
                "items": [{"bus_id": b, "result": r, "port": p, "message": m} for b, r, p, m in items]},
               {"key": "r2m"})


def release_vector(name, rid, ids):
    body = header(T_RELEASE, 0) + struct.pack("<I", rid) + bus_ids(ids)
    return vec(name, "handoff_release", body + tag(K_M2R, body, NONCE), {"request_id": rid, "bus_ids": ids},
               {"key": "m2r", "nonce": NONCE})


def deck_request_vector(name, rid, ms, target):
    body = header(T_DECK_REQ, 0) + struct.pack("<IQB", rid, ms, target)
    return vec(name, "deck_request", body + tag(K_R2M, body), {"request_id": rid, "sent_at_ms": ms, "target": target},
               {"key": "r2m"})


def deck_ack_vector(name, rid, accepted, owner, msg):
    body = header(T_DECK_ACK, 0) + struct.pack("<IBB", rid, accepted, owner) + name_field(msg)
    return vec(name, "deck_ack", body + tag(K_M2R, body),
               {"request_id": rid, "accepted": bool(accepted), "owner": owner, "message": msg}, {"key": "m2r"})


vectors = [
    audio_vector("audio_f32_mono", 0x1234, 7, 336, 48000, 1, "f32", 0, F32_SAMPLES),
    audio_vector("audio_f32_stereo_discontinuity", 0xBEEF, 0, 0, 48000, 2, "f32", 1, F32_SAMPLES),
    audio_vector("audio_f32_seq_wrap_44k1", 0xFFFF, 0xFFFFFFFF, 0x0000_0001_0000_0000, 44100, 1, "f32", 0, [0.75, -0.75]),
    audio_vector("audio_s24_stereo_96k", 0x0001, 42, 96000, 96000, 2, "s24", 0, [0, 8388607, -8388608, 1, -1, 4194304]),
    audio_vector("audio_s16_mono_88k2", 0x00FF, 1000, 123456789, 88200, 1, "s16", 0, [0, 32767, -32768, 1, -1, 16384]),
    audio_vector("audio_f32_8ch", 0x4242, 3, 144, 48000, 8, "f32", 0, F32_SAMPLES),
    stats_vector("stats_typical", 0x1234, 2500, 1000, 3, 1, -42, 0, 5, 999, NONCE),
    stats_vector("stats_extremes", 0xFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0, 0xFFFFFFFF, -2147483648, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFFFFFFFFFF),
    hello_vector("hello_unsolicited", NONCE, 0),
    hello_vector("hello_echo", NONCE, 0xFEDCBA9876543210),
    probe_vector("probe", 0x1122334455667788),
    beacon_vector("beacon_ascii", 50123, RECV_ID, "GAMING-PC"),
    beacon_vector("beacon_utf8", 4810, RECV_ID, "Stúdio-PC ✓"),
    beacon_vector("beacon_empty_name", 4810, bytes(16), ""),
    vec("pair_commit", "commit", commit, {"id": MAC_ID.hex(), "commitment": hashlib.sha256(MAC_PUB).hexdigest(), "name": "Studio Mac"}),
    vec("pair_response", "response", response, {"id": RECV_ID.hex(), "public_key": RECV_PUB.hex(), "name": "GAMING-PC"}),
    vec("pair_reveal", "reveal", reveal, {"public_key": MAC_PUB.hex()}),
    vec("pair_confirm_mac", "confirm", confirm_mac0, {"role": 0, "mac": confirm_mac(0).hex()}),
    vec("pair_confirm_receiver", "confirm", confirm_rcv1, {"role": 1, "mac": confirm_mac(1).hex()}),
    vec("pair_abort", "abort", abort, {"reason": 1}),
    offer_vector("handoff_offer_two", 0xA1B2C3D4, 3240, "", ["1-1", "1-4"]),
    offer_vector("handoff_offer_host", 7, 3240, "192.168.1.10", ["1-2.3"]),
    ack_vector("handoff_ack_attach", 0xA1B2C3D4, 1, [("1-1", 0, 3, ""), ("1-4", 1, 0, "usbip: cannot connect")]),
    ack_vector("handoff_ack_detach_missing", 9, 2, [("1-2", 2, 0, "usbip.exe not found")]),
    release_vector("handoff_release", 0x01020304, ["1-1", "1-4"]),
    deck_request_vector("deck_request_toggle", 0xCAFEBABE, 1_762_960_000_123, 2),
    deck_ack_vector("deck_ack_pc", 0xCAFEBABE, 1, 1, "Sending 1 device to GAMING-PC"),
    # Appended in 0.2.0 so the indices above stay stable.
    stats_vector("stats_extended", 0x1234, 2650, 120334, 2, 0, -31, 1, 0, 120330, NONCE, ext={
        "output_mode": 2, "stream_channels": 2, "output_channels": 2, "target_us": 2600,
        "jitter_mean_us": 120, "jitter_peak_us": 640, "min_target_us": 0, "max_target_us": 20000,
        "output_period_frames": 480, "output_rate": 48000, "stream_rate": 96000, "drift_ppb": -31250,
        "duplicates": 0, "concealed_frames": 96, "resets": 1, "ring_fill": 1, "ring_capacity": 512,
        "ring_full": 0, "marker_timestamp": 4_800_048, "marker_output_us": 123_456_789_012,
        "receiver_now_us": 123_457_000_000, "receiver_ipv4": "192.168.50.20"}),
    hello_vector("hello_extended", NONCE, 0xFEDCBA9876543210, (1, "192.168.50.20", 987_654_321_000)),
    tuning_set_vector("tuning_set_floor", 0x0BADF00D, 0, 4000, 12000),
    tuning_set_vector("tuning_set_adaptive", 2, 1, 0, 20000),
    tuning_ack_vector("tuning_ack_applied", 0x0BADF00D, 0, 4000, 12000, ""),
    tuning_ack_vector("tuning_ack_rejected", 3, 1, 0, 20000, "cap 1.0 ms is outside 2–100 ms"),
    # Appended in 0.2.1: revision 2, with the output health block. The
    # first is a PC with no output open (the Mac's "PC is not playing").
    stats_vector("stats_health_no_output", 0x2345, 0, 5000, 0, 0, 0, 0, 0, 4990, NONCE, ext={
        "output_mode": 0, "stream_channels": 0, "output_channels": 2, "target_us": 0,
        "jitter_mean_us": 0, "jitter_peak_us": 0, "min_target_us": 0, "max_target_us": 20000,
        "output_period_frames": 480, "output_rate": 48000, "stream_rate": 0, "drift_ppb": 0,
        "duplicates": 0, "concealed_frames": 0, "resets": 0, "ring_fill": 512, "ring_capacity": 512,
        "ring_full": 4478, "marker_timestamp": 0, "marker_output_us": 0,
        "receiver_now_us": 50_000_000, "receiver_ipv4": "192.0.2.20",
        "health": {"flags": 0, "device_queue_us": 0, "device_rate_ppb": 0, "device_lead_us": 0,
                   "trims": 0, "skips": 0, "inserts": 0}}),
    stats_vector("stats_health_saturated", 0x2345, 9100, 90000, 0, 0, 1000, 37, 0, 89990, NONCE, ext={
        "output_mode": 1, "stream_channels": 2, "output_channels": 2, "target_us": 9050,
        "jitter_mean_us": 10, "jitter_peak_us": 2950, "min_target_us": 0, "max_target_us": 20000,
        "output_period_frames": 144, "output_rate": 48000, "stream_rate": 48000, "drift_ppb": 1_000_000,
        "duplicates": 0, "concealed_frames": 0, "resets": 1, "ring_fill": 0, "ring_capacity": 512,
        "ring_full": 0, "marker_timestamp": 0, "marker_output_us": 0,
        "receiver_now_us": 240_000_000, "receiver_ipv4": "192.0.2.20",
        "health": {"flags": 0b1111, "device_queue_us": 3000, "device_rate_ppb": -2_150_000,
                   "device_lead_us": 5_250, "trims": 3, "skips": 12, "inserts": 4}}),
]

good = bytes.fromhex(vectors[0]["hex"])
good_body = good[:-8]


def retag(body):
    return body + tag(K_M2R, body, NONCE)


nine_ch = audio_body(1, 0, 0, 48000, 1, "f32", 0, [0.0] * 9)
vectors += [
    {"name": n, "kind": "invalid", "hex": d.hex(), "reason": r}
    for n, d, r in [
        ("bad_magic", b"TWIX" + good[4:], "magic"),
        ("v1_packet", good[:4] + b"\x01" + good[5:], "version"),
        ("unknown_type", good[:5] + b"\x06" + good[6:], "type"),
        ("reserved_control_type", good[:5] + b"\x1f" + good[6:], "type"),
        ("truncated_header", good[:7], "length"),
        ("truncated_audio_header", good[:23], "length"),
        ("audio_no_payload", retag(good_body[:24]), "length"),
        ("audio_partial_frame", retag(good_body[:24 + 6]), "length"),
        ("audio_zero_channels", retag(good_body[:21] + b"\x00" + good_body[22:]), "channels"),
        ("audio_nine_channels", retag(nine_ch[:21] + b"\x09" + nine_ch[22:]), "channels"),
        ("audio_bad_rate", retag(good_body[:20] + b"\x07" + good_body[21:]), "sample_rate"),
        ("audio_bad_format", retag(good_body[:22] + b"\x03" + good_body[23:]), "format"),
        ("audio_payload_too_large", retag(audio_body(1, 0, 0, 48000, 1, "f32", 0, [0.0] * 351)), "length"),
        ("stats_truncated", bytes.fromhex(vectors[6]["hex"])[:40], "length"),
        ("beacon_name_overruns", bytes.fromhex(vectors[11]["hex"])[:-1], "length"),
        ("beacon_bad_utf8", header(T_BEACON, 0) + struct.pack("<H", 4810) + bytes(16) + b"\x02\xff\xfe", "utf8"),
        ("commit_truncated", commit[:40], "length"),
        ("confirm_bad_role", header(T_CONFIRM, 0) + bytes([2]) + confirm_mac(0), "field"),
        ("offer_bad_busid", (lambda b: b + tag(K_M2R, b, NONCE))(header(T_OFFER, 0) + struct.pack("<IH", 1, 3240) + name_field("") + bus_ids(["1 1"])), "field"),
        ("deck_request_bad_target", (lambda b: b + tag(K_R2M, b))(header(T_DECK_REQ, 0) + struct.pack("<IQB", 1, 2, 3)), "field"),
        ("beacon_name_bidi_override", header(T_BEACON, 0) + struct.pack("<H", 4810) + bytes(16) + name_field("PC\u202egnp.exe"), "field"),
        ("beacon_name_newline", header(T_BEACON, 0) + struct.pack("<H", 4810) + bytes(16) + name_field("PC\ncode: 123 456"), "field"),
        ("commit_name_too_long", header(T_COMMIT, 0) + MAC_ID + hashlib.sha256(MAC_PUB).digest() + name_field("M" * 41), "field"),
        ("response_name_zero_width", header(T_RESPONSE, 0) + RECV_ID + RECV_PUB + name_field("GAMING\u200bPC"), "field"),
        ("offer_busid_letters", (lambda b: b + tag(K_M2R, b, NONCE))(header(T_OFFER, 0) + struct.pack("<IH", 1, 3240) + name_field("192.168.50.194") + bus_ids(["1-a"])), "field"),
        ("tuning_set_bad_flag", (lambda b: b + tag(K_M2R, b, NONCE))(header(T_TUNING_SET, 0) + struct.pack("<IBII", 1, 2, 0, 20000)), "field"),
        ("tuning_set_truncated", (lambda b: b + tag(K_M2R, b, NONCE))(header(T_TUNING_SET, 0) + struct.pack("<IBI", 1, 0, 0)), "length"),
        ("ack_truncated_item", (lambda b: b + tag(K_R2M, b))(header(T_ACK, 0) + struct.pack("<IBB", 1, 1, 2) + name_field("1-1") + bytes([0, 1]) + name_field("")), "length"),
    ]
]

# Structurally valid packets whose tag must fail verification.
bad_tag = [
    ("audio_tag_flipped", good[:-1] + bytes([good[-1] ^ 1]), "m2r", NONCE),
    ("audio_payload_tampered", good[:30] + bytes([good[30] ^ 0x80]) + good[31:], "m2r", NONCE),
    ("audio_wrong_nonce", good, "m2r", NONCE + 1),
    ("audio_reflected_key", good_body + tag(K_R2M, good_body, NONCE), "m2r", NONCE),
    ("hello_signed_with_m2r", bytes.fromhex(vectors[8]["hex"])[:-8] + tag(K_M2R, bytes.fromhex(vectors[8]["hex"])[:-8]), "r2m", None),
    ("probe_zero_tag", bytes.fromhex(vectors[10]["hex"])[:-8] + bytes(8), "m2r", None),
]
vectors += [
    {"name": n, "kind": "bad_tag", "hex": d.hex(), "auth": {"key": k, **({"nonce": nonce} if nonce is not None else {})}}
    for n, d, k, nonce in bad_tag
]

doc = {
    "version": VERSION,
    "keys": {
        "root": ROOT.hex(),
        "m2r": K_M2R.hex(),
        "r2m": K_R2M.hex(),
    },
    "pairing": {
        "shared_secret": SHARED.hex(),
        "transcript_hash": TH.hex(),
        "root": PAIR_ROOT.hex(),
        "confirm_key": K_CONFIRM.hex(),
        "sas": f"{SAS:06d}",
        "commit": commit.hex(),
        "response": response.hex(),
        "reveal": reveal.hex(),
    },
    "vectors": vectors,
}
out = HERE / "vectors" / "vectors.json"
out.write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n")
print(f"wrote {len(vectors)} vectors to {out}; SipHash reference vectors OK")

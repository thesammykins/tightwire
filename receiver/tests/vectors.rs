//! SPEC §7.1: every golden vector decodes to its listed fields, verifies (or
//! fails to verify) under the listed key, and re-encodes to identical bytes.

use serde_json::Value;
use tightwire_recv::auth::{self, PairingSecrets, SessionKeys, SipKey};
use tightwire_recv::protocol::*;

fn doc() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../protocol/vectors/vectors.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hexa<const N: usize>(s: &str) -> [u8; N] {
    hex(s).try_into().unwrap()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect()
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap()
}

struct Ctx {
    keys: SessionKeys,
}

impl Ctx {
    fn key(&self, auth: &Value) -> (SipKey, Option<u64>) {
        let k = match auth["key"].as_str().unwrap() {
            "m2r" => self.keys.m2r,
            "r2m" => self.keys.r2m,
            k => panic!("unknown key {k}"),
        };
        (k, auth.get("nonce").map(u))
    }
}

#[test]
fn siphash_reference_vectors() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../protocol/vectors/siphash.json"
    );
    let d: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let key: SipKey = hexa(d["key"].as_str().unwrap());
    for v in d["vectors"].as_array().unwrap() {
        let msg = hex(v["message"].as_str().unwrap());
        let want = hex(v["tag"].as_str().unwrap());
        assert_eq!(
            auth::siphash24(&key, &[&msg]).to_vec(),
            want,
            "len {}",
            msg.len()
        );
        // Split input at every point: streaming must not change the result.
        for cut in 0..=msg.len() {
            let (a, b) = msg.split_at(cut);
            assert_eq!(auth::siphash24(&key, &[a, b]).to_vec(), want);
        }
    }
}

#[test]
fn siphash_matches_siphasher_crate() {
    use std::hash::Hasher;
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for len in 0..300 {
        let mut msg = vec![0u8; len];
        for b in &mut msg {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let key: SipKey = std::array::from_fn(|i| (x >> (i % 8)) as u8 ^ i as u8);
        let mut h = siphasher::sip::SipHasher24::new_with_key(&key);
        h.write(&msg);
        assert_eq!(auth::siphash24(&key, &[&msg]), h.finish().to_le_bytes());
    }
}

#[test]
fn key_schedule() {
    let d = doc();
    let keys = SessionKeys::from_root(&hexa(d["keys"]["root"].as_str().unwrap()));
    assert_eq!(keys.m2r.to_vec(), hex(d["keys"]["m2r"].as_str().unwrap()));
    assert_eq!(keys.r2m.to_vec(), hex(d["keys"]["r2m"].as_str().unwrap()));

    let p = &d["pairing"];
    let s = PairingSecrets::derive(
        &hexa(p["shared_secret"].as_str().unwrap()),
        &hex(p["commit"].as_str().unwrap()),
        &hex(p["response"].as_str().unwrap()),
        &hex(p["reveal"].as_str().unwrap()),
    );
    assert_eq!(
        s.transcript_hash.to_vec(),
        hex(p["transcript_hash"].as_str().unwrap())
    );
    assert_eq!(s.root.to_vec(), hex(p["root"].as_str().unwrap()));
    assert_eq!(
        s.confirm_key.to_vec(),
        hex(p["confirm_key"].as_str().unwrap())
    );
    assert_eq!(format!("{:06}", s.sas), p["sas"].as_str().unwrap());
    for v in d["vectors"].as_array().unwrap() {
        if v["kind"] == "confirm" {
            let role = u(&v["fields"]["role"]) as u8;
            assert_eq!(
                s.confirm_mac(role).to_vec(),
                hex(v["fields"]["mac"].as_str().unwrap())
            );
        }
    }
}

#[test]
fn golden_vectors_round_trip() {
    let d = doc();
    let ctx = Ctx {
        keys: SessionKeys::from_root(&hexa(d["keys"]["root"].as_str().unwrap())),
    };
    let mut checked = 0;
    for v in d["vectors"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let bytes = hex(v["hex"].as_str().unwrap());
        let f = &v["fields"];
        let kind = v["kind"].as_str().unwrap();
        if kind == "invalid" {
            let err = decode(&bytes)
                .err()
                .unwrap_or_else(|| panic!("{name}: accepted invalid packet"));
            let want = match v["reason"].as_str().unwrap() {
                "magic" => DecodeError::Magic,
                "version" => DecodeError::Version,
                "type" => DecodeError::Type,
                "length" => DecodeError::Length,
                "channels" => DecodeError::Channels,
                "sample_rate" => DecodeError::SampleRate,
                "format" => DecodeError::Format,
                "utf8" => DecodeError::Utf8,
                "field" => DecodeError::Field,
                r => panic!("unknown reason {r}"),
            };
            assert_eq!(err, want, "{name}");
            checked += 1;
            continue;
        }
        if kind == "bad_tag" {
            decode(&bytes).unwrap_or_else(|e| panic!("{name}: should decode, got {e}"));
            let (k, n) = ctx.key(&v["auth"]);
            assert!(!auth::verify(&k, n, &bytes), "{name}: forged tag accepted");
            checked += 1;
            continue;
        }
        let sealed = |body: Vec<u8>| {
            let (k, n) = ctx.key(&v["auth"]);
            assert!(auth::verify(&k, n, &bytes), "{name}: tag rejected");
            auth::seal(&k, n, body)
        };
        let re = match decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}")) {
            Packet::Audio(p) => {
                let h = p.header;
                assert_eq!(h.stream_id as u64, u(&f["stream_id"]), "{name}");
                assert_eq!(h.sequence as u64, u(&f["sequence"]), "{name}");
                assert_eq!(h.timestamp, u(&f["timestamp"]), "{name}");
                assert_eq!(h.sample_rate as u64, u(&f["sample_rate"]), "{name}");
                assert_eq!(h.channels as u64, u(&f["channels"]), "{name}");
                assert_eq!(h.format.name(), f["format"].as_str().unwrap(), "{name}");
                assert_eq!(h.flags as u64, u(&f["flags"]), "{name}");
                let expect: Vec<f32> = f["samples_f32"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_f64().unwrap() as f32)
                    .collect();
                let mut buf = [0f32; MAX_SAMPLES];
                let n = p.samples_to_f32(&mut buf);
                assert_eq!(&buf[..n], &expect[..], "{name}");
                assert_eq!(p.frames() * h.channels as usize, n, "{name}");
                sealed(encode_audio(&h, &encode_samples(h.format, &buf[..n])))
            }
            Packet::Stats(s) => {
                assert_eq!(s.stream_id as u64, u(&f["stream_id"]));
                assert_eq!(s.depth_us as u64, u(&f["depth_us"]));
                assert_eq!(s.received as u64, u(&f["received"]));
                assert_eq!(s.lost as u64, u(&f["lost"]));
                assert_eq!(s.late as u64, u(&f["late"]));
                assert_eq!(s.drift_ppm as i64, f["drift_ppm"].as_i64().unwrap());
                assert_eq!(s.underruns as u64, u(&f["underruns"]));
                assert_eq!(s.auth_dropped as u64, u(&f["auth_dropped"]));
                assert_eq!(s.echo_seq as u64, u(&f["echo_seq"]));
                assert_eq!(s.session_nonce, u(&f["session_nonce"]));
                match (&s.ext, f.get("ext")) {
                    (None, None) => {}
                    (Some(e), Some(x)) => {
                        assert_eq!(e.output_mode as u64, u(&x["output_mode"]));
                        assert_eq!(e.stream_channels as u64, u(&x["stream_channels"]));
                        assert_eq!(e.output_channels as u64, u(&x["output_channels"]));
                        assert_eq!(e.target_us as u64, u(&x["target_us"]));
                        assert_eq!(e.jitter_mean_us as u64, u(&x["jitter_mean_us"]));
                        assert_eq!(e.jitter_peak_us as u64, u(&x["jitter_peak_us"]));
                        assert_eq!(e.min_target_us as u64, u(&x["min_target_us"]));
                        assert_eq!(e.max_target_us as u64, u(&x["max_target_us"]));
                        assert_eq!(e.output_period_frames as u64, u(&x["output_period_frames"]));
                        assert_eq!(e.output_rate as u64, u(&x["output_rate"]));
                        assert_eq!(e.stream_rate as u64, u(&x["stream_rate"]));
                        assert_eq!(e.drift_ppb as i64, x["drift_ppb"].as_i64().unwrap());
                        assert_eq!(e.duplicates as u64, u(&x["duplicates"]));
                        assert_eq!(e.concealed_frames as u64, u(&x["concealed_frames"]));
                        assert_eq!(e.resets as u64, u(&x["resets"]));
                        assert_eq!(e.ring_fill as u64, u(&x["ring_fill"]));
                        assert_eq!(e.ring_capacity as u64, u(&x["ring_capacity"]));
                        assert_eq!(e.ring_full as u64, u(&x["ring_full"]));
                        assert_eq!(e.marker_timestamp, u(&x["marker_timestamp"]));
                        assert_eq!(e.marker_output_us, u(&x["marker_output_us"]));
                        assert_eq!(e.receiver_now_us, u(&x["receiver_now_us"]));
                        assert_eq!(
                            std::net::Ipv4Addr::from(e.receiver_ipv4).to_string(),
                            x["receiver_ipv4"].as_str().unwrap()
                        );
                    }
                    (a, b) => panic!("{name}: extension mismatch {a:?} vs {b:?}"),
                }
                sealed(encode_stats(&s))
            }
            Packet::Hello(h) => {
                assert_eq!(h.session_nonce, u(&f["session_nonce"]));
                assert_eq!(h.echo, u(&f["echo"]));
                match (&h.ext, f.get("ext")) {
                    (None, None) => {}
                    (Some(e), Some(x)) => {
                        assert_eq!(e.revision as u64, u(&x["revision"]));
                        assert_eq!(
                            std::net::Ipv4Addr::from(e.receiver_ipv4).to_string(),
                            x["receiver_ipv4"].as_str().unwrap()
                        );
                        assert_eq!(e.receiver_time_us, u(&x["receiver_time_us"]));
                    }
                    (a, b) => panic!("{name}: hello extension mismatch {a:?} vs {b:?}"),
                }
                sealed(encode_hello(&h))
            }
            Packet::Probe { nonce } => {
                assert_eq!(nonce, u(&f["nonce"]));
                sealed(encode_probe(nonce))
            }
            Packet::Beacon(b) => {
                assert_eq!(b.port as u64, u(&f["port"]));
                assert_eq!(
                    auth::to_hex(&b.receiver_id),
                    f["receiver_id"].as_str().unwrap()
                );
                assert_eq!(b.host, f["host"].as_str().unwrap());
                encode_beacon(&b)
            }
            Packet::Commit(c) => {
                assert_eq!(auth::to_hex(&c.id), f["id"].as_str().unwrap());
                assert_eq!(
                    auth::to_hex(&c.commitment),
                    f["commitment"].as_str().unwrap()
                );
                assert_eq!(c.name, f["name"].as_str().unwrap());
                encode_commit(&c)
            }
            Packet::Response(r) => {
                assert_eq!(auth::to_hex(&r.id), f["id"].as_str().unwrap());
                assert_eq!(
                    auth::to_hex(&r.public_key),
                    f["public_key"].as_str().unwrap()
                );
                assert_eq!(r.name, f["name"].as_str().unwrap());
                encode_response(&r)
            }
            Packet::Reveal { public_key } => {
                assert_eq!(auth::to_hex(&public_key), f["public_key"].as_str().unwrap());
                encode_reveal(&public_key)
            }
            Packet::Confirm(c) => {
                assert_eq!(c.role as u64, u(&f["role"]));
                assert_eq!(auth::to_hex(&c.mac), f["mac"].as_str().unwrap());
                encode_confirm(&c)
            }
            Packet::Abort { reason } => {
                assert_eq!(reason as u64, u(&f["reason"]));
                encode_abort(reason)
            }
            Packet::HandoffOffer(o) => {
                assert_eq!(o.request_id as u64, u(&f["request_id"]));
                assert_eq!(o.usbip_port as u64, u(&f["usbip_port"]));
                assert_eq!(o.host, f["host"].as_str().unwrap());
                assert_eq!(o.bus_ids, strs(&f["bus_ids"]));
                sealed(encode_handoff_offer(&o))
            }
            Packet::HandoffAck(a) => {
                assert_eq!(a.request_id as u64, u(&f["request_id"]));
                assert_eq!(a.op as u64, u(&f["op"]));
                let items = f["items"].as_array().unwrap();
                assert_eq!(a.items.len(), items.len());
                for (i, j) in a.items.iter().zip(items) {
                    assert_eq!(i.bus_id, j["bus_id"].as_str().unwrap());
                    assert_eq!(i.result as u64, u(&j["result"]));
                    assert_eq!(i.port as u64, u(&j["port"]));
                    assert_eq!(i.message, j["message"].as_str().unwrap());
                }
                sealed(encode_handoff_ack(&a))
            }
            Packet::HandoffRelease {
                request_id,
                bus_ids,
            } => {
                assert_eq!(request_id as u64, u(&f["request_id"]));
                assert_eq!(bus_ids, strs(&f["bus_ids"]));
                sealed(encode_handoff_release(request_id, &bus_ids))
            }
            Packet::DeckRequest(r) => {
                assert_eq!(r.request_id as u64, u(&f["request_id"]));
                assert_eq!(r.sent_at_ms, u(&f["sent_at_ms"]));
                assert_eq!(r.target as u64, u(&f["target"]));
                sealed(encode_deck_request(&r))
            }
            Packet::DeckAck(a) => {
                assert_eq!(a.request_id as u64, u(&f["request_id"]));
                assert_eq!(a.accepted, f["accepted"].as_bool().unwrap());
                assert_eq!(a.owner as u64, u(&f["owner"]));
                assert_eq!(a.message, f["message"].as_str().unwrap());
                sealed(encode_deck_ack(&a))
            }
            Packet::TuningSet(t) => {
                assert_eq!(t.request_id as u64, u(&f["request_id"]));
                assert_eq!(t.adaptive, f["adaptive"].as_bool().unwrap());
                assert_eq!(t.min_target_us as u64, u(&f["min_target_us"]));
                assert_eq!(t.max_target_us as u64, u(&f["max_target_us"]));
                sealed(encode_tuning_set(&t))
            }
            Packet::TuningAck(a) => {
                assert_eq!(a.request_id as u64, u(&f["request_id"]));
                assert_eq!(a.result as u64, u(&f["result"]));
                assert_eq!(a.min_target_us as u64, u(&f["min_target_us"]));
                assert_eq!(a.max_target_us as u64, u(&f["max_target_us"]));
                assert_eq!(a.message, f["message"].as_str().unwrap());
                sealed(encode_tuning_ack(&a))
            }
        };
        assert_eq!(re, bytes, "{name}: re-encode differs");
        checked += 1;
    }
    assert!(checked >= 50, "only {checked} vectors");
}

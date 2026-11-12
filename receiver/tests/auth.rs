//! SPEC §7.1a (receiver side): admission drops unauthenticated, tampered,
//! replayed and unpaired traffic; the pairing responder completes a handshake
//! with a correct initiator and refuses a key that breaks its commitment.

use std::net::SocketAddr;

use tightwire_recv::auth::{self, PairingSecrets, SessionKeys};
use tightwire_recv::ingress::{Action, Ingress};
use tightwire_recv::pairing::{PairEvent, Responder};
use tightwire_recv::protocol::{self, AudioHeader, Packet, PairCommit, PairConfirm, SampleFormat};
use x25519_dalek::{EphemeralSecret, PublicKey};

const NONCE: u64 = 0xdead_beef_1234_5678;

fn keys() -> SessionKeys {
    SessionKeys::from_root(&[7u8; 32])
}

fn mac() -> SocketAddr {
    "192.168.1.20:4812".parse().unwrap()
}

fn audio(stream: u16, seq: u32, key: &[u8; 16], nonce: u64) -> Vec<u8> {
    let h = AudioHeader {
        stream_id: stream,
        sequence: seq,
        timestamp: seq as u64 * 48,
        sample_rate: 48_000,
        channels: 1,
        format: SampleFormat::F32,
        flags: 0,
    };
    let body = protocol::encode_audio(
        &h,
        &protocol::encode_samples(SampleFormat::F32, &[0.25; 48]),
    );
    auth::seal(key, Some(nonce), body)
}

fn is_audio(a: &Action) -> bool {
    matches!(a, Action::Audio(_))
}

#[test]
fn accepts_authentic_audio() {
    let k = keys();
    let mut ing = Ingress::new(Some(k), NONCE, None);
    assert!(is_audio(&ing.handle(&audio(1, 0, &k.m2r, NONCE), mac())));
    assert_eq!(ing.peer(), Some(mac()));
    assert_eq!(ing.counters.auth_dropped, 0);
}

#[test]
fn drops_unauthenticated_and_tampered() {
    let k = keys();
    let mut ing = Ingress::new(Some(k), NONCE, None);
    let good = audio(1, 0, &k.m2r, NONCE);

    let mut tampered = good.clone();
    tampered[30] ^= 0x01;
    let mut bad_tag = good.clone();
    *bad_tag.last_mut().unwrap() ^= 0x80;
    let untagged = &good[..good.len() - 8];
    let wrong_key = audio(1, 0, &SessionKeys::from_root(&[8u8; 32]).m2r, NONCE);
    let reflected = audio(1, 0, &k.r2m, NONCE);
    let old_session = audio(1, 0, &k.m2r, NONCE ^ 1);
    let mut v1 = good.clone();
    v1[4] = 1;

    for (name, pkt) in [
        ("tampered", &tampered[..]),
        ("bad tag", &bad_tag[..]),
        ("no tag", untagged),
        ("wrong key", &wrong_key[..]),
        ("reflected direction key", &reflected[..]),
        ("previous receiver session", &old_session[..]),
        ("v1 packet", &v1[..]),
        ("garbage", b"hello world"),
    ] {
        assert!(!is_audio(&ing.handle(pkt, mac())), "{name} accepted");
    }
    assert_eq!(ing.counters.auth_dropped, 8);
    assert_eq!(
        ing.peer(),
        None,
        "unauthenticated packets must not set the peer"
    );
}

#[test]
fn unpaired_receiver_accepts_nothing() {
    let k = keys();
    let mut ing = Ingress::new(None, NONCE, None);
    assert!(!is_audio(&ing.handle(&audio(1, 0, &k.m2r, NONCE), mac())));
    let probe = auth::seal(&k.m2r, None, protocol::encode_probe(1));
    assert!(matches!(ing.handle(&probe, mac()), Action::Drop));
}

#[test]
fn retired_stream_cannot_be_replayed() {
    let k = keys();
    let mut ing = Ingress::new(Some(k), NONCE, None);
    let old = audio(1, 5, &k.m2r, NONCE);
    assert!(is_audio(&ing.handle(&old, mac())));
    assert!(is_audio(&ing.handle(&audio(2, 0, &k.m2r, NONCE), mac())));
    // Captured packet from stream 1 replayed after stream 2 started.
    assert!(!is_audio(&ing.handle(&old, mac())));
    assert_eq!(ing.counters.replayed, 1);
    assert_eq!(ing.current_stream(), Some(2));
}

#[test]
fn allow_filter() {
    let k = keys();
    let mut ing = Ingress::new(Some(k), NONCE, Some("192.168.1.99".parse().unwrap()));
    assert!(!is_audio(&ing.handle(&audio(1, 0, &k.m2r, NONCE), mac())));
    assert_eq!(ing.counters.filtered, 1);
}

#[test]
fn probe_gets_authenticated_hello_echo() {
    let k = keys();
    let mut ing = Ingress::new(Some(k), NONCE, None);
    let probe = auth::seal(&k.m2r, None, protocol::encode_probe(0xabcdef));
    let Action::Reply(hello, to) = ing.handle(&probe, mac()) else {
        panic!("no hello")
    };
    assert_eq!(to, mac());
    assert!(auth::verify(&k.r2m, None, &hello));
    let Ok(Packet::Hello(h)) = protocol::decode(&hello) else {
        panic!()
    };
    assert_eq!(h.echo, 0xabcdef);
    assert_eq!(h.session_nonce, NONCE);
    // 0.2.0 hellos carry the receiver clock for the Mac's clock sync.
    let ext = h.ext.expect("hello extension");
    assert_eq!(ext.revision, protocol::PROTOCOL_REVISION);
    // Toward 192.168.1.20 this host may have no route; the field is then 0.0.0.0.
    let _ = ext.receiver_ipv4;
    // A probe tagged with the wrong direction's key is ignored.
    let forged = auth::seal(&k.r2m, None, protocol::encode_probe(1));
    assert!(matches!(ing.handle(&forged, mac()), Action::Drop));
}

/// Plays the Mac's half of the handshake in Rust.
fn handshake(reveal_matches_commit: bool) -> (Option<PairEvent>, Vec<PairEvent>) {
    let mut r = Responder::new([1u8; 16], "PC".into());
    let secret = EphemeralSecret::random();
    let public = PublicKey::from(&secret).to_bytes();
    let commit = protocol::encode_commit(&PairCommit {
        id: [2u8; 16],
        commitment: auth::sha256(&public),
        name: "Mac".into(),
    });
    let (out, ev) = r.handle(&commit, mac());
    assert!(ev.is_none());
    let response = out[0].0.clone();
    let Ok(Packet::Response(resp)) = protocol::decode(&response) else {
        panic!()
    };
    let revealed = if reveal_matches_commit {
        public
    } else {
        [9u8; 32]
    };
    let reveal = protocol::encode_reveal(&revealed);
    let (_, ev) = r.handle(&reveal, mac());
    let mut events = vec![];
    let Some(PairEvent::ShowCode { code, .. }) = ev else {
        return (ev, events);
    };
    let shared = secret.diffie_hellman(&PublicKey::from(resp.public_key));
    let s = PairingSecrets::derive(shared.as_bytes(), &commit, &response, &reveal);
    assert_eq!(code, s.sas_display(), "both sides must show the same code");
    let (out, ev) = r.user_decision(true);
    assert!(ev.is_none(), "must wait for the Mac's confirm");
    let Ok(Packet::Confirm(c)) = protocol::decode(&out[0].0) else {
        panic!()
    };
    assert_eq!(c.mac, s.confirm_mac(1));
    // A forged confirm does not complete pairing.
    let forged = protocol::encode_confirm(&PairConfirm {
        role: 0,
        mac: [0; 16],
    });
    assert!(r.handle(&forged, mac()).1.is_none());
    let confirm = protocol::encode_confirm(&PairConfirm {
        role: 0,
        mac: s.confirm_mac(0),
    });
    let (_, ev) = r.handle(&confirm, mac());
    if let Some(PairEvent::Paired(rec)) = &ev {
        assert_eq!(rec.root, s.root);
        assert_eq!(rec.peer_id, [2u8; 16]);
        assert_eq!(rec.last_address, Some(mac()));
    }
    events.push(ev.unwrap());
    (None, events)
}

#[test]
fn responder_completes_handshake() {
    let (_, events) = handshake(true);
    assert!(matches!(events[0], PairEvent::Paired(_)));
}

#[test]
fn responder_rejects_key_that_breaks_commitment() {
    let (ev, _) = handshake(false);
    assert!(matches!(ev, Some(PairEvent::Aborted(_))), "{ev:?}");
}

#[test]
fn config_file_round_trip() {
    use tightwire_recv::pairing::{ConfigFile, PairingRecord};
    // System temp dir rather than the target dir: the latter may sit on a
    // volume that ignores Unix permissions.
    let path = std::env::temp_dir().join(format!(
        "tightwire-test-{}/pairing.conf",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let fresh = ConfigFile::load(&path).unwrap();
    assert!(fresh.pairing.is_none());
    let c = ConfigFile {
        self_id: [3u8; 16],
        pairing: Some(PairingRecord {
            peer_name: "Studio Mac\nevil = 1".into(),
            peer_id: [4u8; 16],
            root: [5u8; 32],
            last_address: Some(mac()),
        }),
    };
    c.save(&path).unwrap();
    let back = ConfigFile::load(&path).unwrap();
    assert_eq!(back.self_id, c.self_id);
    let p = back.pairing.unwrap();
    assert_eq!(
        p.peer_name, "Studio Macevil = 1",
        "newlines must not inject keys"
    );
    assert_eq!(p.root, [5u8; 32]);
    assert_eq!(p.last_address, Some(mac()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Type 21 is bound to the receiver session like handoff offers: sealed with
/// m2r and the session nonce, or dropped.
#[test]
fn tuning_request_needs_the_session_nonce() {
    let k = keys();
    let mut ing = Ingress::new(Some(k), NONCE, None);
    let body = protocol::encode_tuning_set(&protocol::TuningSet {
        request_id: 9,
        adaptive: false,
        min_target_us: 4000,
        max_target_us: 12000,
    });
    let good = auth::seal(&k.m2r, Some(NONCE), body.clone());
    let Action::Tuning(m, from) = ing.handle(&good, mac()) else {
        panic!("tuning request not accepted")
    };
    assert_eq!((m.request_id, m.min_target_us, from), (9, 4000, mac()));
    for bad in [
        auth::seal(&k.m2r, None, body.clone()),
        auth::seal(&k.m2r, Some(NONCE + 1), body.clone()),
        auth::seal(&k.r2m, Some(NONCE), body),
    ] {
        assert!(matches!(ing.handle(&bad, mac()), Action::Drop));
    }
}

/// Security: a captured probe replayed from elsewhere is answered but must
/// not become the Mac's address (saved for start-up hellos and deck requests).
#[test]
fn replayed_probe_does_not_move_the_peer_address() {
    let k = keys();
    let mut ing = Ingress::new(Some(k), NONCE, None);
    assert!(is_audio(&ing.handle(&audio(1, 0, &k.m2r, NONCE), mac())));
    let probe = auth::seal(&k.m2r, None, protocol::encode_probe(42));
    let elsewhere: SocketAddr = "10.9.9.9:5555".parse().unwrap();
    assert!(matches!(ing.handle(&probe, elsewhere), Action::Reply(_, to) if to == elsewhere));
    assert_eq!(ing.peer(), Some(mac()));
    assert_eq!(ing.audio_peer(), Some(mac()));
}

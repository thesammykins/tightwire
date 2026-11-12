//! The receiver without an output (no device chosen / chosen device absent)
//! still runs its network side, and `doctor` produces every section.

use std::net::UdpSocket;
use std::time::Duration;

use tightwire_recv::auth::{self, SessionKeys};
use tightwire_recv::net::KeySource;
use tightwire_recv::protocol::{self, Packet};
use tightwire_recv::service::{self, Receiver};

#[test]
fn discard_backend_answers_probes_without_opening_audio() {
    let root = [7u8; 32];
    let keys = SessionKeys::from_root(&root);
    // Pick a free port, then let the receiver bind it.
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let r = Receiver::start(service::Options {
        backend: service::Backend::Discard {
            reason: "no output chosen".into(),
        },
        device: None,
        device_id: None,
        shared: false,
        period: None,
        port,
        bind: "127.0.0.1".parse().unwrap(),
        allow: None,
        target_ms: None,
        max_ms: None,
        keys: KeySource::Psk(keys),
        usbip: None,
    })
    .unwrap();
    assert!(
        r.description
            .starts_with("none, audio discarded (no output chosen)"),
        "{}",
        r.description
    );

    let mac = UdpSocket::bind("127.0.0.1:0").unwrap();
    mac.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let probe = auth::seal(&keys.m2r, None, protocol::encode_probe(42));
    let mut buf = [0u8; 1500];
    let mut got_hello = false;
    // An oversized datagram first: the receiver must drop it and go on.
    mac.send_to(&vec![0x5a; 9000], ("127.0.0.1", port)).unwrap();
    for _ in 0..6 {
        mac.send_to(&probe, ("127.0.0.1", port)).unwrap();
        if let Ok((n, _)) = mac.recv_from(&mut buf)
            && auth::verify(&keys.r2m, None, &buf[..n])
            && matches!(protocol::decode(&buf[..n]), Ok(Packet::Hello(h)) if h.echo == 42)
        {
            got_hello = true;
            break;
        }
    }
    assert!(
        got_hello,
        "receiver without output must still answer probes"
    );
    // Probes carry no session nonce (replayable), so they do not count as
    // proof the Mac is there for the tray/doctor, nor move its address.
    std::thread::sleep(Duration::from_millis(1100));
    assert!(r.net_status().last_auth.is_none());
    assert!(r.net_status().peer.is_none());
    assert_eq!(r.failed(), None);
    r.stop().unwrap();
}

#[test]
fn doctor_report_has_every_section() {
    let dir = std::env::temp_dir().join(format!("tw-doctor-{}", std::process::id()));
    let report = tightwire_recv::doctor::report(&dir.join("pairing.conf"), None);
    for s in [
        "doctor (",
        "[pairing]",
        "NOT PAIRED",
        "[output]",
        "[network]",
        "[stream deck handoff]",
        "TCP 3240 test skipped",
    ] {
        assert!(report.contains(s), "missing {s:?} in:\n{report}");
    }
    assert!(report.starts_with(&tightwire_recv::applog::build_label()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Security: a datagram larger than any Tightwire packet (and, on Windows,
/// one that would raise WSAEMSGSIZE with a small buffer) is dropped and the
/// receiver keeps answering.
#[test]
fn oversized_datagram_is_dropped_and_the_receiver_keeps_going() {
    use tightwire_recv::net::{RecvError, classify_recv_error};
    assert_eq!(
        classify_recv_error(&std::io::Error::from_raw_os_error(10040)),
        RecvError::Oversized
    );
    assert_eq!(
        classify_recv_error(&std::io::Error::from(std::io::ErrorKind::WouldBlock)),
        RecvError::Idle
    );
    assert_eq!(
        classify_recv_error(&std::io::Error::other("boom")),
        RecvError::Other
    );
}

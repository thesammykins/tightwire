//! `tightwire-recv deck mac|pc|toggle`: asks the paired Mac to move the
//! handoff devices (SPEC §9). Self-contained: reads the pairing file, sends
//! one authenticated request and waits for the Mac's answer, without talking
//! to a running receiver.
//!
//! The Mac's answer only means it *accepted the request*; the devices move
//! later through a handoff offer/release to the running receiver, which logs
//! its own "handoff:" lines. Wording here keeps those two apart.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::{self, SessionKeys};
use crate::pairing::ConfigFile;
use crate::protocol::{self, DeckAck, DeckRequest, DeckTarget, Packet};

pub fn parse_target(s: &str) -> Option<DeckTarget> {
    match s.to_ascii_lowercase().as_str() {
        "mac" => Some(DeckTarget::Mac),
        "pc" => Some(DeckTarget::Pc),
        "toggle" => Some(DeckTarget::Toggle),
        _ => None,
    }
}

/// "asked the Mac to send devices to this PC": what was requested, never
/// phrased as if it had happened.
pub fn request_phrase(t: DeckTarget) -> &'static str {
    match t {
        DeckTarget::Mac => "asked the Mac to take the devices back",
        DeckTarget::Pc => "asked the Mac to send devices to this PC",
        DeckTarget::Toggle => "asked the Mac to toggle where the devices are",
    }
}

/// One line describing the Mac's answer, for the log, the CLI and the tray.
pub fn describe_ack(t: DeckTarget, a: &DeckAck) -> String {
    format!(
        "{}; the Mac {} (\"{}\"; it reports the devices with the {})",
        request_phrase(t),
        if a.accepted { "accepted" } else { "refused" },
        a.message,
        if a.owner == 1 { "PC" } else { "Mac" }
    )
}

/// Sends the request (three tries, 0.5 s apart) and returns the Mac's ack.
/// Every step is logged (stderr for the CLI, the log file for the tray).
pub fn request(
    config: &Path,
    target: DeckTarget,
    mac: Option<IpAddr>,
    port: u16,
    wait: Duration,
) -> Result<DeckAck, String> {
    let fail = |e: String| {
        crate::twlog!("deck: {}: failed: {e}", request_phrase(target));
        e
    };
    let cfg = ConfigFile::load(config).map_err(fail)?;
    let pairing = cfg
        .pairing
        .ok_or_else(|| fail("not paired: run `tightwire-recv pair` first".into()))?;
    let ip = mac
        .or(pairing.last_address.map(|a| a.ip()))
        .ok_or_else(|| {
            fail(
                "the Mac's address is not known yet (start streaming once, or pass --mac IP)"
                    .into(),
            )
        })?;
    let keys = SessionKeys::from_root(&pairing.root);
    let to = SocketAddr::new(ip, port);
    let sock = UdpSocket::bind((if ip.is_ipv4() { "0.0.0.0" } else { "::" }, 0))
        .map_err(|e| fail(e.to_string()))?;
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| fail(e.to_string()))?;
    let req = DeckRequest {
        request_id: u32::from_le_bytes(auth::random()),
        sent_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| fail(e.to_string()))?
            .as_millis() as u64,
        target,
    };
    crate::twlog!(
        "deck: {} (request #{}, to {to}, waiting up to {:.0} s)",
        request_phrase(target),
        req.request_id,
        wait.as_secs_f64()
    );
    let packet = auth::seal(&keys.r2m, None, protocol::encode_deck_request(&req));
    let deadline = Instant::now() + wait;
    let mut next_send = Instant::now();
    let mut sends = 0;
    let mut ignored = 0;
    let mut buf = vec![0u8; crate::net::RECV_BUFFER];
    while Instant::now() < deadline {
        if sends < 3 && Instant::now() >= next_send {
            if let Err(e) = sock.send_to(&packet, to) {
                return Err(fail(format!("send to {to}: {e}")));
            }
            sends += 1;
            next_send = Instant::now() + Duration::from_millis(500);
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let reason = if from.ip() != ip {
            Some("not from the paired Mac's address")
        } else if !auth::verify(&keys.m2r, None, &buf[..n]) {
            Some("authentication failed (different pairing key?)")
        } else {
            match protocol::decode(&buf[..n]) {
                Ok(Packet::DeckAck(a)) if a.request_id == req.request_id => {
                    crate::twlog!(
                        "deck: request #{}: {}",
                        req.request_id,
                        describe_ack(target, &a)
                    );
                    return Ok(a);
                }
                Ok(Packet::DeckAck(_)) => Some("answer to another request"),
                Ok(_) => Some("not a deck answer"),
                Err(_) => Some("malformed"),
            }
        };
        if let Some(r) = reason
            && ignored < 5
        {
            ignored += 1;
            crate::twlog!("deck: ignored a {n}-byte reply from {from}: {r}");
        }
    }
    Err(fail(format!(
        "no answer from the Mac at {to} within {:.0} s after {sends} sends \
         (is Tightwire running on the Mac with Stream Deck handoff on, and is UDP {} reachable?)",
        wait.as_secs_f64(),
        port
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_worded_as_requests_not_results() {
        let a = DeckAck {
            request_id: 1,
            accepted: true,
            owner: 0,
            message: "Moving devices to the PC".into(),
        };
        let s = describe_ack(DeckTarget::Pc, &a);
        assert!(
            s.starts_with("asked the Mac to send devices to this PC; the Mac accepted"),
            "{s}"
        );
        assert!(s.ends_with("it reports the devices with the Mac)"), "{s}");
        let r = DeckAck {
            accepted: false,
            owner: 1,
            message: "request clock is 40 s off".into(),
            ..a
        };
        assert!(describe_ack(DeckTarget::Mac, &r).contains("the Mac refused"));
    }

    #[test]
    fn unanswered_request_times_out_with_a_reason() {
        // A paired config pointing at a local port nobody answers on.
        let dir = std::env::temp_dir().join(format!("tw-deck-{}", std::process::id()));
        let path = dir.join("pairing.conf");
        let cfg = ConfigFile {
            self_id: [1; 16],
            pairing: Some(crate::pairing::PairingRecord {
                peer_name: "mac".into(),
                peer_id: [2; 16],
                root: [3; 32],
                last_address: None,
            }),
        };
        cfg.save(&path).unwrap();
        // A free port, never 4813: a running Tightwire app owns that one.
        let port = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = request(
            &path,
            DeckTarget::Pc,
            Some("127.0.0.1".parse().unwrap()),
            port,
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(
            e.contains(&format!("no answer from the Mac at 127.0.0.1:{port}")),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

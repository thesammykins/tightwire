//! SPEC §9 (receiver side): usbip-win2 output parsing and the attach/detach
//! sequences, with a fake command runner. Sample output is reconstructed from
//! usbip-win2's format strings (userspace/usbip/port.cpp, attach.cpp,
//! detach.cpp); it has not been captured from a real Windows machine.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tightwire_recv::auth::{self, SessionKeys};
use tightwire_recv::handoff::*;
use tightwire_recv::ingress::{Action, DropInfo, DropReason, Ingress};
use tightwire_recv::net::DropLog;
use tightwire_recv::protocol::{self, HandoffOffer, HandoffResult, Packet};

const PORT_OUTPUT: &str = "Imported USB devices
====================
Port 01: device in use at High Speed(480Mbps)
         Elgato Systems GmbH : unknown product (0fd9:0084)
           -> usbip://192.168.1.10:3240/1-1
           -> remote bus/dev: 001/002
           -> serial: CL12K1A01234
           -> mode: low-latency
Port 02: device in use at Full Speed(12Mbps)
         unknown vendor : unknown product (1005:b113)
           -> usbip://[fe80::1]:3240/1-4
           -> remote bus/dev: 001/005
           -> serial: 
           -> mode: default
";

#[test]
fn parses_port_listing() {
    let p = parse_ports(PORT_OUTPUT);
    assert_eq!(p.len(), 2);
    assert_eq!(
        p[0],
        ImportedPort {
            port: 1,
            host: "192.168.1.10".into(),
            tcp_port: 3240,
            bus_id: "1-1".into()
        }
    );
    assert_eq!(p[1].host, "fe80::1");
    assert_eq!(p[1].bus_id, "1-4");
    assert!(parse_ports("").is_empty());
    assert!(parse_ports("usbip: error: cannot open vhci device\n").is_empty());
    // A "Port" header without its usbip:// line is not a device.
    assert!(parse_ports("Port 03: device in use at ?\n garbage\n").is_empty());
}

#[test]
fn parses_attach_port() {
    assert_eq!(
        parse_attach_port("successfully attached to port 1\n"),
        Some(1)
    );
    assert_eq!(parse_attach_port("3\n"), Some(3));
    assert_eq!(parse_attach_port("usbip: error: ...\n"), None);
    assert_eq!(parse_attach_port("0"), None);
}

#[derive(Default)]
struct Fake {
    calls: Vec<Vec<String>>,
    port_output: String,
    attach_ok: bool,
    attach_hang: bool,
}

impl CommandRunner for Fake {
    fn run(
        &mut self,
        _p: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, String> {
        self.calls.push(args.to_vec());
        let ok = |s: &str| {
            Ok(CommandOutput {
                status: 0,
                stdout: s.into(),
                stderr: String::new(),
            })
        };
        match args.first().map(String::as_str) {
            Some("port") => ok(&self.port_output.clone()),
            Some("attach") if args.contains(&"-x".to_string()) => ok(""),
            Some("attach") if self.attach_hang => {
                Err(format!("usbip.exe timed out after {timeout:?}"))
            }
            Some("attach") if self.attach_ok => ok("4\n"),
            Some("attach") => Ok(CommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: "usbip: error: cannot connect to 192.168.1.10:3240\n".into(),
            }),
            Some("detach") => ok("port 1 is successfully detached\n"),
            _ => Err("unexpected".into()),
        }
    }
}

fn mac() -> IpAddr {
    "192.168.1.10".parse().unwrap()
}

fn offer(id: u32, ids: &[&str]) -> HandoffOffer {
    HandoffOffer {
        request_id: id,
        usbip_port: 3240,
        // 0.2.0: the Mac's own IPv4 inside the tagged body.
        host: "192.168.1.10".into(),
        bus_ids: ids.iter().map(|s| s.to_string()).collect(),
    }
}

fn worker(f: Fake) -> HandoffWorker<Fake> {
    HandoffWorker::new(f, Some(PathBuf::from(DEFAULT_USBIP)))
}

#[test]
fn attach_uses_source_address_and_reports_port() {
    let mut w = worker(Fake {
        attach_ok: true,
        ..Default::default()
    });
    let ack = w.attach(&offer(1, &["1-2"]), mac());
    assert_eq!(ack.items[0].result, HandoffResult::Ok);
    assert_eq!(ack.items[0].port, 4);
}

#[test]
fn command_lines_match_usbip_win2_syntax() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut w = worker(Fake {
        attach_ok: true,
        ..Default::default()
    });
    w.attach(&offer(1, &["1-2"]), mac());
    assert_eq!(
        w.runner().calls,
        vec![
            v(&["port"]),
            v(&[
                "attach",
                "-r",
                "192.168.1.10",
                "-b",
                "1-2",
                "--once",
                "--terse"
            ])
        ]
    );

    let mut w = worker(Fake {
        attach_ok: true,
        ..Default::default()
    });
    let mut o = offer(1, &["1-2"]);
    o.usbip_port = 3241;
    w.attach(&o, mac());
    assert_eq!(
        w.runner().calls[1],
        v(&[
            "--tcp-port",
            "3241",
            "attach",
            "-r",
            "192.168.1.10",
            "-b",
            "1-2",
            "--once",
            "--terse"
        ])
    );

    let mut w = worker(Fake {
        port_output: PORT_OUTPUT.into(),
        ..Default::default()
    });
    w.release(2, &["1-1".into()], mac());
    assert_eq!(
        w.runner().calls,
        vec![
            v(&["port"]),
            v(&["detach", "-p", "1"]),
            v(&["attach", "-r", "192.168.1.10", "-b", "1-1", "-x"])
        ]
    );
}

#[test]
fn already_attached_is_success_without_running_attach() {
    let mut w = worker(Fake {
        port_output: PORT_OUTPUT.into(),
        attach_ok: false,
        ..Default::default()
    });
    let ack = w.attach(&offer(1, &["1-1"]), mac());
    assert_eq!(ack.items[0].result, HandoffResult::Ok);
    assert_eq!(ack.items[0].port, 1);
}

#[test]
fn attach_failure_and_timeout_are_reported_per_device() {
    let mut w = worker(Fake::default());
    let ack = w.attach(&offer(1, &["1-2"]), mac());
    assert_eq!(ack.items[0].result, HandoffResult::Failed);
    assert!(
        ack.items[0].message.contains("cannot connect"),
        "{:?}",
        ack.items[0]
    );
    let mut w = worker(Fake {
        attach_hang: true,
        ..Default::default()
    });
    let ack = w.attach(&offer(1, &["1-2"]), mac());
    assert_eq!(ack.items[0].result, HandoffResult::Failed);
    assert!(ack.items[0].message.contains("timed out"));
}

#[test]
fn missing_usbip_refuses_cleanly() {
    let mut w = HandoffWorker::new(Fake::default(), None);
    let ack = w.attach(&offer(1, &["1-1", "1-2"]), mac());
    assert!(
        ack.items
            .iter()
            .all(|i| i.result == HandoffResult::UsbipMissing)
    );
    let ack = w.release(2, &["1-1".into()], mac());
    assert_eq!(ack.items[0].result, HandoffResult::UsbipMissing);
}

#[test]
fn duplicate_request_answered_from_cache() {
    let mut w = worker(Fake {
        attach_ok: true,
        ..Default::default()
    });
    let a = w.attach(&offer(7, &["1-2"]), mac());
    let b = w.attach(&offer(7, &["1-2"]), mac());
    assert_eq!(a, b);
    // Only one real attach ran.
    assert_eq!(w.log.iter().filter(|l| l.starts_with("attach")).count(), 1);
}

#[test]
fn release_detaches_matching_port_and_stops_retries() {
    let mut w = worker(Fake {
        port_output: PORT_OUTPUT.into(),
        ..Default::default()
    });
    let ack = w.release(3, &["1-1".into(), "1-9".into()], mac());
    assert_eq!(ack.items[0].result, HandoffResult::Ok);
    assert_eq!(ack.items[0].port, 1);
    assert_eq!(ack.items[1].message, "not attached");
}

#[test]
fn release_ignores_same_busid_from_another_host() {
    // 1-4 is attached from fe80::1, not from the Mac asking.
    let mut w = worker(Fake {
        port_output: PORT_OUTPUT.into(),
        ..Default::default()
    });
    let ack = w.release(4, &["1-4".into()], mac());
    assert_eq!(ack.items[0].message, "not attached");
    assert_eq!(ack.items[0].port, 0);
}

#[test]
fn ingress_forwards_only_authentic_offers() {
    let keys = SessionKeys::from_root(&[1u8; 32]);
    let nonce = 99;
    let mut ing = Ingress::new(Some(keys), nonce, None);
    let from = "192.168.1.10:4813".parse().unwrap();
    let body = protocol::encode_handoff_offer(&offer(1, &["1-1"]));
    let good = auth::seal(&keys.m2r, Some(nonce), body.clone());
    assert!(matches!(
        ing.handle(&good, from),
        Action::Control(Packet::HandoffOffer(_), _)
    ));
    let no_nonce = auth::seal(&keys.m2r, None, body.clone());
    assert!(matches!(ing.handle(&no_nonce, from), Action::Drop));
    let wrong_dir = auth::seal(&keys.r2m, Some(nonce), body);
    assert!(matches!(ing.handle(&wrong_dir, from), Action::Drop));
    // A deck request is receiver→Mac; the receiver never acts on one.
    let deck = auth::seal(
        &keys.r2m,
        None,
        protocol::encode_deck_request(&protocol::DeckRequest {
            request_id: 1,
            sent_at_ms: 0,
            target: protocol::DeckTarget::Pc,
        }),
    );
    assert!(matches!(ing.handle(&deck, from), Action::Drop));
    assert_eq!(ing.counters.auth_dropped, 3);
    assert_eq!(
        ing.last_drop,
        Some(DropInfo {
            ptype: Some(protocol::TYPE_DECK_REQUEST),
            reason: DropReason::WrongType
        })
    );
}

#[test]
fn ingress_reports_why_a_handoff_offer_was_dropped() {
    let keys = SessionKeys::from_root(&[1u8; 32]);
    let from = "192.168.1.10:4813".parse().unwrap();
    let body = protocol::encode_handoff_offer(&offer(1, &["1-1"]));
    // Sealed for an earlier receiver session (old nonce).
    let stale = auth::seal(&keys.m2r, Some(1), body.clone());
    let mut ing = Ingress::new(Some(keys), 2, None);
    ing.handle(&stale, from);
    assert_eq!(ing.last_drop.unwrap().reason, DropReason::BadTag);
    assert!(
        DropReason::BadTag
            .describe()
            .contains("earlier receiver session")
    );
    // Unpaired receiver.
    let mut unpaired = Ingress::new(None, 2, None);
    unpaired.handle(&stale, from);
    assert_eq!(unpaired.last_drop.unwrap().reason, DropReason::Unpaired);
    // A good packet clears the previous reason.
    let good = auth::seal(&keys.m2r, Some(2), body);
    assert!(matches!(ing.handle(&good, from), Action::Control(..)));
    assert_eq!(ing.last_drop, None);
}

#[test]
fn dropped_packet_log_is_rate_limited_and_counts_the_rest() {
    let t0 = Instant::now();
    let from = "192.168.1.10:4813".parse().unwrap();
    let offer = DropInfo {
        ptype: Some(protocol::TYPE_HANDOFF_OFFER),
        reason: DropReason::BadTag,
    };
    let mut log = DropLog::default();
    let first = log.note_at(offer, from, 60, t0).unwrap();
    assert!(
        first.starts_with(
            "dropped a handoff offer from 192.168.1.10:4813 (60 bytes): authentication failed"
        ),
        "{first}"
    );
    assert_eq!(
        log.note_at(offer, from, 60, t0 + Duration::from_secs(1)),
        None
    );
    assert_eq!(
        log.note_at(offer, from, 60, t0 + Duration::from_secs(2)),
        None
    );
    // Audio drops have their own (slower) budget.
    let audio = DropInfo {
        ptype: Some(protocol::TYPE_AUDIO),
        reason: DropReason::Unpaired,
    };
    assert!(
        log.note_at(audio, from, 300, t0 + Duration::from_secs(2))
            .is_some()
    );
    assert_eq!(
        log.note_at(audio, from, 300, t0 + Duration::from_secs(30)),
        None
    );
    let later = log
        .note_at(offer, from, 60, t0 + Duration::from_secs(6))
        .unwrap();
    assert!(
        later.ends_with("(+2 similar since the last report)"),
        "{later}"
    );
}

/// `usbip port` exactly as port.cpp prints it for a device attached by IP
/// (hostname printed as given, so IPv6 is not bracketed), with CRLF line
/// ends as a Windows pipe delivers them.
#[test]
fn parses_port_listing_from_windows_pipe() {
    let out = "Imported USB devices\r\n====================\r\nPort 03: device in use at Super Speed(5000Mbps)\r\n         Elgato Systems GmbH : unknown product (0fd9:0084)\r\n           -> usbip://fe80::1:3240/2-1\r\n           -> remote bus/dev: 002/003\r\n           -> serial: \r\n           -> mode: zero-copy\r\n";
    let p = parse_ports(out);
    assert_eq!(
        p,
        vec![ImportedPort {
            port: 3,
            host: "fe80::1".into(),
            tcp_port: 3240,
            bus_id: "2-1".into()
        }]
    );
    // Nothing attached: port.cpp prints nothing at all.
    assert!(parse_ports("").is_empty());
    // Coloured error from a console run.
    assert!(parse_ports("\x1b[31merror\x1b[0m: can't open vhci device\r\n").is_empty());
}

#[test]
fn attach_port_parsing_is_tolerant() {
    // attach.cpp: terse prints the number; otherwise the sentence.
    assert_eq!(parse_attach_port("2\r\n"), Some(2));
    assert_eq!(
        parse_attach_port("debug: connecting\r\nsuccessfully attached to port 12\r\n"),
        Some(12)
    );
    // Digits inside error text are not a port.
    assert_eq!(
        parse_attach_port("error: can't connect to 192.168.1.10:3240\n"),
        None
    );
    assert_eq!(parse_attach_port("300\n"), None);
    assert_eq!(parse_attach_port(""), None);
}

#[test]
fn ack_summary_reads_as_an_outcome() {
    let mut w = worker(Fake {
        attach_ok: true,
        ..Default::default()
    });
    let (ok, text) = ack_summary(&w.attach(&offer(9, &["1-2"]), mac()));
    assert!(ok);
    assert_eq!(text, "attach #9: 1-2 ok (port 4)");
    let mut w = worker(Fake::default());
    let (ok, text) = ack_summary(&w.attach(&offer(10, &["1-2"]), mac()));
    assert!(!ok);
    assert!(
        text.starts_with("attach #10: 1-2 FAILED: usbip: error: cannot connect"),
        "{text}"
    );
}

#[test]
fn every_usbip_call_is_logged_with_command_exit_and_output() {
    let mut w = worker(Fake::default());
    w.attach(&offer(11, &["1-2"]), mac());
    let ran: Vec<&String> = w.log.iter().filter(|l| l.starts_with("ran ")).collect();
    assert_eq!(ran.len(), 2, "{:?}", w.log);
    assert!(
        ran[1].starts_with(
            r#"ran "C:\Program Files\USBip\usbip.exe" attach -r 192.168.1.10 -b 1-2 --once --terse → exit 1 in "#
        ),
        "{}",
        ran[1]
    );
    assert!(
        ran[1].ends_with(r#"stderr: "usbip: error: cannot connect to 192.168.1.10:3240""#),
        "{}",
        ran[1]
    );
    assert!(w.log[0].starts_with("offer #11 from 192.168.1.10: attach [\"1-2\"]"));
}

#[test]
fn unreachable_mac_fails_fast_with_a_hint_on_timeout() {
    let mut w = worker(Fake {
        attach_ok: true,
        ..Default::default()
    })
    .with_reach_check(|_, _| Reach::TimedOut);
    let ack = w.attach(&offer(12, &["1-2"]), mac());
    assert_eq!(ack.items[0].result, HandoffResult::Failed);
    assert!(
        ack.items[0]
            .message
            .contains("the Mac has not allowed this PC yet"),
        "{}",
        ack.items[0].message
    );
    // usbip.exe was never started.
    assert!(w.runner().calls.is_empty());

    // Refused: usbip.exe still runs (its error is authoritative) and the
    // hint is appended to its failure.
    let mut w = worker(Fake::default()).with_reach_check(|_, _| Reach::Refused);
    let ack = w.attach(&offer(13, &["1-2"]), mac());
    assert_eq!(w.runner().calls.len(), 2);
    assert!(ack.items[0].message.contains("cannot connect"));
    assert!(ack.items[0].message.contains("has not allowed this PC yet"));
}

/// Security (0.2.0): the USB/IP host comes only from the authenticated
/// offer body, must be an IPv4 literal and must match the packet source.
#[test]
fn offer_host_must_be_the_signed_ipv4_of_the_sender() {
    let attach_ok = || Fake {
        attach_ok: true,
        ..Default::default()
    };
    // A 0.1.x Mac sends no host: refused with an "update the Mac" message,
    // and usbip.exe never runs (before the fix it attached from the source).
    let mut w = worker(attach_ok());
    let mut o = offer(1, &["1-2"]);
    o.host = String::new();
    let ack = w.attach(&o, mac());
    assert_eq!(ack.items[0].result, HandoffResult::Failed);
    assert!(ack.items[0].message.contains("update Tightwire on the Mac"));
    assert!(w.runner().calls.is_empty());
    // Signed host differs from where the packet came from (relayed offer).
    let mut w = worker(attach_ok());
    let ack = w.attach(&offer(2, &["1-2"]), "192.168.1.66".parse().unwrap());
    assert_eq!(ack.items[0].result, HandoffResult::Failed);
    assert!(ack.items[0].message.contains("came from 192.168.1.66"));
    assert!(w.runner().calls.is_empty());
    // A host name is not accepted either.
    let mut w = worker(attach_ok());
    let mut o = offer(3, &["1-2"]);
    o.host = "mac.local".into();
    assert_eq!(w.attach(&o, mac()).items[0].result, HandoffResult::Failed);
    assert!(w.runner().calls.is_empty());
}

/// A replayed offer whose ack has left the 32-entry cache must not run
/// usbip.exe again.
#[test]
fn replayed_offer_after_cache_pressure_is_refused() {
    let mut w = worker(Fake {
        attach_ok: true,
        ..Default::default()
    });
    assert_eq!(
        w.attach(&offer(1000, &["1-2"]), mac()).items[0].result,
        HandoffResult::Ok
    );
    for id in 0..40 {
        w.release(id, &["1-2".into()], mac());
    }
    let calls = w.runner().calls.len();
    let ack = w.attach(&offer(1000, &["1-2"]), mac());
    assert_eq!(ack.items[0].result, HandoffResult::Failed);
    assert!(ack.items[0].message.contains("replay"));
    assert_eq!(w.runner().calls.len(), calls, "usbip.exe ran for a replay");
    // Same for releases.
    let ack = w.release(5, &["1-2".into()], mac());
    assert!(ack.items[0].message.contains("replay"));
}

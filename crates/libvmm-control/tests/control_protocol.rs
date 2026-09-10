//! §8 control plane: handshake, concurrency cap, lockout, protocol v1 frames,
//! input arbitration, and TLS 1.3-only provisioning.

use libvmm_config::MachineConfig;
use libvmm_control::auth;
use libvmm_control::handshake::*;
use libvmm_control::lockout::*;
use libvmm_control::proto::*;
use libvmm_control::tls;
use libvmm_control::ws::{self, Decoded, Frame};
use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant};

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml")).unwrap()
}

fn source() -> IpAddr {
    IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))
}

fn upgrade_request(authorization: Option<&str>) -> UpgradeRequest {
    let mut text = String::from(
        "GET /console HTTP/1.1\r\n\
         Host: [::]:8080\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n",
    );
    if let Some(a) = authorization {
        text.push_str(&format!("Authorization: {a}\r\n"));
    }
    text.push_str("\r\n");
    UpgradeRequest::parse(&text).unwrap()
}

fn basic(user: &str, pass: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

// -- §8.3 handshake ----------------------------------------------------------

#[test]
fn valid_credentials_yield_101_with_the_rfc6455_accept_key() {
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let r = upgrade_request(Some(&basic("admin", "hypervisor@01")));
    let outcome = evaluate(&r, source(), &mut lock, 0, &cfg.control_wss);

    assert_eq!(outcome.status(), 101);
    assert!(outcome.upgraded());
    // RFC 6455's own worked example.
    match &outcome {
        HandshakeOutcome::Accept { accept_key } => {
            assert_eq!(accept_key, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        }
        other => panic!("expected an accept, got {other:?}"),
    }
    assert!(outcome
        .to_http()
        .starts_with("HTTP/1.1 101 Switching Protocols"));
}

#[test]
fn invalid_credentials_yield_401_with_the_kvm_control_realm() {
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let r = upgrade_request(Some(&basic("admin", "wrong")));
    let outcome = evaluate(&r, source(), &mut lock, 0, &cfg.control_wss);

    assert_eq!(outcome.status(), 401);
    assert!(!outcome.upgraded());
    assert!(outcome
        .to_http()
        .contains(r#"WWW-Authenticate: Basic realm="KVM-Control""#));
}

#[test]
fn a_missing_authorization_header_yields_401() {
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let outcome = evaluate(
        &upgrade_request(None),
        source(),
        &mut lock,
        0,
        &cfg.control_wss,
    );
    assert_eq!(outcome.status(), 401);
}

#[test]
fn a_third_client_is_rejected_with_503_before_the_upgrade() {
    // §8.3: at most 2 authenticated clients; a 3rd is rejected at handshake.
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let r = upgrade_request(Some(&basic("admin", "hypervisor@01")));

    assert!(evaluate(&r, source(), &mut lock, 0, &cfg.control_wss).upgraded());
    assert!(evaluate(&r, source(), &mut lock, 1, &cfg.control_wss).upgraded());

    let third = evaluate(&r, source(), &mut lock, 2, &cfg.control_wss);
    assert_eq!(third.status(), 503);
    assert!(!third.upgraded(), "the 3rd client must never be upgraded");
}

#[test]
fn the_client_registry_enforces_the_cap_and_the_post_race_close_code() {
    let mut reg = ClientRegistry::new(2);
    let a = reg.admit().unwrap();
    let b = reg.admit().unwrap();
    assert!(reg.is_full());
    assert!(reg.admit().is_none(), "the cap is hard");
    assert_eq!(CLOSE_TRY_AGAIN_LATER, 1013, "§8.3 post-race close code");

    reg.release(a);
    assert!(reg.admit().is_some());
    reg.release(b);
    assert_eq!(reg.count(), 1);
}

#[test]
fn a_non_websocket_request_is_400_and_a_wrong_path_is_404() {
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);

    let plain = UpgradeRequest::parse("GET /console HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    assert_eq!(
        evaluate(&plain, source(), &mut lock, 0, &cfg.control_wss).status(),
        400
    );

    let mut wrong = upgrade_request(Some(&basic("admin", "hypervisor@01")));
    wrong.path = "/other".to_string();
    assert_eq!(
        evaluate(&wrong, source(), &mut lock, 0, &cfg.control_wss).status(),
        404
    );
}

// -- §8.4 lockout ------------------------------------------------------------

#[test]
fn ten_failures_lock_the_source_out_for_the_configured_window() {
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let bad = upgrade_request(Some(&basic("admin", "wrong")));

    // Failures 1..9 keep answering 401.
    for attempt in 1..cfg.control_wss.max_auth_attempts {
        let o = evaluate(&bad, source(), &mut lock, 0, &cfg.control_wss);
        assert_eq!(o.status(), 401, "attempt {attempt} should still be 401");
    }
    // The 10th trips the lockout.
    let o = evaluate(&bad, source(), &mut lock, 0, &cfg.control_wss);
    assert_eq!(o.status(), 429, "attempt 10 must lock out");
    match o {
        HandshakeOutcome::LockedOut { remaining_secs } => {
            assert!(remaining_secs <= cfg.control_wss.lockout_duration_secs);
        }
        other => panic!("expected a lockout, got {other:?}"),
    }
}

#[test]
fn while_locked_out_correct_credentials_are_not_even_checked() {
    // §8.4: "while LOCKED -> 429 ... no credential check".
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let bad = upgrade_request(Some(&basic("admin", "wrong")));
    for _ in 0..cfg.control_wss.max_auth_attempts {
        evaluate(&bad, source(), &mut lock, 0, &cfg.control_wss);
    }

    let good = upgrade_request(Some(&basic("admin", "hypervisor@01")));
    let o = evaluate(&good, source(), &mut lock, 0, &cfg.control_wss);
    assert_eq!(o.status(), 429, "the right password must not unlock early");
}

#[test]
fn the_lockout_expires_and_resets_the_counter() {
    let mut lock = LockoutTable::new(10, 300);
    let t0 = Instant::now();
    for _ in 0..10 {
        lock.record_failure_at(source(), t0);
    }
    assert!(lock.check_at(source(), t0).is_locked());
    assert!(lock
        .check_at(source(), t0 + Duration::from_secs(299))
        .is_locked());

    let after = t0 + Duration::from_secs(301);
    assert_eq!(lock.check_at(source(), after), Decision::Proceed);
    assert_eq!(lock.attempts(source()), 0, "expiry resets attempts to 0");
}

#[test]
fn a_successful_auth_resets_the_counter() {
    let mut lock = LockoutTable::new(10, 300);
    for _ in 0..5 {
        lock.record_failure(source());
    }
    assert_eq!(lock.attempts(source()), 5);
    lock.record_success(source());
    assert_eq!(lock.attempts(source()), 0);
}

#[test]
fn lockout_is_tracked_per_source_ip() {
    let mut lock = LockoutTable::new(10, 300);
    let other = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2));
    for _ in 0..10 {
        lock.record_failure(source());
    }
    assert!(lock.check(source()).is_locked());
    assert_eq!(
        lock.check(other),
        Decision::Proceed,
        "one bad source must not lock out everyone"
    );
}

// -- §8.5 protocol v1 --------------------------------------------------------

#[test]
fn the_spec_8_5_client_frames_parse() {
    let keyboard = r#"{ "v":1, "seq":42, "action":"input", "device":"keyboard",
        "type":"EV_KEY", "code":30, "value":1 }"#;
    match parse_client_frame(keyboard).unwrap() {
        ClientFrame::Input(i @ InputFrame::Keyboard { code, value, .. }) => {
            assert_eq!(code, 30);
            assert_eq!(value, 1);
            assert_eq!(i.seq(), 42);
            assert_eq!(
                i.target_bdf(),
                (0x02, 0x00, 0),
                "keyboard routes to 02:00.0"
            );
        }
        other => panic!("expected a keyboard frame, got {other:?}"),
    }

    let tablet = r#"{ "v":1, "seq":43, "action":"input", "device":"tablet",
        "type":"EV_ABS", "x":16384, "y":8192,
        "buttons":{ "left":true, "right":false, "middle":false } }"#;
    match parse_client_frame(tablet).unwrap() {
        ClientFrame::Input(i @ InputFrame::Tablet { x, y, buttons, .. }) => {
            assert_eq!((x, y), (16384, 8192));
            assert!(buttons.left && !buttons.right);
            assert_eq!(i.target_bdf(), (0x02, 0x01, 0), "tablet routes to 02:01.0");
        }
        other => panic!("expected a tablet frame, got {other:?}"),
    }

    for (json, action) in [
        (r#"{ "v":1, "seq":44, "action":"powerdown" }"#, "powerdown"),
        (r#"{ "v":1, "seq":45, "action":"reboot" }"#, "reboot"),
        (
            r#"{ "v":1, "seq":46, "action":"backup", "path":"/var/backups/vm.vmbk" }"#,
            "backup",
        ),
    ] {
        assert_eq!(parse_client_frame(json).unwrap().action(), action);
    }
}

#[test]
fn malformed_json_and_unknown_actions_are_6400() {
    for bad in [
        "{not json",
        r#"{ "v":1, "seq":1, "action":"selfdestruct" }"#,
    ] {
        let (_, e) = parse_client_frame(bad).unwrap_err();
        assert_eq!(e.code(), 6400, "{bad}");
    }
}

#[test]
fn a_mismatched_protocol_version_is_6400() {
    let (seq, e) = parse_client_frame(r#"{ "v":2, "seq":9, "action":"reboot" }"#).unwrap_err();
    assert_eq!(e.code(), 6400);
    assert_eq!(seq, 9, "the error frame must still echo the seq");
}

#[test]
fn an_out_of_range_coordinate_is_6422() {
    let json = r#"{ "v":1, "seq":43, "action":"input", "device":"tablet",
        "type":"EV_ABS", "x":40000, "y":10 }"#;
    let (seq, e) = parse_client_frame(json).unwrap_err();
    assert_eq!(e.code(), 6422);
    assert_eq!(seq, 43);
    assert!(e.to_string().contains("out of range"));
}

#[test]
fn an_out_of_range_key_value_is_6422() {
    let json = r#"{ "v":1, "seq":7, "action":"input", "device":"keyboard",
        "type":"EV_KEY", "code":30, "value":9 }"#;
    let (_, e) = parse_client_frame(json).unwrap_err();
    assert_eq!(e.code(), 6422);
}

#[test]
fn server_frames_match_the_spec_8_5_shapes() {
    assert_eq!(
        ServerFrame::ack(42).to_json(),
        r#"{"type":"ack","v":1,"seq":42}"#
    );

    let err = ServerFrame::error_with(43, 6422, "coordinate out of range");
    let v: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
    assert_eq!(v["v"], 1);
    assert_eq!(v["type"], "error");
    assert_eq!(v["seq"], 43);
    assert_eq!(v["code"], 6422);

    let p = ServerFrame::backup_progress(3_900_000_000, 10_500_000_000);
    let v: serde_json::Value = serde_json::from_str(&p.to_json()).unwrap();
    assert_eq!(v["type"], "progress");
    assert_eq!(v["action"], "backup");
    assert_eq!(v["percent"], 37, "the spec's own worked example");
    assert_eq!(v["bytes"], 3_900_000_000u64);

    let c = ServerFrame::backup_complete("/var/backups/vm.vmbk", "deadbeef");
    let v: serde_json::Value = serde_json::from_str(&c.to_json()).unwrap();
    assert_eq!(v["type"], "complete");
    assert_eq!(v["path"], "/var/backups/vm.vmbk");
    assert_eq!(v["sha256"], "deadbeef");
}

#[test]
fn error_frames_carry_the_appendix_a_code() {
    let e = libvmm_core::ControlError::BackupBusy;
    let frame = ServerFrame::error(46, &e);
    let v: serde_json::Value = serde_json::from_str(&frame.to_json()).unwrap();
    assert_eq!(v["code"], 6423);
}

#[test]
fn tablet_coordinates_map_onto_the_scanout() {
    assert_eq!(map_abs(0, 1920), 0);
    assert_eq!(map_abs(ABS_MAX, 1920), 1919);
    assert_eq!(map_abs(ABS_MAX, 1080), 1079);
    // The spec's example: x=16384 is just past the midpoint.
    assert_eq!(map_abs(16384, 1920), 959);
}

// -- §8.3 input arbitration --------------------------------------------------

#[test]
fn input_arbitration_is_last_write_wins_with_no_primary_role() {
    let mut arb = InputArbiter::new();
    assert!(arb.accept(1));
    assert_eq!(arb.last_writer(), Some(1));
    // The second client is not an observer: its frame simply wins.
    assert!(arb.accept(2));
    assert_eq!(arb.last_writer(), Some(2));
    assert!(arb.accept(1));
    assert_eq!(arb.last_writer(), Some(1));
    assert_eq!(
        arb.accepted(),
        3,
        "no frame is dropped for arbitration reasons"
    );
}

// -- WebSocket codec ---------------------------------------------------------

#[test]
fn a_masked_client_text_frame_round_trips() {
    let payload = br#"{"v":1,"seq":1,"action":"reboot"}"#;
    let mask = [0x37u8, 0xFA, 0x21, 0x3D];
    let mut raw = vec![0x81, 0x80 | payload.len() as u8];
    raw.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        raw.push(b ^ mask[i % 4]);
    }

    match ws::decode(&raw).unwrap() {
        Decoded::Frame(Frame::Text(s), consumed) => {
            assert_eq!(s.as_bytes(), payload);
            assert_eq!(consumed, raw.len());
        }
        other => panic!("expected a text frame, got {other:?}"),
    }
}

#[test]
fn an_unmasked_client_frame_is_refused() {
    let raw = vec![0x81, 0x03, b'a', b'b', b'c'];
    assert_eq!(ws::decode(&raw).unwrap_err().code(), 6400);
}

#[test]
fn a_partial_frame_asks_for_more_bytes_rather_than_erroring() {
    assert!(matches!(ws::decode(&[0x81]).unwrap(), Decoded::Incomplete));
    assert!(matches!(
        ws::decode(&[0x81, 0x85, 0x00]).unwrap(),
        Decoded::Incomplete
    ));
}

#[test]
fn server_frames_are_encoded_unmasked_with_fin_set() {
    let out = ws::encode(&Frame::Text("hi".into()));
    assert_eq!(out[0], 0x81, "FIN | text");
    assert_eq!(out[1], 2, "unmasked, 2-byte payload");
    assert_eq!(&out[2..], b"hi");
}

#[test]
fn a_close_frame_carries_its_code() {
    let out = ws::encode(&Frame::Close {
        code: CLOSE_TRY_AGAIN_LATER,
        reason: String::new(),
    });
    assert_eq!(out[0], 0x88);
    assert_eq!(u16::from_be_bytes([out[2], out[3]]), 1013);
}

// -- §8.2 TLS ----------------------------------------------------------------

#[test]
fn the_hypervisor_self_signs_a_certificate_at_boot() {
    let cfg = reference();
    let id = tls::SelfSignedIdentity::generate(&cfg.vm.name).unwrap();
    assert!(!id.certificate_der.is_empty());
    assert!(!id.private_key_der.is_empty());
    assert!(id.subject_alt_names.contains(&cfg.vm.name));
    assert_eq!(tls::provider_name(), "rustls/ring (TLS 1.3 only)");

    // And the resulting TLS 1.3-only server config must be constructible.
    let _ = tls::server_config(&id).unwrap();
}

#[test]
fn any_tls_version_but_1_3_is_refused() {
    assert!(tls::check_tls_min("control_wss", "1.3").is_ok());
    for bad in ["1.2", "1.1", "1.0"] {
        assert!(
            tls::check_tls_min("control_wss", bad).is_err(),
            "{bad} must be refused"
        );
    }
}

// -- §8.4 credential handling -----------------------------------------------

#[test]
fn basic_auth_decoding_and_constant_time_comparison() {
    let (u, p) = auth::parse_basic(&basic("admin", "hypervisor@01")).unwrap();
    assert_eq!((u.as_str(), p.as_str()), ("admin", "hypervisor@01"));

    assert!(auth::constant_time_eq(b"secret", b"secret"));
    assert!(!auth::constant_time_eq(b"secret", b"secrey"));
    assert!(!auth::constant_time_eq(b"secret", b"secre"));
    assert!(auth::parse_basic("Bearer xyz").is_none());
}

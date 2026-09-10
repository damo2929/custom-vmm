//! Both halves of §8 against each other.
//!
//! The client-side encoder feeds the server-side decoder and vice versa, so
//! the RFC 6455 masking rule and the protocol-v1 schema are checked from both
//! directions rather than only against hand-written fixtures.

use libvmm_config::MachineConfig;
use libvmm_control::handshake::*;
use libvmm_control::lockout::LockoutTable;
use libvmm_control::proto::*;
use libvmm_control::ws::{self, Decoded, Frame, Role};
use std::net::{IpAddr, Ipv6Addr};

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml")).unwrap()
}

fn source() -> IpAddr {
    IpAddr::V6(Ipv6Addr::LOCALHOST)
}

/// Decode exactly one frame, asserting the buffer held a whole one.
fn decode_one(bytes: &[u8], role: Role) -> Frame {
    match ws::decode_as(bytes, role).unwrap() {
        Decoded::Frame(f, consumed) => {
            assert_eq!(
                consumed,
                bytes.len(),
                "the frame must consume the whole buffer"
            );
            f
        }
        Decoded::Incomplete => panic!("expected a complete frame"),
    }
}

// -- RFC 6455 masking, both directions --------------------------------------

#[test]
fn a_client_frame_is_masked_and_the_server_decodes_it() {
    let json = r#"{"v":1,"seq":1,"action":"reboot"}"#;
    let wire = ws::encode_client(&Frame::Text(json.to_string()));

    // The mask bit must be set, and the payload must not appear in clear.
    assert_ne!(wire[1] & 0x80, 0, "client frames MUST be masked");
    assert!(
        !wire.windows(json.len()).any(|w| w == json.as_bytes()),
        "a masked payload must not appear verbatim on the wire"
    );

    assert_eq!(
        decode_one(&wire, Role::Server),
        Frame::Text(json.to_string())
    );
}

#[test]
fn a_server_frame_is_unmasked_and_the_client_decodes_it() {
    let json = ServerFrame::ack(7).to_json();
    let wire = ws::encode(&Frame::Text(json.clone()));

    assert_eq!(wire[1] & 0x80, 0, "server frames MUST NOT be masked");
    assert_eq!(decode_one(&wire, Role::Client), Frame::Text(json));
}

#[test]
fn each_side_refuses_what_the_other_is_required_to_send() {
    let client_wire = ws::encode_client(&Frame::Text("x".into()));
    let server_wire = ws::encode(&Frame::Text("x".into()));

    // A client must not accept a masked frame...
    assert_eq!(
        ws::decode_as(&client_wire, Role::Client)
            .unwrap_err()
            .code(),
        6400
    );
    // ...and a server must not accept an unmasked one.
    assert_eq!(
        ws::decode_as(&server_wire, Role::Server)
            .unwrap_err()
            .code(),
        6400
    );
}

#[test]
fn masking_is_not_a_fixed_key() {
    // A constant mask would defeat its purpose entirely.
    let a = ws::encode_client(&Frame::Text("same payload".into()));
    let b = ws::encode_client(&Frame::Text("same payload".into()));
    assert_ne!(a, b, "each frame must use a fresh masking key");
    // Both still decode to the same text.
    assert_eq!(decode_one(&a, Role::Server), decode_one(&b, Role::Server));
}

#[test]
fn frames_of_every_length_class_round_trip() {
    // 7-bit, 16-bit and 64-bit length encodings.
    for len in [10usize, 200, 70_000] {
        let text = "x".repeat(len);
        let wire = ws::encode_client(&Frame::Text(text.clone()));
        // The 64-bit case exceeds the server's accept limit by design.
        match ws::decode_as(&wire, Role::Server) {
            Ok(Decoded::Frame(Frame::Text(back), _)) => assert_eq!(back.len(), len),
            Ok(other) => panic!("unexpected decode {other:?}"),
            Err(e) => assert!(
                len > ws::MAX_FRAME_PAYLOAD,
                "unexpected error for len {len}: {e}"
            ),
        }
    }
}

// -- protocol v1 round trip --------------------------------------------------

#[test]
fn every_client_action_survives_a_round_trip_through_both_codecs() {
    let frames = [
        ClientFrame::Input(InputFrame::Keyboard {
            v: PROTOCOL_VERSION,
            seq: 42,
            event_type: "EV_KEY".into(),
            code: 30,
            value: 1,
        }),
        ClientFrame::Input(InputFrame::Tablet {
            v: PROTOCOL_VERSION,
            seq: 43,
            event_type: "EV_ABS".into(),
            x: 16384,
            y: 8192,
            buttons: Buttons {
                left: true,
                right: false,
                middle: false,
            },
        }),
        ClientFrame::Powerdown {
            v: PROTOCOL_VERSION,
            seq: 44,
        },
        ClientFrame::Reboot {
            v: PROTOCOL_VERSION,
            seq: 45,
        },
        ClientFrame::Backup {
            v: PROTOCOL_VERSION,
            seq: 46,
            path: "/var/backups/vm.vmbk".into(),
        },
    ];

    for frame in frames {
        // Client: serialise, wrap, mask.
        let json = to_json(&frame).unwrap();
        let wire = ws::encode_client(&Frame::Text(json));

        // Server: unwrap, unmask, parse.
        let Frame::Text(text) = decode_one(&wire, Role::Server) else {
            panic!("expected a text frame");
        };
        let parsed = parse_client_frame(&text).unwrap();

        assert_eq!(parsed, frame, "the frame changed in transit");
        assert_eq!(parsed.seq(), frame.seq());
    }
}

#[test]
fn every_server_frame_parses_back_on_the_client() {
    let frames = [
        ServerFrame::ack(42),
        ServerFrame::error_with(43, 6422, "coordinate out of range"),
        ServerFrame::backup_progress(3_900_000_000, 10_500_000_000),
        ServerFrame::backup_complete("/var/backups/vm.vmbk", "deadbeef"),
    ];

    for frame in frames {
        let wire = ws::encode(&Frame::Text(frame.to_json()));
        let Frame::Text(text) = decode_one(&wire, Role::Client) else {
            panic!("expected a text frame");
        };
        let parsed: ServerFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, frame);
    }
}

// -- the handshake, driven by a client-built request ------------------------

/// Build the exact upgrade request the client sends.
fn client_upgrade_request(key: &str, username: &str, password: &str) -> String {
    use base64::Engine as _;
    let credential =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!(
        "GET {ENDPOINT} HTTP/1.1\r\n\
         Host: [::1]:8080\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Authorization: Basic {credential}\r\n\r\n"
    )
}

#[test]
fn the_clients_own_request_is_accepted_and_its_accept_key_verifies() {
    let cfg = reference();
    let mut lock = LockoutTable::from_config(&cfg.control_wss);

    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let raw = client_upgrade_request(key, &cfg.control_wss.username, &cfg.control_wss.password);
    let request = UpgradeRequest::parse(&raw).unwrap();

    let outcome = evaluate(&request, source(), &mut lock, 0, &cfg.control_wss);
    assert_eq!(outcome.status(), 101);

    // The client recomputes the accept key from its own nonce and must get
    // the same answer, which is what proves it reached a real WebSocket.
    match outcome {
        HandshakeOutcome::Accept { accept_key } => assert_eq!(accept_key, accept_key_for(key)),
        other => panic!("expected an accept, got {other:?}"),
    }
}

fn accept_key_for(key: &str) -> String {
    libvmm_control::handshake::accept_key(key)
}

#[test]
fn the_client_can_distinguish_the_three_rejection_reasons() {
    let cfg = reference();
    let key = "dGhlIHNhbXBsZSBub25jZQ==";

    // 401: wrong password.
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let bad = UpgradeRequest::parse(&client_upgrade_request(key, "admin", "wrong")).unwrap();
    assert_eq!(
        evaluate(&bad, source(), &mut lock, 0, &cfg.control_wss).status(),
        401
    );

    // 503: at the two-client cap, with correct credentials.
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    let good = UpgradeRequest::parse(&client_upgrade_request(
        key,
        &cfg.control_wss.username,
        &cfg.control_wss.password,
    ))
    .unwrap();
    assert_eq!(
        evaluate(&good, source(), &mut lock, 2, &cfg.control_wss).status(),
        503
    );

    // 429: locked out after 10 failures, even with the right password.
    let mut lock = LockoutTable::from_config(&cfg.control_wss);
    for _ in 0..cfg.control_wss.max_auth_attempts {
        evaluate(&bad, source(), &mut lock, 0, &cfg.control_wss);
    }
    assert_eq!(
        evaluate(&good, source(), &mut lock, 0, &cfg.control_wss).status(),
        429
    );
}

#[test]
fn a_ping_from_the_server_is_answered_with_a_masked_pong() {
    // The client answers pings so a session is not dropped for idleness.
    let ping = ws::encode(&Frame::Ping(b"keepalive".to_vec()));
    let Frame::Ping(payload) = decode_one(&ping, Role::Client) else {
        panic!("expected a ping");
    };

    let pong = ws::encode_client(&Frame::Pong(payload.clone()));
    assert_ne!(pong[1] & 0x80, 0, "the client's pong must be masked");
    assert_eq!(decode_one(&pong, Role::Server), Frame::Pong(payload));
}

#[test]
fn the_post_race_close_code_round_trips() {
    // §8.3: a client that loses the race for the last slot is closed 1013.
    let wire = ws::encode(&Frame::Close {
        code: CLOSE_TRY_AGAIN_LATER,
        reason: "at capacity".into(),
    });
    match decode_one(&wire, Role::Client) {
        Frame::Close { code, reason } => {
            assert_eq!(code, 1013);
            assert_eq!(reason, "at capacity");
        }
        other => panic!("expected a close frame, got {other:?}"),
    }
}

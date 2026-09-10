//! Client-side behaviour: input translation, transport address handling, and
//! the USB/IP server driven over a real socket.

use libvmm_control::proto::{parse_client_frame, ClientFrame, InputFrame};
use libvmm_usbip::wire::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use vmm_console_client::{input, transport, usbdev, usbip_server};

// -- §8.5 input translation --------------------------------------------------

#[test]
fn letters_map_to_their_linux_keycodes() {
    // linux/input-event-codes.h: KEY_A=30, KEY_Z=44, KEY_Q=16, KEY_1=2.
    assert_eq!(input::keycode_for('a'), Some((30, false)));
    assert_eq!(input::keycode_for('z'), Some((44, false)));
    assert_eq!(input::keycode_for('q'), Some((16, false)));
    assert_eq!(input::keycode_for('1'), Some((2, false)));
    assert_eq!(input::keycode_for('0'), Some((11, false)));
    assert_eq!(input::keycode_for(' '), Some((57, false)));
    assert_eq!(input::keycode_for('\r'), Some((28, false)));
}

#[test]
fn uppercase_and_symbols_request_shift() {
    assert_eq!(input::keycode_for('A'), Some((30, true)));
    assert_eq!(input::keycode_for('!'), Some((2, true)));
    assert_eq!(input::keycode_for('?'), Some((53, true)));
    // The unshifted partner shares the keycode.
    assert_eq!(input::keycode_for('/'), Some((53, false)));
}

#[test]
fn a_shifted_character_brackets_the_key_with_shift_press_and_release() {
    let events = input::frames_for('A');
    assert_eq!(
        events,
        vec![
            (input::keycode::LEFTSHIFT, input::PRESS),
            (30, input::PRESS),
            (30, input::RELEASE),
            (input::keycode::LEFTSHIFT, input::RELEASE),
        ]
    );
}

#[test]
fn an_unshifted_character_is_just_press_and_release() {
    assert_eq!(
        input::frames_for('a'),
        vec![(30, input::PRESS), (30, input::RELEASE)]
    );
}

#[test]
fn ansi_arrow_sequences_become_arrow_keycodes() {
    // ESC [ A/B/C/D
    let events = input::decode_terminal_input(b"\x1b[A\x1b[B\x1b[C\x1b[D");
    let pressed: Vec<i64> = events
        .iter()
        .filter(|(_, v)| *v == input::PRESS)
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(
        pressed,
        vec![
            input::keycode::UP,
            input::keycode::DOWN,
            input::keycode::RIGHT,
            input::keycode::LEFT
        ]
    );
    // Every press has a matching release.
    assert_eq!(events.len(), 8);
}

#[test]
fn multi_byte_navigation_sequences_are_consumed_whole() {
    // ESC [ 3 ~ is Delete; the trailing '~' must not leak through as a key.
    let events = input::decode_terminal_input(b"\x1b[3~");
    assert_eq!(
        events,
        vec![
            (input::keycode::DELETE, input::PRESS),
            (input::keycode::DELETE, input::RELEASE)
        ]
    );
}

#[test]
fn typing_a_word_produces_frames_the_server_accepts() {
    // Every generated frame must survive the hypervisor's own parser.
    for (i, (code, value)) in input::decode_terminal_input(b"Hi!").into_iter().enumerate() {
        let frame = input::key_frame(i as u64 + 1, code, value);
        let json = libvmm_control::proto::to_json(&frame).unwrap();
        match parse_client_frame(&json) {
            Ok(ClientFrame::Input(InputFrame::Keyboard {
                code: c, value: v, ..
            })) => {
                assert_eq!((c, v), (code, value));
            }
            other => panic!("the server rejected a generated frame: {other:?}"),
        }
    }
}

#[test]
fn generated_tablet_frames_stay_inside_the_valid_range() {
    for (x, y) in [(0i64, 0i64), (16384, 8192), (32767, 32767)] {
        let frame = input::tablet_frame(1, x, y, Default::default());
        let json = libvmm_control::proto::to_json(&frame).unwrap();
        assert!(
            parse_client_frame(&json).is_ok(),
            "({x},{y}) should be accepted"
        );
    }
    // And an out-of-range one is caught by the server, not silently sent.
    let frame = input::tablet_frame(1, 40_000, 0, Default::default());
    let json = libvmm_control::proto::to_json(&frame).unwrap();
    let (_, e) = parse_client_frame(&json).unwrap_err();
    assert_eq!(e.code(), 6422);
}

// -- transport ---------------------------------------------------------------

#[test]
fn the_bracketed_ipv6_form_the_spec_uses_parses() {
    assert_eq!(transport::host_of("[2001:db8::200]:3241"), "2001:db8::200");
    assert_eq!(transport::host_of("[::1]:8080"), "::1");
    assert_eq!(
        transport::host_of("hypervisor.example:8554"),
        "hypervisor.example"
    );
    assert!(transport::resolve("[::1]:8080").is_ok());
}

#[test]
fn ip_literals_are_distinguished_from_names() {
    assert!(transport::is_ip_literal("::1"));
    assert!(transport::is_ip_literal("192.0.2.1"));
    assert!(!transport::is_ip_literal("hypervisor.example"));
    assert!(!transport::is_ip_literal("localhost"));
}

// -- §9 USB/IP server over a real socket ------------------------------------

/// Start the server on an ephemeral port and return its address.
///
/// The listener is the production `serve_one` path; only the port is
/// different, so the protocol under test is the real one.
fn spawn_server(allow: Vec<String>, present: Vec<String>) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        let mut server = usbip_server::UsbipServer::new(allow, false);
        for busid in &present {
            server.export(usbip_server::stub_device(busid));
        }
        // One connection per test is enough.
        if let Ok((stream, _)) = listener.accept() {
            let _ = server.serve_connection_for_test(stream);
        }
    });
    addr
}

fn request(addr: std::net::SocketAddr, bytes: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(bytes).unwrap();
    stream.flush().unwrap();
    let mut reply = Vec::new();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let _ = stream.read_to_end(&mut reply);
    reply
}

#[test]
fn devlist_over_a_socket_returns_only_allowed_devices() {
    // "2-4" is present on the host but not in the allow-set.
    let addr = spawn_server(vec!["1-2".into()], vec!["1-2".into(), "2-4".into()]);
    let reply = request(addr, &OpCommon::new(OP_REQ_DEVLIST, 0).encode());

    let header = OpCommon::decode(&reply).unwrap();
    assert_eq!(header.code, OP_REP_DEVLIST);
    assert_eq!(header.status, ST_OK);

    let count = u32::from_be_bytes(reply[8..12].try_into().unwrap());
    assert_eq!(
        count, 1,
        "§9.2: a device outside the allow-set must not even be listed"
    );

    let device = DeviceInfo::decode(&reply[12..]).unwrap();
    assert_eq!(device.busid, "1-2");
}

#[test]
fn an_allowed_import_is_accepted_over_a_socket() {
    let addr = spawn_server(vec!["1-2".into()], vec!["1-2".into()]);

    let mut req = OpCommon::new(OP_REQ_IMPORT, 0).encode().to_vec();
    let mut busid = [0u8; BUSID_LEN];
    busid[..3].copy_from_slice(b"1-2");
    req.extend_from_slice(&busid);

    let reply = request(addr, &req);
    let header = OpCommon::decode(&reply).unwrap();
    assert_eq!(header.code, OP_REP_IMPORT);
    assert_eq!(header.status, ST_OK);
    assert_eq!(DeviceInfo::decode(&reply[8..]).unwrap().busid, "1-2");
}

#[test]
fn a_disallowed_import_is_refused_over_a_socket() {
    let addr = spawn_server(vec!["1-2".into()], vec!["1-2".into(), "9-9".into()]);

    let mut req = OpCommon::new(OP_REQ_IMPORT, 0).encode().to_vec();
    let mut busid = [0u8; BUSID_LEN];
    busid[..3].copy_from_slice(b"9-9");
    req.extend_from_slice(&busid);

    let reply = request(addr, &req);
    let header = OpCommon::decode(&reply).unwrap();
    assert_eq!(header.code, OP_REP_IMPORT);
    assert_eq!(
        header.status, ST_NA,
        "§9.2: an import outside the allow-set MUST be refused"
    );
    assert_eq!(
        reply.len(),
        OpCommon::LEN,
        "a refusal carries no device descriptor"
    );
}

#[test]
fn a_peer_speaking_the_wrong_version_is_rejected() {
    let addr = spawn_server(vec!["1-2".into()], vec!["1-2".into()]);
    let mut req = OpCommon::new(OP_REQ_DEVLIST, 0).encode();
    req[0..2].copy_from_slice(&0x0100u16.to_be_bytes());
    // The server closes without replying rather than guessing the layout.
    assert!(request(addr, &req).is_empty());
}

// -- host enumeration --------------------------------------------------------

#[test]
fn host_enumeration_reports_well_formed_devices() {
    // A machine may legitimately have no USB devices, so this checks shape
    // rather than presence.
    for d in usbdev::enumerate() {
        assert!(!d.busid.is_empty());
        assert!(
            !d.busid.contains(':'),
            "interface directories must be filtered out"
        );
        assert!(d.busnum > 0 && d.devnum > 0);
        let node = d.devnode().display().to_string();
        assert!(
            node.starts_with("/dev/bus/usb/"),
            "unexpected device node {node}"
        );
        // The wire form must round-trip.
        let wire = d.to_wire();
        assert_eq!(DeviceInfo::decode(&wire.encode()).unwrap().busid, d.busid);
    }
}

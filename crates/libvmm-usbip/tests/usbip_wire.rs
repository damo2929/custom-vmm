//! §9 USB/IP: big-endian wire structures, the import sequence, the allow-set
//! isolation rule, and peer-reset handling.

use libvmm_usbip::bridge::*;
use libvmm_usbip::wire::*;

#[test]
fn all_structures_are_big_endian() {
    // §0.1 / §9: USB/IP wire structures are network order.
    let c = OpCommon::new(OP_REQ_IMPORT, ST_OK);
    let b = c.encode();
    assert_eq!(&b[0..2], &USBIP_VERSION.to_be_bytes());
    assert_eq!(&b[2..4], &OP_REQ_IMPORT.to_be_bytes());
    assert_eq!(u16::from_be_bytes([b[2], b[3]]), 0x8003);
    // If this were little-endian the code would read 0x0380.
    assert_ne!(u16::from_le_bytes([b[2], b[3]]), 0x8003);
}

#[test]
fn op_common_round_trips_and_checks_the_version() {
    let c = OpCommon::new(OP_REP_DEVLIST, ST_OK);
    let back = OpCommon::decode(&c.encode()).unwrap();
    assert_eq!(back, c);
    assert!(back.check_version().is_ok());

    let mut bad = c.encode();
    bad[0..2].copy_from_slice(&0x0100u16.to_be_bytes());
    let e = OpCommon::decode(&bad).unwrap().check_version().unwrap_err();
    assert_eq!(e.code(), 7002);
}

#[test]
fn the_spec_9_1_opcodes_are_correct() {
    assert_eq!(OP_REQ_DEVLIST, 0x8005);
    assert_eq!(OP_REP_DEVLIST, 0x0005);
    assert_eq!(OP_REQ_IMPORT, 0x8003);
    assert_eq!(OP_REP_IMPORT, 0x0003);
    assert_eq!(USBIP_CMD_SUBMIT, 1);
    assert_eq!(USBIP_CMD_UNLINK, 2);
    assert_eq!(USBIP_RET_SUBMIT, 3);
    assert_eq!(USBIP_RET_UNLINK, 4);
}

#[test]
fn header_basic_round_trips() {
    let h = HeaderBasic {
        command: USBIP_CMD_SUBMIT,
        seqnum: 42,
        devid: 0x0001_0002,
        direction: DIRECTION_IN,
        ep: 1,
    };
    assert_eq!(HeaderBasic::decode(&h.encode()).unwrap(), h);
    assert_eq!(HeaderBasic::LEN, 20);
}

#[test]
fn cmd_submit_round_trips_with_its_setup_packet() {
    let c = CmdSubmit {
        header: HeaderBasic {
            command: USBIP_CMD_SUBMIT,
            seqnum: 7,
            devid: 3,
            direction: DIRECTION_IN,
            ep: 0,
        },
        transfer_flags: 0,
        transfer_buffer_length: 64,
        start_frame: 0,
        number_of_packets: -1,
        interval: 0,
        // GET_DESCRIPTOR(device)
        setup: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x40, 0x00],
    };
    let back = CmdSubmit::decode(&c.encode()).unwrap();
    assert_eq!(back, c);
    assert_eq!(back.setup[1], 0x06);
    // An IN transfer carries no payload after the header.
    assert_eq!(back.payload_len(), 0);
}

#[test]
fn an_out_submission_carries_its_data_length() {
    let c = CmdSubmit {
        header: HeaderBasic {
            command: USBIP_CMD_SUBMIT,
            seqnum: 8,
            devid: 3,
            direction: DIRECTION_OUT,
            ep: 2,
        },
        transfer_flags: 0,
        transfer_buffer_length: 512,
        start_frame: 0,
        number_of_packets: -1,
        interval: 0,
        setup: [0u8; 8],
    };
    assert_eq!(c.payload_len(), 512);
}

#[test]
fn ret_submit_round_trips() {
    let r = RetSubmit {
        header: HeaderBasic {
            command: USBIP_RET_SUBMIT,
            seqnum: 7,
            devid: 0,
            direction: DIRECTION_IN,
            ep: 0,
        },
        status: 0,
        actual_length: 18,
        start_frame: 0,
        number_of_packets: -1,
        error_count: 0,
    };
    assert_eq!(RetSubmit::decode(&r.encode()).unwrap(), r);
}

#[test]
fn a_truncated_structure_is_rejected_rather_than_read_out_of_bounds() {
    assert_eq!(OpCommon::decode(&[0, 1, 2]).unwrap_err().code(), 7002);
    assert_eq!(HeaderBasic::decode(&[0u8; 10]).unwrap_err().code(), 7002);
    assert_eq!(CmdSubmit::decode(&[0u8; 30]).unwrap_err().code(), 7002);
    assert_eq!(DeviceInfo::decode(&[0u8; 100]).unwrap_err().code(), 7002);
}

#[test]
fn device_info_round_trips_with_nul_padded_fields() {
    let d = DeviceInfo {
        path: "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2".into(),
        busid: "1-2".into(),
        busnum: 1,
        devnum: 2,
        speed: 3,
        id_vendor: 0x046D,
        id_product: 0xC52B,
        bcd_device: 0x1201,
        device_class: 0,
        device_subclass: 0,
        device_protocol: 0,
        configuration_value: 1,
        num_configurations: 1,
        num_interfaces: 2,
    };
    let back = DeviceInfo::decode(&d.encode()).unwrap();
    assert_eq!(back, d);
    assert_eq!(back.busid, "1-2", "trailing NULs must be trimmed");
}

// -- §9.2 transport ----------------------------------------------------------

#[test]
fn use_tls_selects_port_3241() {
    assert_eq!(port_for(true), 3241);
    assert_eq!(port_for(false), 3240);
}

// -- §9.2 import and relay ---------------------------------------------------

#[test]
fn the_import_allow_set_is_enforced() {
    // §9.2: "the server MUST reject import of a busid not in its allow-set".
    let policy = ImportPolicy::new(["1-2".to_string(), "3-1".to_string()]);
    assert!(policy.authorize("1-2").is_ok());
    assert_eq!(policy.status_for("1-2"), ST_OK);

    let e = policy.authorize("2-4").unwrap_err();
    assert_eq!(e.code(), 7001, "must be Usbip(ImportDenied)");
    assert_eq!(policy.status_for("2-4"), ST_NA);
}

#[test]
fn an_empty_allow_set_permits_nothing() {
    let policy = ImportPolicy::default();
    assert!(policy.authorize("1-1").is_err());
}

#[test]
fn attaching_devices_consumes_ports_and_the_ninth_is_refused() {
    let mut b = XhciBridge::new(8);
    for i in 0..8 {
        assert_eq!(b.attach(&format!("1-{i}"), i as u32).unwrap(), i);
    }
    let e = b.attach("2-1", 99).unwrap_err();
    assert_eq!(e.code(), 7006);
}

#[test]
fn a_urb_round_trips_through_submit_and_completion() {
    let mut b = XhciBridge::new(8);
    let port = b.attach("1-2", 0x0001_0002).unwrap();

    let cmd = b.submit(port, 1, DIRECTION_IN, 64, [0u8; 8], 0).unwrap();
    assert_eq!(cmd.header.command, USBIP_CMD_SUBMIT);
    assert_eq!(cmd.header.devid, 0x0001_0002);
    assert_eq!(b.inflight_count(), 1);

    let ret = RetSubmit {
        header: HeaderBasic {
            command: USBIP_RET_SUBMIT,
            seqnum: cmd.header.seqnum,
            devid: cmd.header.devid,
            direction: DIRECTION_IN,
            ep: 1,
        },
        status: 0,
        actual_length: 18,
        start_frame: 0,
        number_of_packets: -1,
        error_count: 0,
    };
    let flight = b.complete(&ret).unwrap();
    assert_eq!(flight.port, port);
    assert_eq!(
        b.inflight_count(),
        0,
        "the completion must retire the submission"
    );
}

#[test]
fn a_completion_with_an_unknown_seqnum_is_rejected() {
    let mut b = XhciBridge::new(8);
    b.attach("1-2", 1).unwrap();
    let ret = RetSubmit {
        header: HeaderBasic {
            command: USBIP_RET_SUBMIT,
            seqnum: 999,
            devid: 1,
            direction: DIRECTION_IN,
            ep: 1,
        },
        status: 0,
        actual_length: 0,
        start_frame: 0,
        number_of_packets: -1,
        error_count: 0,
    };
    assert_eq!(b.complete(&ret).unwrap_err().code(), 7002);
}

#[test]
fn seqnums_are_unique_across_submissions() {
    let mut b = XhciBridge::new(8);
    let port = b.attach("1-2", 1).unwrap();
    let a = b.submit(port, 1, DIRECTION_IN, 8, [0u8; 8], 0).unwrap();
    let c = b.submit(port, 1, DIRECTION_IN, 8, [0u8; 8], 0).unwrap();
    assert_ne!(a.header.seqnum, c.header.seqnum);
}

#[test]
fn unlink_names_the_seqnum_being_cancelled() {
    let mut b = XhciBridge::new(8);
    let port = b.attach("1-2", 1).unwrap();
    let submit = b.submit(port, 1, DIRECTION_IN, 8, [0u8; 8], 0).unwrap();

    let unlink = b.unlink(submit.header.seqnum).unwrap();
    assert_eq!(unlink.header.command, USBIP_CMD_UNLINK);
    assert_eq!(unlink.unlink_seqnum, submit.header.seqnum);
    assert_ne!(
        unlink.header.seqnum, submit.header.seqnum,
        "the unlink has its own seqnum"
    );
}

#[test]
fn a_dropped_socket_detaches_the_device_and_orphans_its_urbs() {
    // §9.2: "A dropped socket detaches the device and signals the guest with
    // an xHCI port-disconnect event."
    let mut b = XhciBridge::new(8);
    let port = b.attach("1-2", 1).unwrap();
    let first = b.submit(port, 1, DIRECTION_IN, 8, [0u8; 8], 0).unwrap();
    let second = b.submit(port, 2, DIRECTION_OUT, 8, [0u8; 8], 0).unwrap();

    let orphaned = b.on_peer_reset(port);
    assert_eq!(
        orphaned.len(),
        2,
        "in-flight URBs must be completed, not hung"
    );
    assert!(orphaned.contains(&first.header.seqnum));
    assert!(orphaned.contains(&second.header.seqnum));
    assert!(matches!(b.port(port), Some(PortState::Disconnected { .. })));
    assert_eq!(b.inflight_count(), 0);

    // Once the guest has seen the disconnect the port is reusable.
    b.clear_disconnect(port);
    assert_eq!(b.port(port), Some(&PortState::Empty));
    assert!(b.attach("3-1", 2).is_ok());
}

#[test]
fn submitting_to_an_empty_port_is_refused() {
    let mut b = XhciBridge::new(2);
    assert_eq!(
        b.submit(0, 1, DIRECTION_IN, 8, [0u8; 8], 0)
            .unwrap_err()
            .code(),
        7005
    );
}

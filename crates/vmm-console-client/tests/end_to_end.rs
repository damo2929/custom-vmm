//! A real client against a real listener, over TLS 1.3 on a loopback socket.
//!
//! This is the test that proves the two halves of §8 actually interoperate:
//! the hypervisor's `ControlListener` and the client's `ControlClient`, with
//! nothing stubbed between them.

use libvmm_config::MachineConfig;
use libvmm_control::proto::{ClientFrame, InputFrame, ServerFrame, PROTOCOL_VERSION};
use libvmm_control::tls::CertPolicy;
use libvmm_control::{ActionHandler, ControlListener};
use libvmm_core::ControlError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vmm_console_client::{input, wss};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Records what the listener dispatched, so a test can assert on it.
#[derive(Default)]
struct Recorder {
    inputs: AtomicU64,
    actions: Mutex<Vec<String>>,
    /// Set to make `on_backup` fail, to exercise the error-frame path.
    fail_backup: bool,
}

impl Recorder {
    fn actions(&self) -> Vec<String> {
        self.actions.lock().map(|a| a.clone()).unwrap_or_default()
    }
    fn note(&self, what: &str) {
        self.actions
            .lock()
            .map(|mut a| a.push(what.to_string()))
            .ok();
    }
}

impl ActionHandler for Recorder {
    fn on_input(&self, _client: u64, frame: &InputFrame) -> Result<(), ControlError> {
        self.inputs.fetch_add(1, Ordering::Relaxed);
        self.note(&format!("input:{}", frame.seq()));
        Ok(())
    }
    fn on_powerdown(&self, _client: u64) -> Result<(), ControlError> {
        self.note("powerdown");
        Ok(())
    }
    fn on_reboot(&self, _client: u64) -> Result<(), ControlError> {
        self.note("reboot");
        Ok(())
    }
    fn on_backup(
        &self,
        _client: u64,
        path: &str,
        progress: &dyn Fn(ServerFrame),
    ) -> Result<(), ControlError> {
        self.note(&format!("backup:{path}"));
        progress(ServerFrame::backup_progress(37, 100));
        if self.fail_backup {
            return Err(ControlError::BackupBusy);
        }
        Ok(())
    }
}

struct Server {
    addr: String,
    recorder: Arc<Recorder>,
    config: libvmm_config::ControlWss,
}

/// Start a listener on an ephemeral port.
fn start(fail_backup: bool) -> Server {
    let mut config =
        MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml"))
            .unwrap()
            .control_wss;
    config.port = 0; // ask the OS for a free port

    let recorder = Arc::new(Recorder {
        fail_backup,
        ..Default::default()
    });
    let listener = Arc::new(
        ControlListener::new(
            config.clone(),
            "loopback-test",
            Arc::clone(&recorder) as Arc<dyn ActionHandler>,
        )
        .expect("the listener needs a TLS provider"),
    );
    let socket = listener.bind().expect("bind");
    let addr = socket.local_addr().expect("local_addr").to_string();

    std::thread::spawn(move || listener.serve(socket));
    // Give the accept loop a moment to reach `incoming()`.
    std::thread::sleep(Duration::from_millis(50));

    Server {
        addr,
        recorder,
        config,
    }
}

fn connect(server: &Server) -> wss::ControlClient {
    wss::ControlClient::connect(
        &server.addr,
        &server.config.username,
        &server.config.password,
        CertPolicy::AcceptAny,
        TIMEOUT,
    )
    .expect("the client should connect")
}

/// Unwrap a refused connection. `ControlClient` holds a live socket and is
/// deliberately not `Debug`, so `expect_err` cannot be used on it.
fn expect_refused(
    result: Result<wss::ControlClient, libvmm_core::VmmError>,
    context: &str,
) -> libvmm_core::VmmError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("{context}"),
    }
}

/// Read frames until one satisfies `want`, or the deadline passes.
fn wait_for(
    client: &mut wss::ControlClient,
    want: impl Fn(&ServerFrame) -> bool,
) -> Option<ServerFrame> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if let Ok(Some(frame)) = client.recv(Some(Duration::from_millis(200))) {
            if want(&frame) {
                return Some(frame);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------

#[test]
fn a_client_connects_over_tls_and_the_server_dispatches_its_actions() {
    let server = start(false);
    let mut client = connect(&server);

    let seq = client
        .send(|seq| ClientFrame::Reboot {
            v: PROTOCOL_VERSION,
            seq,
        })
        .expect("send reboot");

    let ack =
        wait_for(&mut client, |f| matches!(f, ServerFrame::Ack { .. })).expect("an ack for reboot");
    assert_eq!(
        ack,
        ServerFrame::Ack {
            v: PROTOCOL_VERSION,
            seq
        }
    );
    assert!(server.recorder.actions().contains(&"reboot".to_string()));
}

#[test]
fn input_frames_are_dispatched_but_not_acknowledged() {
    // §8.5: "Input frames are acknowledged only on error to keep the datapath
    // light."
    let server = start(false);
    let mut client = connect(&server);

    for _ in 0..5 {
        client
            .send(|seq| input::key_frame(seq, 30, 1))
            .expect("send key");
    }
    // Give the server time to process, then confirm no ack came back.
    std::thread::sleep(Duration::from_millis(300));
    let stray = client.recv(Some(Duration::from_millis(200))).expect("recv");
    assert!(
        stray.is_none(),
        "input frames must not be acknowledged, got {stray:?}"
    );

    assert_eq!(
        server.recorder.inputs.load(Ordering::Relaxed),
        5,
        "all five must have been dispatched"
    );
}

#[test]
fn a_bad_frame_yields_an_error_and_the_socket_stays_open() {
    // §8.5 robustness rule.
    let server = start(false);
    let mut client = connect(&server);

    // An out-of-range coordinate: the server answers 6422.
    client
        .send(|seq| input::tablet_frame(seq, 40_000, 0, Default::default()))
        .expect("send a bad tablet frame");

    let error =
        wait_for(&mut client, |f| matches!(f, ServerFrame::Error { .. })).expect("an error frame");
    match error {
        ServerFrame::Error { code, .. } => assert_eq!(code, 6422),
        other => panic!("expected an error, got {other:?}"),
    }

    // The socket must still work: a following action is answered normally.
    let seq = client
        .send(|seq| ClientFrame::Powerdown {
            v: PROTOCOL_VERSION,
            seq,
        })
        .expect("send powerdown");
    let ack = wait_for(&mut client, |f| matches!(f, ServerFrame::Ack { .. }))
        .expect("an ack after the error");
    assert_eq!(
        ack,
        ServerFrame::Ack {
            v: PROTOCOL_VERSION,
            seq
        }
    );
}

#[test]
fn backup_progress_frames_reach_the_client() {
    let server = start(false);
    let mut client = connect(&server);

    client
        .send(|seq| ClientFrame::Backup {
            v: PROTOCOL_VERSION,
            seq,
            path: "/var/backups/vm.vmbk".into(),
        })
        .expect("send backup");

    let progress = wait_for(&mut client, |f| matches!(f, ServerFrame::Progress { .. }))
        .expect("a progress frame");
    match progress {
        ServerFrame::Progress {
            action, percent, ..
        } => {
            assert_eq!(action, "backup");
            assert_eq!(percent, 37);
        }
        other => panic!("expected progress, got {other:?}"),
    }
    assert!(server
        .recorder
        .actions()
        .iter()
        .any(|a| a.starts_with("backup:")));
}

#[test]
fn a_failing_action_produces_an_error_frame_with_its_appendix_a_code() {
    let server = start(true);
    let mut client = connect(&server);

    client
        .send(|seq| ClientFrame::Backup {
            v: PROTOCOL_VERSION,
            seq,
            path: "/tmp/x.vmbk".into(),
        })
        .expect("send backup");

    let error =
        wait_for(&mut client, |f| matches!(f, ServerFrame::Error { .. })).expect("an error frame");
    match error {
        // ControlError::BackupBusy is 6423.
        ServerFrame::Error { code, .. } => assert_eq!(code, 6423),
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn wrong_credentials_are_refused_at_the_upgrade() {
    let server = start(false);
    let e = expect_refused(
        wss::ControlClient::connect(
            &server.addr,
            "admin",
            "wrong",
            CertPolicy::AcceptAny,
            TIMEOUT,
        ),
        "bad credentials must not connect",
    );
    assert_eq!(e.code(), 6401, "the client must report NotAuthenticated");
}

#[test]
fn a_third_client_is_refused_while_two_are_connected() {
    // §8.3, end to end: the cap is enforced by the live listener.
    let server = start(false);
    let _first = connect(&server);
    let _second = connect(&server);
    // Let both sessions claim their slots.
    std::thread::sleep(Duration::from_millis(200));

    let e = expect_refused(
        wss::ControlClient::connect(
            &server.addr,
            &server.config.username,
            &server.config.password,
            CertPolicy::AcceptAny,
            TIMEOUT,
        ),
        "a third client must be refused",
    );
    assert_eq!(e.code(), 6003, "the client must report AtClientCap");
}

#[test]
fn a_released_slot_admits_the_next_client() {
    let server = start(false);
    let first = connect(&server);
    let _second = connect(&server);
    std::thread::sleep(Duration::from_millis(200));

    first.close();
    // The server notices the close and frees the slot.
    std::thread::sleep(Duration::from_millis(400));

    let mut third = wss::ControlClient::connect(
        &server.addr,
        &server.config.username,
        &server.config.password,
        CertPolicy::AcceptAny,
        TIMEOUT,
    )
    .expect("the freed slot should admit a new client");

    let seq = third
        .send(|seq| ClientFrame::Reboot {
            v: PROTOCOL_VERSION,
            seq,
        })
        .expect("the new session should work");
    assert!(wait_for(
        &mut third,
        |f| matches!(f, ServerFrame::Ack { seq: s, .. } if *s == seq)
    )
    .is_some());
}

#[test]
fn ten_failures_lock_the_source_out_even_with_the_right_password() {
    // §8.4, end to end.
    let server = start(false);
    for _ in 0..server.config.max_auth_attempts {
        let _ = wss::ControlClient::connect(
            &server.addr,
            "admin",
            "wrong",
            CertPolicy::AcceptAny,
            TIMEOUT,
        );
    }
    let e = expect_refused(
        wss::ControlClient::connect(
            &server.addr,
            &server.config.username,
            &server.config.password,
            CertPolicy::AcceptAny,
            TIMEOUT,
        ),
        "the source must be locked out",
    );
    assert_eq!(e.code(), 6004, "the client must report LockedOut");
}

#[test]
fn seq_numbers_are_monotonic_across_a_session() {
    let server = start(false);
    let mut client = connect(&server);

    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(
            client
                .send(|seq| ClientFrame::Reboot {
                    v: PROTOCOL_VERSION,
                    seq,
                })
                .expect("send"),
        );
    }
    assert_eq!(seen, vec![1, 2, 3, 4], "§8.5: seq is monotonic per client");

    // And every one is acknowledged with its own seq echoed back.
    let mut acked = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while acked.len() < 4 && std::time::Instant::now() < deadline {
        if let Ok(Some(ServerFrame::Ack { seq, .. })) =
            client.recv(Some(Duration::from_millis(200)))
        {
            acked.push(seq);
        }
    }
    assert_eq!(acked, vec![1, 2, 3, 4]);
}

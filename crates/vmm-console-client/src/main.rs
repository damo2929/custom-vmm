//! `vmm-console-client` — remote console and embedded USB/IP server (§1.1).
//!
//! Three jobs, matching the three protocols the hypervisor exposes:
//!
//! * `connect` / `send` — the WSS control channel (§8), protocol v1
//! * `console` — the RTSPS media stream (§7), reassembled to elementary streams
//! * `usbip` — the embedded USB/IP server (§9), relaying real host devices

#![deny(clippy::unwrap_used, clippy::expect_used)]

use vmm_console_client::{
    decode, input, rtsp_client, transport, usbdev, usbip_server, wayland, wss,
};

use clap::{Parser, Subcommand};
use libvmm_control::proto::{ClientFrame, ServerFrame, PROTOCOL_VERSION};
use libvmm_control::tls::CertPolicy;
use libvmm_core::VmmError;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "vmm-console-client",
    version,
    about = "Remote console and USB/IP server for the legacy-free Rust KVM hypervisor"
)]
struct Args {
    #[command(subcommand)]
    command: Command,

    #[arg(long, default_value = "info", global = true)]
    log_level: String,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the WSS control channel and forward local keystrokes to the guest
    /// (§8). Ctrl-] returns to the shell.
    Connect {
        /// Hypervisor control endpoint, `host:port`.
        #[arg(long, default_value = "[::1]:8080")]
        addr: String,
        #[arg(long, default_value = "admin")]
        username: String,
        #[arg(long, default_value = "hypervisor@01")]
        password: String,
        /// Accept the hypervisor's boot-time self-signed certificate (§8.2).
        #[arg(long)]
        insecure: bool,
        /// Exit after this many seconds instead of running until Ctrl-].
        #[arg(long)]
        seconds: Option<u64>,
    },

    /// Send one control action and print the server's reply (§8.5).
    Send {
        #[arg(long, default_value = "[::1]:8080")]
        addr: String,
        #[arg(long, default_value = "admin")]
        username: String,
        #[arg(long, default_value = "hypervisor@01")]
        password: String,
        #[arg(long)]
        insecure: bool,
        /// powerdown, reboot, backup, key, move
        action: String,
        /// Destination for `backup`.
        #[arg(long, default_value = "/var/backups/vm.vmbk")]
        path: String,
        /// Linux keycode for `key`.
        #[arg(long, default_value_t = 30)]
        code: i64,
        #[arg(long, default_value_t = 1)]
        value: i64,
        /// Tablet coordinates for `move`, 0..32767.
        #[arg(long, default_value_t = 16384)]
        x: i64,
        #[arg(long, default_value_t = 8192)]
        y: i64,
        /// Keep reading replies for this long, to catch backup progress.
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },

    /// Open the RTSPS console stream and reassemble it (§7).
    Console {
        #[arg(long, default_value = "[::1]:8554")]
        addr: String,
        #[arg(long, default_value = "/live")]
        stream_path: String,
        #[arg(long, default_value = "admin")]
        username: String,
        #[arg(long, default_value = "hypervisor@01")]
        password: String,
        #[arg(long)]
        insecure: bool,
        /// Write the video elementary stream here, in whatever codec was
        /// negotiated. Annex-B when that is H.264.
        #[arg(long)]
        video_out: Option<std::path::PathBuf>,
        /// Write length-prefixed audio packets here, in whatever codec was
        /// negotiated (Opus, or Vorbis as the fallback).
        #[arg(long)]
        audio_out: Option<std::path::PathBuf>,
        /// Decode the video rather than storing it, writing raw BGRA frames
        /// here. Mutually exclusive with --video-out.
        #[arg(long, conflicts_with = "video_out")]
        decode_to: Option<std::path::PathBuf>,
        /// Decode the video and write the last frame here as a PPM.
        #[arg(long, conflicts_with = "video_out")]
        snapshot: Option<std::path::PathBuf>,
        /// Do not open the console window. For headless capture — a
        /// recording box, or CI — where there is no compositor to open it
        /// on. Implied by --video-out, which stores the stream instead of
        /// decoding it.
        #[arg(long)]
        no_display: bool,
        /// The §8 control endpoint, where the window's keyboard and pointer
        /// input is sent. Watch-only without it.
        #[arg(long, default_value = "[::1]:8080")]
        control_addr: String,
        /// Show the console without sending input to the guest.
        #[arg(long)]
        view_only: bool,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },

    /// Serve the embedded USB/IP server (§9).
    Usbip {
        /// Bus IDs the server may export. Anything else is refused (§9.2).
        #[arg(long = "allow", value_name = "BUSID")]
        allow: Vec<String>,
        /// Use TLS 1.3 on :3241 instead of cleartext on :3240.
        #[arg(long)]
        tls: bool,
        /// Report what would be exported and exit, without binding.
        #[arg(long)]
        dry_run: bool,
        /// Export a placeholder for every allowed bus ID that is not present
        /// on this host, so the protocol can be exercised without hardware.
        #[arg(long)]
        stub: bool,
    },

    /// List the USB devices on this host, with their bus IDs (§9.2).
    Devices,

    /// Print a control frame without connecting, for scripting (§8.5).
    Frame {
        /// input-key, input-move, powerdown, reboot, backup
        action: String,
        #[arg(long, default_value_t = 1)]
        seq: u64,
        #[arg(long, default_value_t = 30)]
        code: i64,
        #[arg(long, default_value_t = 1)]
        value: i64,
        #[arg(long, default_value_t = 16384)]
        x: i64,
        #[arg(long, default_value_t = 8192)]
        y: i64,
        #[arg(long, default_value = "/var/backups/vm.vmbk")]
        path: String,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    env_logger::Builder::new()
        .parse_filters(&args.log_level)
        .init();

    match run(args.command) {
        Ok(code) => code,
        Err(e) => {
            log::error!("[{} {}] {e}", e.domain(), e.code());
            ExitCode::from((e.code() / 1000).min(255) as u8)
        }
    }
}

fn run(command: Command) -> Result<ExitCode, VmmError> {
    match command {
        Command::Connect {
            addr,
            username,
            password,
            insecure,
            seconds,
        } => connect(&Endpoint::new(addr, username, password, insecure), seconds),
        Command::Send {
            addr,
            username,
            password,
            insecure,
            action,
            path,
            code,
            value,
            x,
            y,
            wait_secs,
        } => send_one(
            &Endpoint::new(addr, username, password, insecure),
            &SendRequest {
                action: &action,
                path: &path,
                code,
                value,
                x,
                y,
                wait_secs,
            },
        ),
        Command::Console {
            addr,
            stream_path,
            username,
            password,
            insecure,
            video_out,
            audio_out,
            decode_to,
            snapshot,
            no_display,
            control_addr,
            view_only,
            seconds,
        } => console(
            &Endpoint::new(addr, username, password, insecure),
            &stream_path,
            ConsoleOutputs {
                video_out,
                audio_out,
                decode_to,
                snapshot,
                no_display,
                control_addr,
                view_only,
            },
            seconds,
        ),
        Command::Usbip {
            allow,
            tls,
            dry_run,
            stub,
        } => usbip(allow, tls, dry_run, stub),
        Command::Devices => {
            devices();
            Ok(ExitCode::SUCCESS)
        }
        Command::Frame {
            action,
            seq,
            code,
            value,
            x,
            y,
            path,
        } => match build_frame(&action, seq, code, value, x, y, &path) {
            Some(json) => {
                println!("{json}");
                Ok(ExitCode::SUCCESS)
            }
            None => {
                log::error!(
                        "unknown action {action:?}; expected input-key, input-move, powerdown, reboot or backup"
                    );
                Ok(ExitCode::FAILURE)
            }
        },
    }
}

/// §8.2/§9.2: without a cluster CA the hypervisor's certificate is
/// self-signed, so `--insecure` is how a client accepts it.
fn policy(insecure: bool) -> CertPolicy {
    let p = CertPolicy::from_verify_flag(!insecure);
    if let Some(warning) = p.warning() {
        log::warn!("{warning}");
    }
    p
}

/// Warn before a verification failure that has an obvious cause: the
/// hypervisor's boot-time certificate carries `localhost` and the VM name as
/// SANs (§8.2), never an IP literal, so verifying against one always fails.
fn warn_if_ip_literal(addr: &str, policy: CertPolicy) {
    if policy == CertPolicy::Verify && transport::is_ip_literal(&transport::host_of(addr)) {
        log::warn!(
            "{addr} is an IP literal: the hypervisor's self-signed certificate lists \
             \"localhost\" and the VM name as SANs (§8.2), so verification will fail. \
             Use --insecure, or connect by name."
        );
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where and how to reach the hypervisor. Every subcommand takes these.
struct Endpoint {
    addr: String,
    username: String,
    password: String,
    policy: CertPolicy,
}

impl Endpoint {
    fn new(addr: String, username: String, password: String, insecure: bool) -> Self {
        let policy = policy(insecure);
        warn_if_ip_literal(&addr, policy);
        Endpoint {
            addr,
            username,
            password,
            policy,
        }
    }

    fn connect_control(&self) -> Result<wss::ControlClient, VmmError> {
        wss::ControlClient::connect(
            &self.addr,
            &self.username,
            &self.password,
            self.policy,
            CONNECT_TIMEOUT,
        )
    }
}

// ---------------------------------------------------------------------------
// §8 control channel
// ---------------------------------------------------------------------------

fn connect(endpoint: &Endpoint, seconds: Option<u64>) -> Result<ExitCode, VmmError> {
    let mut client = endpoint.connect_control()?;
    log::info!(
        "connected to {} (protocol v{PROTOCOL_VERSION})",
        client.endpoint
    );

    let mut terminal = input::RawTerminal::acquire();
    match &terminal {
        Some(_) => log::info!("forwarding keystrokes to the guest; Ctrl-] to disconnect"),
        None => log::info!("stdin is not a terminal: watching for server frames only"),
    }

    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let mut sent = 0u64;

    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }

        if let Some(term) = &terminal {
            let bytes = term.read_available();
            // Ctrl-] (0x1D) is the escape back to the shell, as with telnet.
            if bytes.contains(&0x1D) {
                log::info!("disconnecting");
                break;
            }
            for (code, value) in input::decode_terminal_input(&bytes) {
                log::trace!("seq {}: keycode {code} value {value}", client.next_seq());
                client.send(|seq| input::key_frame(seq, code, value))?;
                sent += 1;
            }
        }

        // §8.5: input frames are acknowledged only on error, so anything that
        // arrives is worth showing.
        match client.recv(Some(Duration::from_millis(100)))? {
            Some(frame) => report(&frame),
            None if terminal.is_none() && deadline.is_none() => break,
            None => {}
        }
    }

    if let Some(term) = terminal.as_mut() {
        term.restore();
    }
    log::info!("sent {sent} input frame(s)");
    client.close();
    Ok(ExitCode::SUCCESS)
}

/// What a single `send` should do.
struct SendRequest<'a> {
    action: &'a str,
    path: &'a str,
    code: i64,
    value: i64,
    x: i64,
    y: i64,
    wait_secs: u64,
}

fn send_one(endpoint: &Endpoint, request: &SendRequest<'_>) -> Result<ExitCode, VmmError> {
    let SendRequest {
        action,
        path,
        code,
        value,
        x,
        y,
        wait_secs,
    } = *request;
    let mut client = endpoint.connect_control()?;
    log::info!("connected to {}", client.endpoint);

    let seq = match action {
        "powerdown" => client.send(|seq| ClientFrame::Powerdown {
            v: PROTOCOL_VERSION,
            seq,
        })?,
        "reboot" => client.send(|seq| ClientFrame::Reboot {
            v: PROTOCOL_VERSION,
            seq,
        })?,
        "backup" => client.send(|seq| ClientFrame::Backup {
            v: PROTOCOL_VERSION,
            seq,
            path: path.to_string(),
        })?,
        "key" => {
            // §8.5: value is 0=release, 1=press, 2=repeat. Catch a bad one
            // here rather than making the server answer 6422.
            if !matches!(value, input::RELEASE | input::PRESS | input::REPEAT) {
                log::error!(
                    "--value {value} is not {} (release), {} (press) or {} (repeat)",
                    input::RELEASE,
                    input::PRESS,
                    input::REPEAT
                );
                return Ok(ExitCode::FAILURE);
            }
            client.send(|seq| input::key_frame(seq, code, value))?
        }
        "move" => client.send(|seq| input::tablet_frame(seq, x, y, Default::default()))?,
        other => {
            log::error!("unknown action {other:?}");
            return Ok(ExitCode::FAILURE);
        }
    };
    log::info!("sent {action} as seq {seq}");

    // A backup streams progress frames, so keep reading rather than exiting
    // on the first reply.
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    let mut exit = ExitCode::SUCCESS;
    while Instant::now() < deadline {
        if let Some(frame) = client.recv(Some(Duration::from_millis(250)))? {
            report(&frame);
            match &frame {
                ServerFrame::Error { .. } => exit = ExitCode::FAILURE,
                ServerFrame::Complete { .. } => break,
                ServerFrame::Ack { .. } if action != "backup" => break,
                _ => {}
            }
        }
    }
    client.close();
    Ok(exit)
}

/// Print a server frame the way an operator wants to read it.
fn report(frame: &ServerFrame) {
    match frame {
        ServerFrame::Ack { seq, .. } => log::info!("ack seq {seq}"),
        ServerFrame::Error {
            seq, code, message, ..
        } => {
            log::error!("error seq {seq} [{code}] {message}")
        }
        ServerFrame::Progress {
            action,
            percent,
            bytes,
            total_bytes,
            ..
        } => {
            log::info!("{action}: {percent}% ({bytes}/{total_bytes} bytes)")
        }
        ServerFrame::Complete {
            action,
            path,
            sha256,
            ..
        } => {
            log::info!("{action} complete: {path} sha256={sha256}")
        }
    }
}

// ---------------------------------------------------------------------------
// §7 media
// ---------------------------------------------------------------------------

/// Where a console session sends what it receives.
///
/// Grouped rather than passed loose because the five are one decision: they
/// select between storing the elementary stream, decoding to files, and
/// painting a window.
struct ConsoleOutputs {
    video_out: Option<std::path::PathBuf>,
    audio_out: Option<std::path::PathBuf>,
    decode_to: Option<std::path::PathBuf>,
    snapshot: Option<std::path::PathBuf>,
    no_display: bool,
    control_addr: String,
    view_only: bool,
}

fn console(
    endpoint: &Endpoint,
    stream_path: &str,
    out: ConsoleOutputs,
    seconds: u64,
) -> Result<ExitCode, VmmError> {
    let ConsoleOutputs {
        video_out,
        audio_out,
        decode_to,
        snapshot,
        no_display,
        control_addr,
        view_only,
    } = out;

    // The window is the point of a console client, so it opens unless the
    // caller explicitly asked for something else: --no-display, or
    // --video-out, which stores the stream without decoding it at all.
    let display = !no_display && video_out.is_none();
    let mut client = rtsp_client::RtspClient::connect(
        &endpoint.addr,
        stream_path,
        &endpoint.username,
        &endpoint.password,
        endpoint.policy,
        CONNECT_TIMEOUT,
    )?;
    log::info!("DESCRIBE returned {} media section(s):", client.media.len());
    for m in &client.media {
        log::info!(
            "  {} {} (payload type {}, control {})",
            m.kind,
            m.encoding,
            m.payload_type,
            m.control
        );
    }

    client.setup_and_play()?;
    log::info!("PLAYING for {seconds}s");

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let decoding = display || decode_to.is_some() || snapshot.is_some();

    // Decoding and storing are alternatives: the first turns the stream back
    // into pictures, the second keeps the elementary stream as it arrived.
    let outcome = if decoding {
        // Input needs a §8 connection: the media stream is one-way. A
        // console that cannot type is still worth showing, so failing to
        // reach the control endpoint downgrades to view-only with the
        // reason, rather than refusing to open at all.
        let control = if display && !view_only {
            match wss::ControlClient::connect(
                &control_addr,
                &endpoint.username,
                &endpoint.password,
                endpoint.policy,
                CONNECT_TIMEOUT,
            ) {
                Ok(c) => {
                    log::info!("input goes to {control_addr}");
                    Some(c)
                }
                Err(e) => {
                    log::warn!("view-only: no control channel at {control_addr}: {e}");
                    None
                }
            }
        } else {
            None
        };

        decode_session(
            &mut client,
            decode_to,
            snapshot,
            display,
            control,
            audio_out.as_deref(),
            deadline,
        )?
    } else {
        store_session(&mut client, video_out, audio_out, deadline)?
    };

    // §7.2: PLAYING --PAUSE--> READY --TEARDOWN--> INIT. Pausing first stops
    // the encoder feed before the channels are freed, so the server is never
    // packetising into a torn-down session.
    client.pause()?;
    client.teardown()?;

    let (received, lost, dropped) = client.stats();
    log::info!(
        "received {} RTP packet(s), {lost} lost, {dropped} fragment(s) dropped",
        received
    );

    Ok(outcome)
}

/// Write the elementary streams to disk, unchanged.
fn store_session(
    client: &mut rtsp_client::RtspClient,
    video_out: Option<std::path::PathBuf>,
    audio_out: Option<std::path::PathBuf>,
    deadline: Instant,
) -> Result<ExitCode, VmmError> {
    let mut sink =
        rtsp_client::FileSink::new(video_out.as_deref(), audio_out.as_deref()).map_err(|e| {
            libvmm_core::MediaError::BadRequest(format!("opening the output files: {e}"))
        })?;

    client.pump(&mut sink, deadline)?;

    log::info!(
        "video: {} access unit(s), {} bytes{}",
        sink.video_units,
        sink.video_bytes,
        video_out
            .map(|p| format!(" -> {}", p.display()))
            .unwrap_or_default()
    );
    log::info!(
        "audio: {} packet(s), {} bytes{}",
        sink.audio_packets,
        sink.audio_bytes,
        audio_out
            .map(|p| format!(" -> {}", p.display()))
            .unwrap_or_default()
    );

    if sink.video_units == 0 && sink.audio_packets == 0 {
        log::warn!("no media arrived; the encoder may not be producing frames");
        return Ok(ExitCode::from(3));
    }
    Ok(ExitCode::SUCCESS)
}

/// Decode the video stream back into frames.
fn decode_session(
    client: &mut rtsp_client::RtspClient,
    decode_to: Option<std::path::PathBuf>,
    snapshot: Option<std::path::PathBuf>,
    display: bool,
    mut control: Option<wss::ControlClient>,
    audio_out: Option<&std::path::Path>,
    deadline: Instant,
) -> Result<ExitCode, VmmError> {
    // Decode whatever the server said it is sending. The SDP is the answer
    // to the capability list this client advertised on DESCRIBE, so a codec
    // appearing here is one this build can decode.
    let codec = negotiated_video_codec(client)?;
    log::info!("decoding {} as announced in the SDP", codec.as_str());

    // All three outputs run together: the window shows the console while
    // --decode-to records it and --snapshot keeps the last frame. Asking for
    // a recording should not cost you the picture.
    let window = if display {
        Some(wayland::WaylandWindow::open("VM console")?)
    } else {
        None
    };
    let raw = match &decode_to {
        Some(path) => Some(decode::RawBgraWriter::create(path)?),
        None => None,
    };
    let fanout = decode::Fanout::new(window, raw, snapshot.is_some());
    let mut sink = decode::DecodingSink::new(codec, fanout, audio_out)?;

    // An interactive console pumps in slices so the window's input can be
    // drained and forwarded between them. The read timeout comes down too:
    // on a quiet stream it is the worst-case delay between a user pressing a
    // key and the guest seeing it.
    let interactive = control.is_some();
    if interactive {
        client.read_timeout = Duration::from_millis(20);
    }

    let mut pumped = Ok(());
    let mut sent = 0u64;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let slice = if interactive {
            (now + Duration::from_millis(20)).min(deadline)
        } else {
            deadline
        };
        pumped = client.pump(&mut sink, slice);
        if pumped.is_err() {
            break;
        }
        if let Some(window) = sink.handler_mut().display.as_mut() {
            if window.closed() {
                break;
            }
            let events = window.drain_input()?;
            if let Some(control) = control.as_mut() {
                for event in events {
                    send_input(control, event)?;
                    sent += 1;
                }
            }
        }
        if !interactive {
            break;
        }
    }

    // A closed window ends the session, so the pump's result is held until
    // that has been checked: the user closing a window is an instruction,
    // not a failure to report.
    let closed = sink.handler().display.as_ref().is_some_and(|w| w.closed());
    if closed {
        log::info!("the window was closed; ending the session");
    } else {
        pumped?;
    }
    if interactive {
        log::info!("sent {sent} input event(s) to the guest");
    }

    report_decode(&sink, audio_out);
    if let Some(window) = &sink.handler().display {
        log::info!("painted {} frame(s)", window.frames);
    }
    if let (Some(path), Some(raw)) = (&decode_to, &sink.handler().raw) {
        log::info!(
            "wrote {} raw BGRA frame(s) -> {}",
            raw.frames,
            path.display()
        );
    }
    if let (Some(path), Some(latest)) = (&snapshot, &sink.handler().latest) {
        latest.write_ppm(path)?;
        log::info!("wrote the last decoded frame -> {}", path.display());
    }
    if let Some((w, h)) = sink.geometry() {
        log::info!(
            "decoded {} frame(s) at {w}x{h} from {} access unit(s)",
            sink.frames_decoded,
            sink.video_units
        );
    }
    Ok(exit_for(&sink))
}

/// Forward one window event to the guest as a §8.5 input frame.
fn send_input(
    control: &mut wss::ControlClient,
    event: wayland::InputEvent,
) -> Result<(), VmmError> {
    match event {
        wayland::InputEvent::Key { code, value } => {
            control.send(|seq| input::key_frame(seq, code, value))?;
        }
        wayland::InputEvent::Pointer {
            x,
            y,
            left,
            right,
            middle,
        } => {
            let buttons = libvmm_control::proto::Buttons {
                left,
                right,
                middle,
            };
            control.send(|seq| input::tablet_frame(seq, x, y, buttons))?;
        }
    }
    Ok(())
}

/// Which video codec the server's SDP announced.
///
/// The server chooses from what this client advertised, so an unrecognised
/// encoding name means the two ends disagree about what was negotiated —
/// which is worth failing on rather than guessing at.
fn negotiated_video_codec(
    client: &rtsp_client::RtspClient,
) -> Result<vmm_codec_sys::VideoCodec, VmmError> {
    let Some(video) = client.media.iter().find(|m| m.kind == "video") else {
        return Err(libvmm_core::MediaError::BadRequest(
            "the SDP announced no video media section".to_string(),
        )
        .into());
    };
    libvmm_media::negotiate::video_from_str(&video.encoding).ok_or_else(|| {
        libvmm_core::MediaError::BadRequest(format!(
            "the server announced video codec {:?}, which this client cannot decode. \
             Advertised: {}",
            video.encoding,
            client.capabilities().to_header()
        ))
        .into()
    })
}

fn report_decode<H: decode::FrameHandler>(
    sink: &decode::DecodingSink<H>,
    audio_out: Option<&std::path::Path>,
) {
    if sink.undecodable_units() > 0 {
        log::info!(
            "{} access unit(s) could not be parsed (normal when joining mid-GOP)",
            sink.undecodable_units()
        );
    }
    if let Some(e) = &sink.last_error {
        log::warn!("last decode error: {e}");
    }
    log::info!(
        "audio: {} packet(s), {} bytes{}",
        sink.audio_packets,
        sink.audio_bytes,
        audio_out
            .map(|p| format!(" -> {}", p.display()))
            .unwrap_or_default()
    );
}

fn exit_for<H: decode::FrameHandler>(sink: &decode::DecodingSink<H>) -> ExitCode {
    if sink.frames_decoded == 0 && sink.audio_packets == 0 {
        log::warn!("no media arrived; the encoder may not be producing frames");
        return ExitCode::from(3);
    }
    if sink.frames_decoded == 0 {
        log::warn!("audio arrived but no video frame decoded");
        return ExitCode::from(4);
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// §9 USB/IP
// ---------------------------------------------------------------------------

fn usbip(allow: Vec<String>, tls: bool, dry_run: bool, stub: bool) -> Result<ExitCode, VmmError> {
    if allow.is_empty() {
        log::error!(
            "--allow is required: the server must never export a device outside its allow-set (§9.2)"
        );
        return Ok(ExitCode::FAILURE);
    }

    let mut server = usbip_server::UsbipServer::new(allow.clone(), tls);
    let found = server.scan_host();
    log::info!(
        "allow-set {allow:?} matched {found} host device(s); serving on [::]:{} ({})",
        server.port,
        server.transport()
    );
    for device in &server.devices {
        log::info!(
            "  {} {:04x}:{:04x} at {}",
            device.info.busid,
            device.info.id_vendor,
            device.info.id_product,
            device.host.devnode().display()
        );
    }
    for busid in &allow {
        if server.devices.iter().any(|d| d.info.busid == *busid) {
            continue;
        }
        if stub {
            log::info!("  {busid} is not present: exporting a placeholder (--stub)");
            server.export(usbip_server::stub_device(busid));
        } else {
            log::warn!("  {busid} is allowed but not present on this host");
        }
    }

    if dry_run {
        log::info!(
            "dry run: OP_REP_DEVLIST would be {} bytes",
            server.devlist().len()
        );
        return Ok(ExitCode::SUCCESS);
    }

    server.serve()?;
    Ok(ExitCode::SUCCESS)
}

fn devices() {
    let found = usbdev::enumerate();
    if found.is_empty() {
        println!("no USB devices found under /sys/bus/usb/devices");
        return;
    }
    println!(
        "{:<10} {:<11} {:<24} {:<10} DESCRIPTORS",
        "BUSID", "VID:PID", "DEVNODE", "CLASS"
    );
    for d in &found {
        // The descriptor blob comes from sysfs and needs no privileges, so
        // its presence is a good proxy for whether the device is readable.
        let descriptors = match usbdev::read_descriptors(d) {
            Some(bytes) => format!("{} bytes", bytes.len()),
            None => "unreadable".to_string(),
        };
        println!(
            "{:<10} {:04x}:{:04x}   {:<24} {:02x}:{:02x}:{:02x}   {descriptors}",
            d.busid,
            d.id_vendor,
            d.id_product,
            d.devnode().display(),
            d.device_class,
            d.device_subclass,
            d.device_protocol
        );
    }
    println!("\nexport one with: vmm-console-client usbip --allow <BUSID>");
}

// ---------------------------------------------------------------------------

/// Build a §8.5 protocol-v1 client frame without connecting.
fn build_frame(
    action: &str,
    seq: u64,
    code: i64,
    value: i64,
    x: i64,
    y: i64,
    path: &str,
) -> Option<String> {
    let frame = match action {
        "input-key" => input::key_frame(seq, code, value),
        "input-move" => input::tablet_frame(seq, x, y, Default::default()),
        "powerdown" => ClientFrame::Powerdown {
            v: PROTOCOL_VERSION,
            seq,
        },
        "reboot" => ClientFrame::Reboot {
            v: PROTOCOL_VERSION,
            seq,
        },
        "backup" => ClientFrame::Backup {
            v: PROTOCOL_VERSION,
            seq,
            path: path.to_string(),
        },
        _ => return None,
    };

    let json = libvmm_control::proto::to_json(&frame)?;
    // Round-trip through the server's own parser so the CLI can never emit a
    // frame the hypervisor would reject.
    match libvmm_control::parse_client_frame(&json) {
        Ok(_) => Some(json),
        Err((seq, e)) => {
            log::error!("{}", ServerFrame::error(seq, &e).to_json());
            None
        }
    }
}

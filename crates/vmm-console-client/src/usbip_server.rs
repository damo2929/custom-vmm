//! The embedded USB/IP server — §9.
//!
//! Binds IPv6 `:3240` cleartext or `:3241` TLS 1.3, serves `OP_REQ_DEVLIST`
//! and `OP_REQ_IMPORT`, then relays `USBIP_CMD_SUBMIT` / `USBIP_CMD_UNLINK`
//! to the real device and answers with `USBIP_RET_SUBMIT` / `USBIP_RET_UNLINK`
//! (§9.2 steps 1–6).
//!
//! Isolation (§9.2) is enforced in two places that cannot be bypassed
//! independently: a busid outside the allow-set is neither listed by
//! `devlist` nor importable.

use crate::usbdev::{self, ClaimedDevice, HostDevice};
use libvmm_core::{UsbipError, VmmResult};
use libvmm_usbip::bridge::{port_for, ImportPolicy};
use libvmm_usbip::wire::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// The accepted connection: cleartext on `:3240`, TLS 1.3 on `:3241` (§9.2).
pub enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

/// A device the server is willing to export.
pub struct Exported {
    pub host: HostDevice,
    pub info: DeviceInfo,
    /// True once a client has imported it; a second import is refused.
    pub imported: bool,
}

/// The server's device table and import policy.
pub struct UsbipServer {
    pub policy: ImportPolicy,
    pub devices: Vec<Exported>,
    pub port: u16,
    use_tls: bool,
}

impl UsbipServer {
    pub fn new(allow: Vec<String>, use_tls: bool) -> Self {
        UsbipServer {
            policy: ImportPolicy::new(allow),
            devices: Vec::new(),
            port: port_for(use_tls),
            use_tls,
        }
    }

    /// Which transport the server is configured for (§9.2).
    pub fn transport(&self) -> &'static str {
        if self.use_tls {
            "TLS 1.3"
        } else {
            "cleartext"
        }
    }

    /// Populate the device table from this host, keeping only what the
    /// allow-set permits.
    pub fn scan_host(&mut self) -> usize {
        self.devices.clear();
        for host in usbdev::enumerate() {
            if !self.policy.permits(&host.busid) {
                continue;
            }
            let info = host.to_wire();
            self.devices.push(Exported {
                host,
                info,
                imported: false,
            });
        }
        self.devices.len()
    }

    pub fn export(&mut self, host: HostDevice) {
        let info = host.to_wire();
        self.devices.push(Exported {
            host,
            info,
            imported: false,
        });
    }

    /// How many exported devices the allow-set actually lets a client see.
    pub fn listed_count(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| self.policy.permits(&d.info.busid))
            .count()
    }

    /// Answer `OP_REQ_DEVLIST` — only devices in the allow-set are listed, so
    /// a client cannot even learn about a device it may not import.
    pub fn devlist(&self) -> Vec<u8> {
        let listed: Vec<&Exported> = self
            .devices
            .iter()
            .filter(|d| self.policy.permits(&d.info.busid))
            .collect();

        let mut out = OpCommon::new(OP_REP_DEVLIST, ST_OK).encode().to_vec();
        out.extend_from_slice(&(listed.len() as u32).to_be_bytes());
        for d in listed {
            out.extend_from_slice(&d.info.encode());
            // Each device is followed by its interface descriptors; a single
            // catch-all interface is enough for the bridge to enumerate.
            out.extend_from_slice(&[
                d.info.device_class,
                d.info.device_subclass,
                d.info.device_protocol,
                0, // padding
            ]);
        }
        out
    }

    /// Answer `OP_REQ_IMPORT`. A busid outside the allow-set is refused with
    /// `ST_NA`, surfacing as `Usbip(ImportDenied)` 7001.
    pub fn import(&mut self, busid: &str) -> VmmResult<Vec<u8>> {
        self.policy.authorize(busid)?;

        let device = self
            .devices
            .iter_mut()
            .find(|d| d.info.busid == busid)
            .ok_or_else(|| -> libvmm_core::VmmError {
                UsbipError::ImportDenied {
                    busid: busid.to_string(),
                }
                .into()
            })?;

        if device.imported {
            return Err(UsbipError::ImportDenied {
                busid: format!("{busid} (already imported by another client)"),
            }
            .into());
        }
        device.imported = true;

        let mut out = OpCommon::new(OP_REP_IMPORT, ST_OK).encode().to_vec();
        out.extend_from_slice(&device.info.encode());
        Ok(out)
    }

    /// Release an import so the device can be exported again.
    pub fn release(&mut self, busid: &str) {
        if let Some(d) = self.devices.iter_mut().find(|d| d.info.busid == busid) {
            d.imported = false;
        }
    }

    /// The refusal reply for a denied import.
    pub fn import_denied() -> Vec<u8> {
        OpCommon::new(OP_REP_IMPORT, ST_NA).encode().to_vec()
    }

    fn host_for(&self, busid: &str) -> Option<&HostDevice> {
        self.devices
            .iter()
            .find(|d| d.info.busid == busid)
            .map(|d| &d.host)
    }

    /// Bind and serve until interrupted.
    ///
    /// `use_tls` selects `:3241` with TLS 1.3 (§9.2). The certificate is
    /// self-signed at start-up, matching the hypervisor's own policy (§8.2):
    /// the cluster TLS authority is out of scope, so `tls_verify_cert = false`
    /// on the bridge side is what makes the pair work today.
    pub fn serve(&mut self) -> VmmResult<()> {
        let tls = if self.use_tls {
            let identity = libvmm_control::tls::SelfSignedIdentity::generate("vmm-usbip-server")?;
            log::info!(
                "USB/IP: TLS 1.3 identity self-signed for {:?}",
                identity.subject_alt_names
            );
            Some(libvmm_control::tls::server_config(&identity)?)
        } else {
            None
        };

        let bind = format!("[::]:{}", self.port);
        let listener = TcpListener::bind(&bind).map_err(|e| -> libvmm_core::VmmError {
            UsbipError::Connect {
                addr: bind.clone(),
                detail: e.to_string(),
            }
            .into()
        })?;
        log::info!(
            "USB/IP server listening on {bind} ({}), exporting {} device(s)",
            self.transport(),
            self.listed_count()
        );

        for connection in listener.incoming() {
            match connection {
                Ok(stream) => {
                    let peer = stream
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    log::info!("client {peer} connected");
                    stream.set_nodelay(true).ok();
                    let result = match &tls {
                        Some(config) => {
                            match rustls::ServerConnection::new(std::sync::Arc::clone(config)) {
                                Ok(connection) => self.serve_stream(&mut Transport::Tls(Box::new(
                                    rustls::StreamOwned::new(connection, stream),
                                ))),
                                Err(e) => Err(libvmm_core::ControlError::Tls(e.to_string()).into()),
                            }
                        }
                        None => self.serve_stream(&mut Transport::Plain(stream)),
                    };
                    if let Err(e) = result {
                        log::warn!("client {peer}: [{} {}] {e}", e.domain(), e.code());
                    }
                    log::info!("client {peer} disconnected");
                }
                Err(e) => log::warn!("accept failed: {e}"),
            }
        }
        Ok(())
    }

    /// Serve exactly one cleartext connection. Exposed so tests can drive the
    /// real protocol path over a socket instead of a mock.
    pub fn serve_connection_for_test(&mut self, stream: TcpStream) -> VmmResult<()> {
        stream.set_nodelay(true).ok();
        self.serve_stream(&mut Transport::Plain(stream))
    }

    /// Serve one client: the op phase, then the URB relay (§9.2).
    ///
    /// Generic over the transport so `:3240` cleartext and `:3241` TLS 1.3
    /// share one implementation — the protocol cannot drift between them.
    fn serve_stream(&mut self, stream: &mut Transport) -> VmmResult<()> {
        let mut header = [0u8; OpCommon::LEN];
        if !read_exact(stream, &mut header)? {
            return Ok(());
        }
        let op = OpCommon::decode(&header)?;
        op.check_version()?;

        match op.code {
            OP_REQ_DEVLIST => {
                write_all(stream, &self.devlist())?;
                Ok(())
            }
            OP_REQ_IMPORT => {
                let mut busid_bytes = [0u8; BUSID_LEN];
                if !read_exact(stream, &mut busid_bytes)? {
                    return Ok(());
                }
                let end = busid_bytes
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(BUSID_LEN);
                let busid = String::from_utf8_lossy(&busid_bytes[..end]).into_owned();

                match self.import(&busid) {
                    Ok(reply) => {
                        write_all(stream, &reply)?;
                        let result = self.relay(stream, &busid);
                        self.release(&busid);
                        result
                    }
                    Err(e) => {
                        log::warn!("refusing import of {busid}: {e}");
                        write_all(stream, &Self::import_denied())?;
                        Err(e)
                    }
                }
            }
            other => Err(UsbipError::BadWire {
                structure: "op_common",
                detail: format!("unexpected opcode {other:#06x}"),
            }
            .into()),
        }
    }

    /// §9.2 steps 5–6: relay URBs between the bridge and the real device.
    fn relay(&mut self, stream: &mut Transport, busid: &str) -> VmmResult<()> {
        let host = self.host_for(busid).cloned_or_err(busid)?;
        let claimed = ClaimedDevice::open(&host)?;
        log::info!(
            "relaying URBs for {busid} ({:04x}:{:04x})",
            host.id_vendor,
            host.id_product
        );

        let mut submitted = 0u64;
        loop {
            let mut header = [0u8; HeaderBasic::LEN];
            if !read_exact(stream, &mut header)? {
                log::info!("bridge closed the connection after {submitted} URB(s)");
                return Ok(());
            }
            let basic = HeaderBasic::decode(&header)?;

            match basic.command {
                USBIP_CMD_SUBMIT => {
                    let mut rest = [0u8; CmdSubmit::LEN - HeaderBasic::LEN];
                    if !read_exact(stream, &mut rest)? {
                        return Ok(());
                    }
                    let mut whole = header.to_vec();
                    whole.extend_from_slice(&rest);
                    let cmd = CmdSubmit::decode(&whole)?;

                    // An OUT transfer carries its data inline.
                    let mut buffer = vec![0u8; cmd.transfer_buffer_length.max(0) as usize];
                    if cmd.payload_len() > 0 && !read_exact(stream, &mut buffer)? {
                        return Ok(());
                    }

                    let endpoint =
                        (cmd.header.ep as u8 & 0x0F) | if cmd.header.is_in() { 0x80 } else { 0x00 };
                    let is_control = cmd.header.ep == 0;
                    let is_interrupt = cmd.interval > 0 && !is_control;

                    let (status, actual) = match claimed.transfer(
                        endpoint,
                        is_control,
                        is_interrupt,
                        &cmd.setup,
                        &mut buffer,
                    ) {
                        Ok(n) => (0, n),
                        Err(e) => {
                            log::debug!("URB {} failed: {e}", cmd.header.seqnum);
                            // -EPIPE is the conventional stall report.
                            (-32, 0)
                        }
                    };

                    let ret = RetSubmit {
                        header: HeaderBasic {
                            command: USBIP_RET_SUBMIT,
                            seqnum: cmd.header.seqnum,
                            devid: cmd.header.devid,
                            direction: cmd.header.direction,
                            ep: cmd.header.ep,
                        },
                        status,
                        actual_length: actual,
                        start_frame: 0,
                        number_of_packets: -1,
                        error_count: 0,
                    };
                    write_all(stream, &ret.encode())?;
                    // An IN transfer returns its data after the header.
                    if cmd.header.is_in() && actual > 0 {
                        write_all(stream, &buffer[..(actual as usize).min(buffer.len())])?;
                    }
                    submitted += 1;
                }
                USBIP_CMD_UNLINK => {
                    let mut rest = [0u8; CmdUnlink::LEN - HeaderBasic::LEN];
                    if !read_exact(stream, &mut rest)? {
                        return Ok(());
                    }
                    let mut whole = header.to_vec();
                    whole.extend_from_slice(&rest);
                    let unlink = CmdUnlink::decode(&whole)?;

                    // Transfers here are synchronous, so by the time an
                    // unlink arrives the URB has already completed: report
                    // -ECONNRESET, which is what the kernel returns for an
                    // unlink that found nothing to cancel.
                    let mut reply = HeaderBasic {
                        command: USBIP_RET_UNLINK,
                        seqnum: unlink.header.seqnum,
                        devid: unlink.header.devid,
                        direction: unlink.header.direction,
                        ep: unlink.header.ep,
                    }
                    .encode()
                    .to_vec();
                    reply.extend_from_slice(&(-104i32).to_be_bytes());
                    reply.extend_from_slice(&[0u8; 24]);
                    write_all(stream, &reply)?;
                }
                other => {
                    return Err(UsbipError::BadWire {
                        structure: "usbip_header_basic",
                        detail: format!("unexpected command {other}"),
                    }
                    .into())
                }
            }
        }
    }
}

/// Small helper so a missing device reads as an import refusal.
trait ClonedOrErr {
    fn cloned_or_err(self, busid: &str) -> VmmResult<HostDevice>;
}

impl ClonedOrErr for Option<&HostDevice> {
    fn cloned_or_err(self, busid: &str) -> VmmResult<HostDevice> {
        self.cloned().ok_or_else(|| {
            UsbipError::ImportDenied {
                busid: busid.to_string(),
            }
            .into()
        })
    }
}

/// Read exactly `buf.len()` bytes. Returns false on a clean end of stream.
fn read_exact(stream: &mut Transport, buf: &mut [u8]) -> VmmResult<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(UsbipError::PeerReset(e.to_string()).into()),
        }
    }
    Ok(true)
}

fn write_all(stream: &mut Transport, bytes: &[u8]) -> VmmResult<()> {
    stream
        .write_all(bytes)
        .and_then(|_| stream.flush())
        .map_err(|e| UsbipError::PeerReset(e.to_string()).into())
}

/// A placeholder descriptor for a device named on the command line, so the
/// devlist and import replies can be produced without host USB enumeration.
pub fn stub_device(busid: &str) -> HostDevice {
    HostDevice {
        busid: busid.to_string(),
        busnum: 1,
        devnum: 1,
        id_vendor: 0,
        id_product: 0,
        bcd_device: 0,
        device_class: 0,
        device_subclass: 0,
        device_protocol: 0,
        num_configurations: 1,
        num_interfaces: 1,
        configuration_value: 1,
        speed: 3,
        sysfs_path: std::path::PathBuf::from(format!("/sys/bus/usb/devices/{busid}")),
    }
}

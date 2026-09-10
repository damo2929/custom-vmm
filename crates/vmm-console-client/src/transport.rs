//! Blocking stream transport, with or without TLS.
//!
//! §8.2 and change-log item 3 mandate TLS 1.3 with no fallback on every
//! listener, so a plaintext connection is only ever used against the USB/IP
//! cleartext port (§9), which the spec does define.

use libvmm_core::{ControlError, VmmResult};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A connected stream, TLS-wrapped where the endpoint requires it.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    /// Connect to `addr`, optionally negotiating TLS 1.3 for `server_name`.
    pub fn connect(
        addr: &str,
        server_name: &str,
        tls: Option<libvmm_control::tls::CertPolicy>,
        timeout: Duration,
    ) -> VmmResult<Self> {
        let target = resolve(addr)?;
        let tcp =
            TcpStream::connect_timeout(&target, timeout).map_err(|e| ControlError::Connect {
                addr: addr.to_string(),
                detail: e.to_string(),
            })?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(timeout)).ok();

        match tls {
            None => Ok(Stream::Plain(tcp)),
            Some(policy) => {
                let config = libvmm_control::tls::client_config(policy)?;
                let name = rustls::pki_types::ServerName::try_from(server_name.to_string())
                    .map_err(|e| {
                        ControlError::Tls(format!("invalid server name {server_name:?}: {e}"))
                    })?;
                let connection = rustls::ClientConnection::new(config, name).map_err(|e| {
                    ControlError::Tls(format!("starting the TLS 1.3 handshake: {e}"))
                })?;
                Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(
                    connection, tcp,
                ))))
            }
        }
    }

    /// Describe the negotiated transport, for the connection log.
    pub fn describe(&self) -> &'static str {
        match self {
            Stream::Plain(_) => "cleartext TCP",
            Stream::Tls(_) => "TLS 1.3",
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) {
        let tcp = match self {
            Stream::Plain(t) => t,
            Stream::Tls(s) => s.get_ref(),
        };
        tcp.set_read_timeout(timeout).ok();
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Resolve `host:port`, accepting the `[::1]:8080` bracket form the spec uses
/// throughout.
pub fn resolve(addr: &str) -> VmmResult<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()
        .map_err(|e| ControlError::Connect {
            addr: addr.to_string(),
            detail: e.to_string(),
        })?
        .next()
        .ok_or_else(|| {
            ControlError::Connect {
                addr: addr.to_string(),
                detail: "resolved to no addresses".into(),
            }
            .into()
        })
}

/// The host part of `host:port`, for TLS SNI and the HTTP `Host` header.
pub fn host_of(addr: &str) -> String {
    match addr.rfind(':') {
        Some(i) if addr.starts_with('[') => {
            addr[..i].trim_matches(|c| c == '[' || c == ']').to_string()
        }
        Some(i) => addr[..i].to_string(),
        None => addr.to_string(),
    }
}

/// Is this an IP literal rather than a DNS name? rustls requires a
/// `ServerName::IpAddress` for literals, which `try_from` handles, but a
/// self-signed certificate will not carry the IP as a SAN — worth saying so.
pub fn is_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

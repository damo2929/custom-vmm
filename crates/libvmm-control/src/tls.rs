//! TLS provisioning — §8.2.
//!
//! TLS 1.3 only, no fallback, on all listeners (change-log item 3). The
//! hypervisor generates and self-signs its certificate at boot.
//!
//! **Out of scope (§8.2):** cluster-wide TLS authority — signing CA, trust
//! distribution, client validation policy — is operated by the cluster and is
//! external to the VM controller and client.
//!
//! The provider is `rustls` over `ring`, which compiles C and assembly. §1.1's
//! no-C rule was lifted for exactly this reason: there is no production-grade
//! pure-Rust TLS 1.3 implementation, and §8.2 permits no cleartext fallback.

use libvmm_core::{ControlError, VmmResult};

/// The only TLS version any listener accepts.
pub const REQUIRED_TLS_VERSION: &str = "1.3";

/// The provider's name, for the boot log.
pub const fn provider_name() -> &'static str {
    "rustls/ring (TLS 1.3 only)"
}

/// A self-signed certificate and its key, generated at boot.
pub struct SelfSignedIdentity {
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub subject_alt_names: Vec<String>,
}

impl SelfSignedIdentity {
    /// Generate a fresh identity for this hypervisor (§8.2).
    ///
    /// The certificate is regenerated on every boot: there is no on-disk key,
    /// because persisting one without the cluster CA to sign it would create
    /// a trust anchor this spec explicitly leaves out of scope.
    pub fn generate(vm_name: &str) -> VmmResult<Self> {
        let mut names = vec!["localhost".to_string()];
        if !vm_name.is_empty() {
            names.push(vm_name.to_string());
        }
        let cert = rcgen::generate_simple_self_signed(names.clone())
            .map_err(|e| ControlError::Tls(format!("generating self-signed certificate: {e}")))?;
        Ok(SelfSignedIdentity {
            certificate_der: cert.cert.der().to_vec(),
            private_key_der: cert.key_pair.serialize_der(),
            subject_alt_names: names,
        })
    }
}

/// Build a server configuration that speaks **only** TLS 1.3.
///
/// rustls is given the TLS 1.3 protocol version alone, so there is no
/// downgrade path even if a client offers 1.2 (change-log item 3).
pub fn server_config(
    identity: &SelfSignedIdentity,
) -> VmmResult<std::sync::Arc<rustls::ServerConfig>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::sync::Arc;

    let certs = vec![CertificateDer::from(identity.certificate_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key_der.clone()));

    let provider = rustls::crypto::ring::default_provider();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| ControlError::Tls(format!("restricting to TLS 1.3: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| ControlError::Tls(format!("installing the self-signed certificate: {e}")))?;

    Ok(Arc::new(config))
}

/// Reject a configured `tls_min` that is not 1.3. The config crate enforces
/// this too; this is the listener-side belt and braces.
pub fn check_tls_min(section: &'static str, tls_min: &str) -> VmmResult<()> {
    if tls_min != REQUIRED_TLS_VERSION {
        return Err(ControlError::Tls(format!(
            "{section}.tls_min = \"{tls_min}\": TLS 1.3 only, no fallback"
        ))
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// How a client validates the hypervisor's certificate.
///
/// §8.2 puts the cluster TLS authority — signing CA, trust distribution,
/// validation policy — out of scope, and the hypervisor self-signs at boot.
/// A client therefore has no trust anchor to check against unless the
/// operator supplies one, which is exactly the situation §9.2's
/// `tls_verify_cert = false` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertPolicy {
    /// Validate against the platform trust store. Correct once a cluster CA
    /// exists; fails against a boot-time self-signed certificate.
    Verify,
    /// Accept any server certificate. Relies on the external cluster TLS
    /// authority (§9.2) and SHOULD NOT be used on untrusted networks: it
    /// stops an active attacker being detected, though the channel is still
    /// encrypted.
    AcceptAny,
}

impl CertPolicy {
    pub const fn from_verify_flag(verify: bool) -> Self {
        if verify {
            CertPolicy::Verify
        } else {
            CertPolicy::AcceptAny
        }
    }

    /// The warning that MUST accompany the permissive policy (§9.2).
    pub const fn warning(self) -> Option<&'static str> {
        match self {
            CertPolicy::Verify => None,
            CertPolicy::AcceptAny => Some(
                "peer certificate validation is disabled: the channel is encrypted but the \
                 server is not authenticated. This SHOULD be true on untrusted networks (§9.2).",
            ),
        }
    }
}

/// Build a client configuration that speaks **only** TLS 1.3.
pub fn client_config(policy: CertPolicy) -> VmmResult<std::sync::Arc<rustls::ClientConfig>> {
    use std::sync::Arc;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| ControlError::Tls(format!("restricting to TLS 1.3: {e}")))?;

    let config = match policy {
        CertPolicy::Verify => {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots_placeholder());
            if roots.is_empty() {
                return Err(ControlError::Tls(
                    "CertPolicy::Verify needs a trust anchor, and no cluster CA is configured \
                     (§8.2 leaves the cluster TLS authority out of scope). Use --insecure to \
                     accept the hypervisor's boot-time self-signed certificate."
                        .to_string(),
                )
                .into());
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        CertPolicy::AcceptAny => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(danger::AcceptAnyServerCert::new(provider)))
            .with_no_client_auth(),
    };

    Ok(std::sync::Arc::new(config))
}

/// There is no bundled trust store: §8.2 says the CA is the cluster's to
/// operate. Kept as a seam so a cluster CA can be dropped in here.
fn webpki_roots_placeholder() -> Vec<rustls::pki_types::TrustAnchor<'static>> {
    Vec::new()
}

mod danger {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};
    use std::sync::Arc;

    /// Accepts any server certificate.
    ///
    /// This is the `tls_verify_cert = false` behaviour of §9.2, and it is
    /// deliberately confined to this module so the one place that skips
    /// authentication is easy to find and audit. Signature verification on
    /// the handshake itself is still performed by the provider — only the
    /// certificate *chain* is unchecked.
    #[derive(Debug)]
    pub struct AcceptAnyServerCert {
        provider: Arc<CryptoProvider>,
    }

    impl AcceptAnyServerCert {
        pub fn new(provider: Arc<CryptoProvider>) -> Self {
            AcceptAnyServerCert { provider }
        }
    }

    impl ServerCertVerifier for AcceptAnyServerCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

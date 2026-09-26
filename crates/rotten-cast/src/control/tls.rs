//! Cast-specific TLS connector.
//!
//! Google Cast receivers present self-signed certificates, so normal WebPKI chain
//! and host-name verification cannot succeed. The exemption in this module is scoped
//! to the Cast control connector only: it never changes process-wide trust, the
//! default `rustls` provider or any other TLS client in the workspace.
//!
//! The verifier still cryptographically verifies the TLS 1.2 / 1.3 handshake
//! signature against the certificate that was presented, using the *ring* provider's
//! supported algorithms. It does **not** authenticate the receiver's identity: an
//! active attacker on the LAN could impersonate a Cast device. Only use this on
//! trusted networks.

use std::sync::{Arc, Once};

use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, ring, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
};

/// Build the Cast control connector with the self-signed-certificate exemption.
pub(crate) fn cast_connector() -> anyhow::Result<TlsConnector> {
    let provider = Arc::new(ring::default_provider());
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|error| anyhow::anyhow!("failed to configure Cast TLS versions: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CastCertVerifier { provider }))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Turn a receiver host into a rustls server name.
///
/// IP literals become [`ServerName::IpAddress`] (no SNI); host names are passed as
/// DNS names. The name is never matched against the certificate.
pub(crate) fn server_name(host: &str) -> anyhow::Result<ServerName<'static>> {
    ServerName::try_from(host.to_owned())
        .map_err(|_| anyhow::anyhow!("invalid Cast receiver host name"))
}

static UNAUTHENTICATED_WARNING: Once = Once::new();

#[derive(Debug)]
struct CastCertVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for CastCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        UNAUTHENTICATED_WARNING.call_once(|| {
            tracing::warn!(
                "Google Cast TLS: certificate chain and host name are NOT verified; \
                 the receiver identity is unauthenticated (trusted LAN only)"
            );
        });
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
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
    ) -> Result<HandshakeSignatureValid, TlsError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_accepts_ip_and_dns() {
        assert!(matches!(
            server_name("192.168.1.20").unwrap(),
            ServerName::IpAddress(_)
        ));
        assert!(matches!(
            server_name("chromecast.local").unwrap(),
            ServerName::DnsName(_)
        ));
        assert!(server_name("").is_err());
    }

    #[test]
    fn verifier_skips_identity_but_advertises_signature_schemes() {
        let provider = Arc::new(ring::default_provider());
        let verifier = CastCertVerifier {
            provider: Arc::clone(&provider),
        };
        let checked = verifier.verify_server_cert(
            &CertificateDer::from(Vec::<u8>::new()),
            &[],
            &ServerName::try_from("cast.example").unwrap(),
            &[],
            UnixTime::since_unix_epoch(std::time::Duration::from_secs(0)),
        );
        assert!(checked.is_ok());
        assert!(!verifier.supported_verify_schemes().is_empty());
    }

    #[test]
    fn connector_builds_with_ring_provider() {
        assert!(cast_connector().is_ok());
    }
}

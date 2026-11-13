use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use shared_protocol::{format_cert_sha256_fingerprint, normalize_fingerprint, ALPN_SFU_QUIC};

/// Result of verifying the SFU TLS leaf certificate during handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TofuCheckResult {
    Trusted,
    UntrustedNew {
        fingerprint: String,
    },
    Mismatch {
        expected: String,
        actual: String,
    },
}

/// Trust-On-First-Use (TOFU) TLS certificate verifier storing the observed SHA-256 fingerprint.
#[derive(Debug)]
pub struct TofuServerCertVerifier {
    expected_norm: Option<String>,
    observed_formatted: Mutex<Option<String>>,
    observed_norm: Mutex<Option<String>>,
    probe_only: bool,
    supported_algs: WebPkiSupportedAlgorithms,
}

impl TofuServerCertVerifier {
    /// Strict verifier requiring an exact SHA-256 fingerprint match.
    pub fn pinned(expected_fingerprint: &str) -> Arc<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Arc::new(Self {
            expected_norm: Some(normalize_fingerprint(expected_fingerprint)),
            observed_formatted: Mutex::new(None),
            observed_norm: Mutex::new(None),
            probe_only: false,
            supported_algs: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        })
    }

    /// Probe verifier that records the peer's SHA-256 fingerprint so the UI can ask the user before trusting.
    pub fn probe() -> Arc<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Arc::new(Self {
            expected_norm: None,
            observed_formatted: Mutex::new(None),
            observed_norm: Mutex::new(None),
            probe_only: true,
            supported_algs: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        })
    }

    pub fn observed_fingerprint(&self) -> Option<String> {
        self.observed_formatted.lock().clone()
    }

    pub fn check_result(&self) -> TofuCheckResult {
        let actual_fmt = self.observed_formatted.lock().clone();
        let actual_norm = self.observed_norm.lock().clone();
        match (&self.expected_norm, actual_fmt, actual_norm) {
            (Some(exp), Some(act_fmt), Some(act_norm)) => {
                if *exp == act_norm {
                    TofuCheckResult::Trusted
                } else {
                    TofuCheckResult::Mismatch {
                        expected: exp.clone(),
                        actual: act_fmt,
                    }
                }
            }
            (None, Some(act_fmt), _) => TofuCheckResult::UntrustedNew {
                fingerprint: act_fmt,
            },
            _ => TofuCheckResult::Trusted,
        }
    }
}

impl ServerCertVerifier for TofuServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let formatted = format_cert_sha256_fingerprint(end_entity.as_ref());
        let normalized = normalize_fingerprint(&formatted);
        *self.observed_formatted.lock() = Some(formatted.clone());
        *self.observed_norm.lock() = Some(normalized.clone());

        if let Some(expected) = &self.expected_norm {
            if *expected == normalized {
                return Ok(ServerCertVerified::assertion());
            }
            return Err(TlsError::General(format!(
                "SFU certificate fingerprint mismatch (expected {expected}, got {formatted})"
            )));
        }

        if self.probe_only {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(format!(
                "Untrusted SFU certificate fingerprint: {formatted}"
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

/// Builds a Quinn `ClientConfig` backed by our TOFU verifier and `screenshare-sfu/1` ALPN.
pub fn build_quic_client_config(verifier: Arc<TofuServerCertVerifier>) -> Result<quinn::ClientConfig> {
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN_SFU_QUIC.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .context("Failed constructing Quinn QUIC client crypto config")?;
    let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_crypto));

    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_uni_streams(512u32.into());
    transport.max_concurrent_bidi_streams(16u32.into());
    transport.datagram_receive_buffer_size(Some(512 * 1024));
    transport.datagram_send_buffer_size(512 * 1024);
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    if let Ok(idle) = std::time::Duration::from_secs(20).try_into() {
        transport.max_idle_timeout(Some(idle));
    }
    client_cfg.transport_config(Arc::new(transport));

    Ok(client_cfg)
}

/// Derives a valid TLS SNI server name for an address host (falling back to `"localhost"` for IP literals).
pub fn sni_for_host(host: &str) -> String {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.parse::<std::net::IpAddr>().is_ok() || trimmed.is_empty() {
        "localhost".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tofu_verifier_match_mismatch_and_probe() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let der = cert.cert.der();
        let fp = format_cert_sha256_fingerprint(der.as_ref());

        let pinned_ok = TofuServerCertVerifier::pinned(&fp);
        assert!(pinned_ok
            .verify_server_cert(
                der,
                &[],
                &ServerName::try_from("localhost").unwrap(),
                &[],
                UnixTime::now(),
            )
            .is_ok());
        assert_eq!(pinned_ok.check_result(), TofuCheckResult::Trusted);

        let pinned_bad = TofuServerCertVerifier::pinned("00:11:22:33");
        assert!(pinned_bad
            .verify_server_cert(
                der,
                &[],
                &ServerName::try_from("localhost").unwrap(),
                &[],
                UnixTime::now(),
            )
            .is_err());
        assert!(matches!(
            pinned_bad.check_result(),
            TofuCheckResult::Mismatch { .. }
        ));

        let probe = TofuServerCertVerifier::probe();
        assert!(probe
            .verify_server_cert(
                der,
                &[],
                &ServerName::try_from("localhost").unwrap(),
                &[],
                UnixTime::now(),
            )
            .is_ok());
        assert_eq!(
            probe.check_result(),
            TofuCheckResult::UntrustedNew { fingerprint: fp }
        );
    }
}

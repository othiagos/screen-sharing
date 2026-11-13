use std::fs;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::{ServerConfig, TransportConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use shared_protocol::{format_cert_sha256_fingerprint, ALPN_SFU_QUIC};

#[derive(Debug, Clone)]
pub enum TlsSource {
    PemFiles {
        cert_path: PathBuf,
        key_path: PathBuf,
    },
    SelfSigned {
        cert_dir: Option<PathBuf>,
        extra_sans: Vec<String>,
    },
}

pub struct PreparedTls {
    pub server_config: ServerConfig,
    pub fingerprint_sha256: String,
    pub cert_der: CertificateDer<'static>,
}

pub fn prepare_server_tls(source: &TlsSource) -> Result<PreparedTls> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (certs, key) = match source {
        TlsSource::PemFiles {
            cert_path,
            key_path,
        } => load_pem_certs_and_key(cert_path, key_path)?,
        TlsSource::SelfSigned {
            cert_dir,
            extra_sans,
        } => load_or_generate_self_signed(cert_dir.as_ref(), extra_sans)?,
    };

    let leaf_der = certs
        .first()
        .context("TLS certificate chain must contain at least one certificate")?
        .clone();
    let fingerprint_sha256 = format_cert_sha256_fingerprint(leaf_der.as_ref());

    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("Failed to configure rustls ServerConfig")?;
    crypto.alpn_protocols = vec![ALPN_SFU_QUIC.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .context("Failed to build Quinn QuicServerConfig")?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_crypto));

    let mut transport = TransportConfig::default();
    transport.max_concurrent_uni_streams(512u32.into());
    transport.max_concurrent_bidi_streams(16u32.into());
    transport.datagram_receive_buffer_size(Some(512 * 1024));
    transport.datagram_send_buffer_size(512 * 1024);
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    if let Ok(idle) = Duration::from_secs(20).try_into() {
        transport.max_idle_timeout(Some(idle));
    }
    server_config.transport_config(Arc::new(transport));

    Ok(PreparedTls {
        server_config,
        fingerprint_sha256,
        cert_der: leaf_der,
    })
}

fn load_pem_certs_and_key(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_file = fs::File::open(cert_path)
        .with_context(|| format!("Failed to open TLS cert file {}", cert_path.display()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to parse PEM certs from {}", cert_path.display()))?;

    let key_file = fs::File::open(key_path)
        .with_context(|| format!("Failed to open TLS key file {}", key_path.display()))?;
    let mut key_reader = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .with_context(|| format!("Failed to parse PEM key from {}", key_path.display()))?
        .with_context(|| format!("No private key found in {}", key_path.display()))?;

    Ok((certs, key))
}

fn load_or_generate_self_signed(
    cert_dir: Option<&PathBuf>,
    extra_sans: &[String],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    if let Some(dir) = cert_dir {
        let cert_path = dir.join("sfu-cert.der");
        let key_path = dir.join("sfu-key.der");
        if cert_path.exists() && key_path.exists() {
            let cert_bytes = fs::read(&cert_path)?;
            let key_bytes = fs::read(&key_path)?;
            let cert_der = CertificateDer::from(cert_bytes);
            let key_der = PrivateKeyDer::Pkcs8(key_bytes.into());
            return Ok((vec![cert_der], key_der));
        }
    }

    let mut sans = vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()];
    for san in extra_sans {
        let trimmed = san.trim();
        if !trimmed.is_empty() && !sans.iter().any(|existing| existing == trimmed) {
            sans.push(trimmed.to_string());
        }
    }

    let certified = rcgen::generate_simple_self_signed(sans)
        .context("Failed to generate self-signed QUIC certificate")?;
    let cert_der_bytes = certified.cert.der().to_vec();
    let key_der_bytes = certified.key_pair.serialize_der();

    if let Some(dir) = cert_dir {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("sfu-cert.der"), &cert_der_bytes)?;
        fs::write(dir.join("sfu-key.der"), &key_der_bytes)?;
    }

    Ok((
        vec![CertificateDer::from(cert_der_bytes)],
        PrivateKeyDer::Pkcs8(key_der_bytes.into()),
    ))
}

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use sfu_server::{spawn_sfu_server, tls::TlsSource, SfuServerConfig};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind_addr: SocketAddr = if let Ok(addr) = std::env::var("SFU_BIND_ADDR") {
        addr.parse()?
    } else if let Ok(addr) = std::env::var("SFU_BIND") {
        addr.parse()?
    } else if let Ok(port) = std::env::var("PORT") {
        format!("0.0.0.0:{port}").parse()?
    } else {
        "0.0.0.0:4433".parse()?
    };

    let auth_token = std::env::var("SFU_AUTH_TOKEN").ok();
    let tls = match (
        std::env::var("SFU_TLS_CERT").ok(),
        std::env::var("SFU_TLS_KEY").ok(),
    ) {
        (Some(cert), Some(key)) => TlsSource::PemFiles {
            cert_path: PathBuf::from(cert),
            key_path: PathBuf::from(key),
        },
        _ => {
            let cert_dir = std::env::var("SFU_CERT_DIR")
                .ok()
                .map(PathBuf::from)
                .or_else(|| Some(PathBuf::from("./.sfu-certs")));
            let extra_sans = std::env::var("SFU_PUBLIC_HOST")
                .ok()
                .map(|h| {
                    h.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            TlsSource::SelfSigned {
                cert_dir,
                extra_sans,
            }
        }
    };

    let config = SfuServerConfig {
        bind_addr,
        auth_token: auth_token.clone(),
        tls,
    };

    let (bound_addr, fingerprint, _state, handle) = spawn_sfu_server(config).await?;
    info!("SFU QUIC server ready on udp://{bound_addr}");
    info!("Server TLS SHA-256 Fingerprint: {fingerprint}");
    if auth_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        info!("SFU_AUTH_TOKEN authentication is ENABLED");
    } else {
        info!("SFU_AUTH_TOKEN is not set (open access; set SFU_AUTH_TOKEN to protect bandwidth)");
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down SFU server...");
        }
        res = handle => {
            if let Err(err) = res {
                tracing::error!("SFU server task exited unexpectedly: {err}");
            }
        }
    }

    Ok(())
}

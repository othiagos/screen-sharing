use std::net::SocketAddr;

use anyhow::Result;
use signaling_server::{build_router, state::SignalingState};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind_addr: SocketAddr = if let Ok(addr) = std::env::var("SIGNALING_BIND_ADDR") {
        addr.parse()?
    } else if let Ok(port) = std::env::var("PORT") {
        format!("0.0.0.0:{port}").parse()?
    } else {
        "0.0.0.0:9001".parse()?
    };

    let state = SignalingState::new();
    let app = build_router(state);
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;

    info!("Signaling server listening on ws://{local_addr}/ws");
    axum::serve(listener, app).await?;
    Ok(())
}

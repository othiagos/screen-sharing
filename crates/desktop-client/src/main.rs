#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use desktop_client::commands::build_tauri_app;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new(
                "info,webrtc=error,webrtc_ice=error,webrtc_sctp=error,webrtc_mdns=error,webrtc_dtls=error,webrtc_srtp=error",
            )
        }))
        .init();

    if let Err(err) = build_tauri_app().run(tauri::generate_context!()) {
        tracing::error!("error while running tauri application: {err}");
    }
}


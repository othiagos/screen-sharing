#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use desktop_client::{config::AppConfig, ui::AppView};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,quinn=warn,rustls=warn")),
        )
        .init();

    let tokio_rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("sfu-tokio")
            .build()
            .expect("failed to initialize Tokio runtime"),
    );

    let initial_config = AppConfig::load();
    let initial_mode = if initial_config.dark_mode {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        Theme::change(initial_mode, None, cx);

        let bounds = Bounds::centered(None, size(px(1240.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };

        let rt = Arc::clone(&tokio_rt);
        let cfg = initial_config.clone();
        if let Err(err) = gpui_kit::open_window(options, cx, move |window, cx| {
            window.set_window_title("Screen Share");
            cx.new(|cx| AppView::new(rt, cfg, window, cx))
        }) {
            tracing::error!("Failed to open main window: {err}");
        }
    });
}

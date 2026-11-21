extern crate gpui_kit as gpui;

rust_i18n::i18n!("locales", fallback = "en");

pub mod config;
pub mod i18n;
pub mod media;
pub mod net;
pub mod ui;

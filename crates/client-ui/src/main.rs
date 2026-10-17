pub mod app;
pub mod components;
pub mod ipc;

use leptos::*;

use crate::app::App;

fn main() {
    mount_to_body(|| view! { <App /> });
}

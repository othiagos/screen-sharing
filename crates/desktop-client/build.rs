use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.clone());

    let dist_dir = workspace_root.join("crates/client-ui/dist");
    let _ = std::fs::create_dir_all(&dist_dir);

    let wasm_release = workspace_root.join("target/wasm32-unknown-unknown/release/client-ui.wasm");
    let wasm_debug = workspace_root.join("target/wasm32-unknown-unknown/debug/client-ui.wasm");
    let src_html = workspace_root.join("crates/client-ui/index.html");
    println!("cargo:rerun-if-changed={}", wasm_release.display());
    println!("cargo:rerun-if-changed={}", wasm_debug.display());
    println!("cargo:rerun-if-changed={}", src_html.display());

    let wasm_input = if wasm_release.exists() {
        Some(wasm_release)
    } else if wasm_debug.exists() {
        Some(wasm_debug)
    } else {
        None
    };

    if let Some(wasm_path) = wasm_input {
        let mut bindgen = wasm_bindgen_cli_support::Bindgen::new();
        if let Ok(b) = bindgen.input_path(&wasm_path).web(true) {
            let _ = b.generate(&dist_dir);
        }
    }

    let src_html = workspace_root.join("crates/client-ui/index.html");
    let dist_html = dist_dir.join("index.html");
    if let Ok(html_content) = std::fs::read_to_string(&src_html) {
        let loader_script = r#"<script type="module">import init from './client-ui.js'; init('./client-ui_bg.wasm');</script>"#;
        let bundled_html = html_content.replace(
            r#"<link data-trunk rel="rust" data-wasm-opt="z" />"#,
            loader_script,
        );
        let _ = std::fs::write(&dist_html, bundled_html);
    }

    tauri_build::build();
}

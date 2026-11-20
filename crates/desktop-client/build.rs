fn main() {
    println!("cargo:rerun-if-changed=icons/app.rc");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");

    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if matches!(target_arch.as_str(), "x86" | "x86_64")
        && std::env::var_os("OPENH264_NO_ASM").is_none()
    {
        let has_nasm = std::process::Command::new("nasm")
            .arg("-v")
            .output()
            .is_ok_and(|o| o.status.success());
        if !has_nasm {
            println!(
                "cargo:warning=NASM nao encontrado no PATH: OpenH264 sera compilado sem assembly x86 SIMD (~3x mais lento). Instale `nasm` para desempenho maximo."
            );
        }
    }

    #[cfg(windows)]
    {
        embed_resource::compile("icons/app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to compile Windows icon resource");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=icons/app.rc");
    println!("cargo:rerun-if-changed=icons/icon.ico");

    #[cfg(windows)]
    {
        embed_resource::compile("icons/app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to compile Windows icon resource");
    }
}

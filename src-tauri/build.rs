fn main() {
    let mut attrs = tauri_build::Attributes::new();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        // tauri-build only embeds its manifest in the app binary, but the dialog plugin needs
        // Common Controls v6 (TaskDialogIndirect) in every executable that links it, test
        // executables included; without it they die with STATUS_ENTRYPOINT_NOT_FOUND. Embed
        // the same manifest through the linker instead, as Tauri's own build script does.
        attrs =
            attrs.windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        let manifest =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
    tauri_build::try_build(attrs).expect("failed to run tauri-build");
}

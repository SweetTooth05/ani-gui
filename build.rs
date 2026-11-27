//! Exposes the Rust target triple, so the updater downloads the release
//! asset built for exactly this platform, and embeds the Windows app icon
//! when building on/for Windows and the `.ico` file is present.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=ANI_GUI_TARGET={target}");
    println!("cargo:rerun-if-env-changed=ANI_GUI_MAL_CLIENT_ID");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/ani-gui.ico");

    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
        );
        let icon = manifest_dir.join("assets/icons/ani-gui.ico");
        if icon.is_file() {
            let out_dir =
                std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
            let rc = out_dir.join("ani-gui.rc");
            let escaped = icon
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            std::fs::write(&rc, format!("1 ICON \"{escaped}\"\n"))
                .expect("writing Windows resource script");
            embed_resource::compile(&rc, embed_resource::NONE)
                .manifest_optional()
                .unwrap();
        }
    }
}

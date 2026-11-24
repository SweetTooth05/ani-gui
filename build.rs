//! Exposes the Rust target triple, so the updater downloads the release
//! asset built for exactly this platform.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=ANI_GUI_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}

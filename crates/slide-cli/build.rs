// `assets.rs` embeds `web/dist` via rust-embed. If the directory is missing
// the macro silently produces an empty asset set — release binaries then
// ship without a UI and only fail at request time. Catch that at build
// time on release profiles by checking for the entry point file.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let index = manifest_dir.join("../../web/dist/index.html");
    println!("cargo:rerun-if-changed={}", index.display());

    let profile = std::env::var("PROFILE").unwrap_or_default();
    if profile != "release" {
        return;
    }

    if !index.exists() {
        eprintln!();
        eprintln!("error: web/dist/index.html is missing.");
        eprintln!();
        eprintln!("Release builds embed web/dist via rust-embed. Run:");
        eprintln!("    (cd web && npm install && npm run build)");
        eprintln!("before `cargo build --release -p slide-cli`.");
        eprintln!();
        std::process::exit(1);
    }
}

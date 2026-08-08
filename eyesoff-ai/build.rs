use std::env;
use std::fs;
use std::path::Path;

/// FNV-1a 64 over every byte of the static shell (the page, the worker, the
/// manifest, the assets). The digest becomes the service worker's cache key,
/// so it has to change exactly when any served shell byte changes and stay
/// identical across rebuilds of identical sources.
fn main() {
    let files = [
        "src/chat.html",
        "src/sw.js",
        "assets/manifest.webmanifest",
        "assets/emoji.woff2",
        "assets/eyesoff.svg",
        "assets/favicon.ico",
        "assets/apple-touch-icon.png",
        "assets/icon-192.png",
        "assets/icon-512.png",
        "assets/icon-maskable-512.png",
    ];
    let root = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let mut h: u64 = 0xcbf29ce484222325;
    for f in files {
        println!("cargo:rerun-if-changed={f}");
        let bytes = fs::read(Path::new(&root).join(f))
            .unwrap_or_else(|e| panic!("asset rev: cannot read {f}: {e}"));
        for b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=ASSET_REV={h:016x}");
}

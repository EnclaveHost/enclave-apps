use std::{env, path::PathBuf, process::Command};
fn main() {
    // The explicit probe/test feature links the same wrapper as RISC Box.
    // The production dependency leaves this to the app build.
    if env::var_os("CARGO_FEATURE_CODEC_TESTS").is_none() {
        return;
    }
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let source = root.join("../vendor/minih264/wrapper.c");
    println!("cargo:rerun-if-changed={}", source.display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("../vendor/minih264/minih264e.h").display()
    );
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let mut cc = Command::new(env::var("RBX_CLANG").unwrap_or_else(|_| "clang".into()));
    if arch.starts_with("wasm") {
        cc.arg(format!("--target={arch}-wasip2"))
            .arg("-nostdlibinc")
            .arg(format!(
                "-I{}",
                root.join("../vendor/minih264/shim").display()
            ));
    }
    assert!(cc
        .args(["-O2", "-DNDEBUG", "-DRBX_SHIELD_VIDEO", "-c"])
        .arg(source)
        .arg("-o")
        .arg(out.join("codec.o"))
        .status()
        .unwrap()
        .success());
    assert!(Command::new("ar")
        .arg("rcs")
        .arg(out.join("libshield_video_test_codec.a"))
        .arg(out.join("codec.o"))
        .status()
        .unwrap()
        .success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=shield_video_test_codec");
}

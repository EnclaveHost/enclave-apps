//! Compile the vendored minih264 wrapper (vendor/minih264/) into a static
//! library cargo bundles into this crate's artifacts. The H.264 encoder is C;
//! everything else here is Rust — see src/video.rs for why it earns its place
//! (the only permissively-licensed codec fast enough to encode the desktop in
//! real time inside the wasm).
//!
//! Wasm targets are compiled freestanding against vendor/minih264/shim (no C
//! sysroot exists at cargo time); the handful of libc symbols resolve at the
//! final link against the wasi-libc the Rust side already carries. The SET
//! build's target spec turns on atomics+bulk-memory; the object must carry
//! the same flags or wasm-ld refuses to mix it into a shared-memory link —
//! CARGO_CFG_TARGET_FEATURE is the signal.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Individual files, not the directory: a directory's mtime moves on
    // entry add/remove, NOT on an edit inside a file, so watching the dir
    // silently shipped stale C once already.
    println!("cargo:rerun-if-changed=vendor/minih264/wrapper.c");
    println!("cargo:rerun-if-changed=vendor/minih264/minih264e.h");
    println!("cargo:rerun-if-changed=vendor/minih264/shim");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let obj = out.join("minih264_wrapper.o");
    let lib = out.join("libminih264wrap.a");

    let mut cc = Command::new(env::var("RBX_CLANG").unwrap_or_else(|_| "clang".into()));
    cc.args(["-O2", "-DNDEBUG", "-c", "vendor/minih264/wrapper.c", "-o"]).arg(&obj);
    if arch == "wasm32" {
        cc.args(["--target=wasm32-wasip2", "-nostdlibinc", "-Ivendor/minih264/shim"]);
        for f in ["atomics", "bulk-memory", "mutable-globals"] {
            if features.split(',').any(|x| x == f) {
                cc.arg(format!("-m{f}"));
            }
        }
    }
    let st = cc.status().expect("clang not found (set RBX_CLANG to a clang that targets wasm32)");
    assert!(st.success(), "minih264 wrapper failed to compile");

    let _ = std::fs::remove_file(&lib);
    let st = Command::new(env::var("RBX_AR").unwrap_or_else(|_| "ar".into()))
        .arg("rcs").arg(&lib).arg(&obj)
        .status()
        .expect("ar not found (set RBX_AR)");
    assert!(st.success(), "ar failed");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=minih264wrap");

    build_opl3(&out, &arch, &features);
    build_enet(&out, &arch, &features);
}

/// Moonlight's ENet fork, for the GameStream control channel.
///
/// Vendored verbatim and compiled here so the wire format matches
/// moonlight-common-c by construction — the client is linked against this exact
/// fork, and a reimplementation that is 99% right is a control channel that
/// connects and then stalls. Only the PROTOCOL core is built: ENet's platform
/// layer (unix.c) is BSD sockets and poll, and the guest reaches the network
/// through wasi:sockets, so those thirteen symbols come from Rust instead
/// (src/gamestream/enet_sys.rs). include/enet/wasi.h stands in for unix.h.
fn build_enet(out: &PathBuf, arch: &str, features: &str) {
    const SRCS: [&str; 7] = [
        "protocol", "host", "peer", "list", "packet", "callbacks", "compress",
    ];
    for f in SRCS {
        println!("cargo:rerun-if-changed=vendor/enet/{f}.c");
    }
    println!("cargo:rerun-if-changed=vendor/enet/include/enet");
    println!("cargo:rerun-if-changed=vendor/enet/shim");

    let mut objs = Vec::new();
    for name in SRCS {
        let src = format!("vendor/enet/{name}.c");
        let obj = out.join(format!("enet_{name}.o"));
        let mut cc = Command::new(env::var("RBX_CLANG").unwrap_or_else(|_| "clang".into()));
        cc.args(["-O2", "-DNDEBUG", "-c", &src, "-o"])
            .arg(&obj)
            .args(["-Ivendor/enet/include"]);
        if arch == "wasm32" {
            // No C sysroot at cargo time, so the handful of libc headers the
            // core needs (memcpy/memset/malloc/free) come from the shim and
            // resolve at the final link against wasi-libc.
            cc.args(["--target=wasm32-wasip2", "-nostdlibinc", "-Ivendor/enet/shim"]);
            for f in ["atomics", "bulk-memory", "mutable-globals"] {
                if features.split(',').any(|x| x == f) {
                    cc.arg(format!("-m{f}"));
                }
            }
        }
        let st = cc.status().expect("clang not found (set RBX_CLANG)");
        assert!(st.success(), "enet/{name}.c failed to compile");
        objs.push(obj);
    }

    let lib = out.join("libenet.a");
    let _ = std::fs::remove_file(&lib);
    let mut ar = Command::new(env::var("RBX_AR").unwrap_or_else(|_| "ar".into()));
    ar.arg("rcs").arg(&lib);
    for o in &objs {
        ar.arg(o);
    }
    assert!(ar.status().expect("ar not found (set RBX_AR)").success(), "ar failed for enet");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=enet");
}

/// Nuked OPL3, for the machine's music. Same freestanding-wasm recipe as
/// minih264 above; see vendor/opl3/wrapper.c for why the synthesis is here
/// and not in the guest.
fn build_opl3(out: &PathBuf, arch: &str, features: &str) {
    println!("cargo:rerun-if-changed=vendor/opl3/wrapper.c");
    println!("cargo:rerun-if-changed=vendor/opl3/opl3.c");
    println!("cargo:rerun-if-changed=vendor/opl3/opl3.h");
    println!("cargo:rerun-if-changed=vendor/opl3/wf_rom.h");

    let mut objs = Vec::new();
    for src in ["vendor/opl3/opl3.c", "vendor/opl3/wrapper.c"] {
        let obj = out.join(format!("{}.o", src.rsplit('/').next().unwrap()));
        let mut cc = Command::new(env::var("RBX_CLANG").unwrap_or_else(|_| "clang".into()));
        cc.args(["-O2", "-DNDEBUG", "-c", src, "-o"]).arg(&obj);
        if arch == "wasm32" {
            // opl3.c's own shim first: it supplies the <stdlib.h> the
            // freestanding wasm build has no sysroot for, and which opl3.c
            // includes without using.
            cc.args(["--target=wasm32-wasip2", "-nostdlibinc", "-Ivendor/opl3/shim",
                     "-Ivendor/minih264/shim", "-Ivendor/opl3"]);
            for f in ["atomics", "bulk-memory", "mutable-globals"] {
                if features.split(',').any(|x| x == f) {
                    cc.arg(format!("-m{f}"));
                }
            }
        }
        let st = cc.status().expect("clang not found (set RBX_CLANG)");
        assert!(st.success(), "opl3 failed to compile: {src}");
        objs.push(obj);
    }

    let lib = out.join("libopl3wrap.a");
    let _ = std::fs::remove_file(&lib);
    let mut ar = Command::new(env::var("RBX_AR").unwrap_or_else(|_| "ar".into()));
    ar.arg("rcs").arg(&lib);
    for o in &objs {
        ar.arg(o);
    }
    assert!(ar.status().expect("ar not found (set RBX_AR)").success(), "ar failed for opl3");
    println!("cargo:rustc-link-lib=static=opl3wrap");
}

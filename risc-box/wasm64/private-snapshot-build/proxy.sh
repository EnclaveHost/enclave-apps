#!/bin/sh
set -eu
umask 022
export W64=/build/tools RUST_TC=nightly-2026-08-07 CARGO_HOME=/build/cargo-user CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_NET_OFFLINE=true TMPDIR=/build/tmp
rustc +$RUST_TC -Vv > /build/out/rustc-container.txt
cmp /build/recipe/rustc-host.txt /build/out/rustc-container.txt
python3 /build/src/risc-box/wasm64/proxy-app.py /build/input/risc-box64-codegen-raw.wasm -o /build/out/risc-box64-private-snapshots.wasm
/build/tools/wasm-tools validate --features all /build/out/risc-box64-private-snapshots.wasm

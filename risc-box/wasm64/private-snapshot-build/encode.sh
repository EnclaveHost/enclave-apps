#!/bin/sh
set -eu
umask 000
rustc +nightly-2026-08-07 -Vv > /build/out/rustc-container.txt
cmp /build/recipe/rustc-host.txt /build/out/rustc-container.txt
/encoder --bound-table /build/out/risc-box64-core.wasm /build/out/risc-box64-core-bounded.wasm
/borrow/wasm-tools-set component new /build/out/risc-box64-core-bounded.wasm -o /build/out/risc-box64-raw-bounded.wasm
/borrow/set-componentize/release/set-componentize /build/out/risc-box64-raw-bounded.wasm -o /build/out/risc-box64-set-bounded.wasm
/encoder /build/out/risc-box64-set-bounded.wasm /build/out/risc-box64-codegen-raw.wasm
/borrow/wasm-tools-set validate --features all /build/out/risc-box64-codegen-raw.wasm

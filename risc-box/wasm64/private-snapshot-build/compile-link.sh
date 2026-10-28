#!/bin/sh
set -eu
export W64=/opt/w64 RUST_TC=nightly-2026-08-07 CARGO_HOME=/build/cargo
export CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/build/target
cd /build/src/risc-box
mkdir -p /build/toolchain /build/out
cp -R --no-preserve=ownership /borrow/sysroot64-set /build/toolchain/
chmod -R a+rX /build/toolchain
mkdir -p /build/clang-resource/lib/wasm64-unknown-wasip2
cp -R --no-preserve=ownership $W64/wasi-sdk/lib/clang/23/include /build/clang-resource/
cp /borrow/libclang_rt.builtins-wasm64.a /build/clang-resource/lib/wasm64-unknown-wasip2/libclang_rt.builtins.a
cp /borrow/wasm-tools-set $W64/wasm-tools
cp /borrow/wac-set $W64/wac
SC="$(rustc +$RUST_TC --print sysroot)/lib/rustlib/wasm64-wasip2-set/lib/self-contained"
mkdir -p "$SC"
for f in crt1-command.o crt1-reactor.o crt1.o libc.a; do ln -sf "/build/toolchain/sysroot64-set/lib/wasm64-wasip2/$f" "$SC/$f"; done
"$W64/wasi-sdk/bin/llvm-ar" rcs "$SC/libunwind.a"
cp Cargo.lock /build/out/Cargo.lock.before
trap 'cp /build/out/Cargo.lock.before Cargo.lock' EXIT
__CARGO_TESTS_ONLY_SRC_ROOT="$W64/rustsrc/library" \
WASM64_AR="$W64/wasi-sdk/bin/llvm-ar" WASM64_CLANG="$W64/wasi-sdk/bin/clang" \
RBX_CLANG="$W64/wasi-sdk/bin/clang" RBX_AR="$W64/wasi-sdk/bin/llvm-ar" \
RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals -L /build/toolchain/sysroot64-set/lib/wasm64-wasip2" \
cargo +$RUST_TC rustc --offline --release --lib --crate-type staticlib --features 'set aot codegen' \
 -Zbuild-std=std,panic_abort -Zjson-target-spec --target wasm64/wasm64-wasip2-set.json \
 --config "patch.crates-io.wit-bindgen.path='$W64/wit-bindgen'" \
 --config "patch.crates-io.wasip2.path='$W64/wasip2'" \
 --config "patch.crates-io.getrandom2.path='$W64/crates/getrandom-0.2.17'" \
 --config "patch.crates-io.getrandom2.package='getrandom'" \
 --config "patch.crates-io.getrandom4.path='$W64/crates/getrandom-0.4.3'" \
 --config "patch.crates-io.getrandom4.package='getrandom'"
cp Cargo.lock /build/out/Cargo.lock.resolved
python3 wasm64/regen_component_type.py /borrow/wasi-libc/libc-bottom-half/sources/wasip2_component_type.o /build/out/wasip2_component_type.o --clang "$W64/wasi-sdk/bin/clang" --target wasm64-wasip2
"$W64/wasi-sdk/bin/llvm-ar" r /build/toolchain/sysroot64-set/lib/wasm64-wasip2/libc.a /build/out/wasip2_component_type.o
"$W64/wasi-sdk/bin/clang" -resource-dir=/build/clang-resource --target=wasm64-wasip2 -pthread --sysroot=/build/toolchain/sysroot64-set \
 -DRISC_CODEGEN set/main.c set/codegen.c /build/target/wasm64-wasip2-set/release/librisc_box.a -O2 \
 -Wl,--skip-wit-component,--shared-memory,--export-table,--growable-table,--export=cabi_realloc,-z,stack-size=1048576,--max-memory=17179869184 \
 -o /build/out/risc-box64-core.wasm

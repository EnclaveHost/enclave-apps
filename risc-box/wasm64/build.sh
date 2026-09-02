#!/bin/sh
# Build risc-box as a wasm64 (memory64) wasip2 component: a machine host that
# can address more than 4 GiB — guests bigger than the wasm32 line, and more
# of them per process.
#
# What is different from the ordinary `cargo build --target wasm32-wasip2`:
#
#   * there is no wasm64-wasip2 target in rustc: wasm64-wasip2.json here is
#     the wasm32-wasip2 spec with a 64-bit arch/data-layout/pointer width and
#     `-mwasm64` for the linker; std is built from source (-Zbuild-std) from a
#     COPY of rust-src widened for wasm64 (std-wasm64.sh);
#   * the libc is wasi-libc built for wasm64-wasip2 with the platform's
#     wasm64 patch (musl's 32-bit-pointer atomics assumption), plus its
#     component-type object regenerated for wasm64 (regen_component_type.py),
#     installed as the target's self-contained sysroot;
#   * wit-bindgen's cabi_realloc runtime ships as a wasm32 object and its
#     runtime helpers are gated on wasm32; the patch in patches/ builds the
#     object for wasm64 with clang and widens the gates to any wasm;
#   * the vendored C (OPL3, ENet, minih264) is compiled by build.rs with
#     RBX_CLANG, here the wasi-sdk clang, for --target=wasm64-wasip2;
#   * the wasip2 crate (wasi 0.2 bindings; std's and getrandom 0.4's) stubs
#     every import with unreachable!() off wasm32; $W64/wasip2 is the same
#     crate with those gates widened, patched into both the std graph
#     (std-wasm64.sh appends it to the library workspace) and the app graph;
#   * getrandom 0.2 imports preview1 `random_get`, which rustc's wasm32
#     adapter module cannot serve on a 64-bit memory (and wasm-ld keeps an
#     import-module'd undefined symbol apart from a same-named definition, so
#     no shim can catch it): the patched copy asks wasi 0.2 `random` instead;
#   * getrandom (both the 0.2 and 0.4 lines in the graph) gates its WASI
#     backend on target_arch = "wasm32"; the copies in $W64/crates are the
#     same sources with that gate widened to target_family = "wasm"
#     (patches/getrandom-*.patch), patched in by name for each version;
#   * wit-component (the component encoder) typed every canonical pointer as
#     i32; patches/wit-component-memory64.patch makes it follow the module's
#     memory width, so `wasm-tools component new` (built from that tree) is
#     the encode step, and rustc's own linker stops at the core module
#     (--skip-wit-component).
#
# The engine (wasmtime 49) parses and runs memory64 components with
# `-W memory64,component-model-memory64`, but its HOST-side typed canonical
# ABI still reads every pointer and length as 32 bits (wasmtime FIXME #4311):
# a wasm64 component calling WASI directly gets its return areas misread.
# What the engine does implement completely is the component-to-component
# adapter (FACT), which transcodes between a 64-bit caller and a 32-bit
# callee. So the last step plugs the app into wasm64/wasiproxy: a wasm32
# component exporting every stable wasi 0.2.12 interface by forwarding to
# the identically-named import. The host only ever sees a 32-bit caller.
# (`wac plug`, not `wasm-tools compose`: the latter cannot remap resource
# types across the boundary.)
#
# prepare-toolchain.sh builds everything this needs into $W64 once.
set -e
cd "$(dirname "$0")/.."
W64="${W64:-$HOME/.cache/risc-box-w64}"
RUST_TC="${RUST_TC:-nightly}"   # the toolchain name (a dated nightly in the Dockerfile)
OUT="${OUT:-wasm64/risc-box64.wasm}"
for need in "$W64/wasi-sdk/bin/clang" "$W64/sysroot64/lib/wasm64-wasip2/libc.a" \
            "$W64/wasm-tools" "$W64/wac" "$W64/rustsrc/library/Cargo.toml" "$W64/wit-bindgen/build.rs" \
            "$W64/crates/getrandom-0.2.17/Cargo.toml" "$W64/crates/getrandom-0.4.3/Cargo.toml"; do
  [ -e "$need" ] || { echo "[w64] missing $need — run wasm64/prepare-toolchain.sh first"; exit 2; }
done

# The self-contained sysroot rustc links for a custom target lives under the
# toolchain's rustlib/<target-name>/lib/self-contained. Point it at ours.
SYSROOT="$(rustc "+$RUST_TC" --print sysroot)"
SC="$SYSROOT/lib/rustlib/wasm64-wasip2/lib/self-contained"
mkdir -p "$SC"
for f in crt1-command.o crt1-reactor.o crt1.o libc.a; do
  ln -sf "$W64/sysroot64/lib/wasm64-wasip2/$f" "$SC/$f"
done
[ -f "$SC/libunwind.a" ] || "$W64/wasi-sdk/bin/llvm-ar" rcs "$SC/libunwind.a"

# The [patch] entries below re-resolve the graph, and cargo writes that
# resolution back to Cargo.lock; the wasm32 build would then flip it back.
# Keep the checked-in lock: restore it whatever happens.
cp Cargo.lock "${TMPDIR:-/tmp}/risc-box-Cargo.lock.w64"
trap 'cp "${TMPDIR:-/tmp}/risc-box-Cargo.lock.w64" Cargo.lock' EXIT

echo "[w64] building the core module (std from source, wasm64-wasip2)"
__CARGO_TESTS_ONLY_SRC_ROOT="$W64/rustsrc/library" \
WASM64_AR="$W64/wasi-sdk/bin/llvm-ar" WASM64_CLANG="$W64/wasi-sdk/bin/clang" \
RBX_CLANG="$W64/wasi-sdk/bin/clang" RBX_AR="$W64/wasi-sdk/bin/llvm-ar" \
RUSTFLAGS="-C link-arg=--skip-wit-component ${RUSTFLAGS:-}" \
  cargo "+$RUST_TC" build --release -Zbuild-std=std,panic_abort -Zjson-target-spec \
    --target wasm64/wasm64-wasip2.json \
    --config "patch.crates-io.wit-bindgen.path='$W64/wit-bindgen'" \
    --config "patch.crates-io.wasip2.path='$W64/wasip2'" \
    --config "patch.crates-io.getrandom2.path='$W64/crates/getrandom-0.2.17'" \
    --config "patch.crates-io.getrandom2.package='getrandom'" \
    --config "patch.crates-io.getrandom4.path='$W64/crates/getrandom-0.4.3'" \
    --config "patch.crates-io.getrandom4.package='getrandom'" \
    ${EXTRA_FEATURES:+--features "$EXTRA_FEATURES"}
CORE=target/wasm64-wasip2/release/risc-box.wasm
[ -f "$CORE" ] || { echo "[w64] no core module at $CORE"; exit 1; }

echo "[w64] encoding the component"
RAW="${OUT%.wasm}-raw.wasm"
"$W64/wasm-tools" component new "$CORE" -o "$RAW"
"$W64/wasm-tools" validate --features all "$RAW"

echo "[w64] building the wasi pass-through proxy (wasm32)"
# the proxy is its own crate; its target dir follows CARGO_TARGET_DIR when
# set (a container build redirects everything), else lives next to it
PROXY_TD="${CARGO_TARGET_DIR:-$PWD/wasm64/wasiproxy/target}"
case "$PROXY_TD" in /*) ;; *) PROXY_TD="$PWD/$PROXY_TD";; esac
( cd wasm64/wasiproxy
  # regenerate the forwarding crate when the WIT moved (the checked-in
  # src/lib.rs is that output; gen.py is deterministic)
  if [ -x "$W64/wasm-tools" ]; then
    "$W64/wasm-tools" component wit --json wit > "${TMPDIR:-/tmp}/wasiproxy.json"
    python3 gen.py "${TMPDIR:-/tmp}/wasiproxy.json" > "${TMPDIR:-/tmp}/wasiproxy-lib.rs"
    cmp -s "${TMPDIR:-/tmp}/wasiproxy-lib.rs" src/lib.rs || {
      echo "[w64] wasiproxy/src/lib.rs regenerated from wit/"; cp "${TMPDIR:-/tmp}/wasiproxy-lib.rs" src/lib.rs; }
  fi
  cargo build --release --target wasm32-wasip2 --target-dir "$PROXY_TD" )
PROXY="$PROXY_TD/wasm32-wasip2/release/wasiproxy.wasm"

echo "[w64] plugging the app into the proxy"
"$W64/wac" plug --plug "$PROXY" "$RAW" -o "$OUT"
"$W64/wasm-tools" validate --features all "$OUT"

# Every WASI import the app makes must now be answered by the proxy: the
# composed component may import only what the proxy itself imports. An
# app import the proxy does not export would stay a direct host import and
# be misread at runtime, so this is fatal, not a warning.
python3 - "$OUT" "$PROXY" "$RAW" "$W64/wasm-tools" <<'PY'
import subprocess, sys, re
def imports(p):
    out = subprocess.run([sys.argv[4], "component", "wit", p], capture_output=True, text=True, check=True).stdout
    return set(re.findall(r"^\s*import ([^;]+);", out, re.M))
out, proxy, raw = map(imports, sys.argv[1:4])
stray = out - proxy
assert not stray, f"app imports bypass the proxy: {sorted(stray)}"
print(f"[w64] app imported {len(raw)} interfaces; all plugged; composed imports = proxy's {len(out)}")
b = open(sys.argv[1], "rb").read()
assert b[:4] == b"\x00asm", "not wasm"
layer = b[6] | (b[7] << 8)
assert layer == 1, f"expected a component, got layer {layer}"
print(f"[w64] {sys.argv[1]}: {len(b):,} bytes, component, memory64, proxied")
PY

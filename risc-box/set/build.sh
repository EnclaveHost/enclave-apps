#!/bin/sh
# Build risc-box as a shared-everything-threads component: real OS threads on
# real cores inside one component instance, so watching the machine stops
# costing the machine (see src/worker.rs).
#
# Why this is not just `cargo build --target wasm32-wasip2`:
#
#   * the stock wasip2 target sets `singlethread: true`, which strips the
#     atomics the shared memory needs — hence the generated spec below;
#   * the precompiled core/std for wasip2 are built WITHOUT atomics and cannot
#     be linked into a shared-memory module — hence `-Zbuild-std`;
#   * `set-componentize` refuses to encode a component that does not EXPORT
#     `cabi_realloc`, and nothing in a staticlib link pulls std's own copy out
#     of the archive — hence the explicit export;
#   * the final link needs --shared-memory/--max-memory/--export-table and the
#     componentize pass, all of which the platform's clang wrapper already
#     owns — so we hand it the staticlib rather than reimplementing them.
#
# rustc must be on LLVM 23 to match the sysroot's clang 23; older nightlies
# fail the link with "object file uses globals for thread context".
set -e
cd "$(dirname "$0")/.."

IMG="${IMG:-enclave-wasipsetc-build:local}"
OUT="${OUT:-set/risc-box-set.wasm}"
WORK="${WORK:-target/set}"
mkdir -p "$WORK"

# The SET sysroot lives in the toolchain image; take it from there rather than
# keeping a copy in the repo, so it cannot drift from the image that links it.
if [ ! -d "$WORK/sysroot" ]; then
  echo "[set] extracting the SET sysroot from $IMG"
  cid=$(docker create "$IMG" true)
  docker cp "$cid:/opt/wasip2-set-sysroot" "$WORK/sysroot" >/dev/null
  docker rm -f "$cid" >/dev/null
fi

# wasip2, minus the single-thread assumption, plus the feature set the C
# wrapper forces. Generated from rustc's own spec so it tracks the toolchain
# instead of being hand-maintained.
echo "[set] generating the target spec"
rustc -Z unstable-options --print target-spec-json --target wasm32-wasip2 > "$WORK/p2.json"
python3 - "$WORK/p2.json" "$WORK/wasm32-wasip2-set.json" <<'PY'
import json, sys
spec = json.load(open(sys.argv[1]))
spec.pop("is-builtin", None)
spec["singlethread"] = False
spec["features"] = "+atomics,+bulk-memory,+mutable-globals"
json.dump(spec, open(sys.argv[2], "w"), indent=2)
PY

# --lib only, deliberately. Building the bin as well would ask rust-lld to
# produce a finished component WITHOUT --shared-memory, and the SET libc's
# pthread objects reference TLS symbols (__tls_align, __tls_size,
# __wasm_init_tls) that wasm-ld only synthesizes for a shared-memory link. The
# staticlib has no such link step: the SET wrapper does it, with the flags.
echo "[set] building the staticlib"
SYSROOT="$PWD/$WORK/sysroot"
# The `wasi` patch is applied HERE rather than in Cargo.toml, on purpose: a
# [patch] table cannot be feature-gated, and this must not touch the ordinary
# wasip2 component that ships. See set/wasi-p2-shim for why preview1 has to be
# absent from a SET build entirely.
#
# It does, however, rewrite Cargo.lock — the patched `wasi` loses its registry
# source and checksum — and leaving that behind would break the ordinary build,
# which still wants the real crate. So the lockfile is restored on the way out,
# including on failure.
if [ -f Cargo.lock ]; then
  cp Cargo.lock "$WORK/Cargo.lock.orig"
  # shellcheck disable=SC2064
  trap "cp '$PWD/$WORK/Cargo.lock.orig' '$PWD/Cargo.lock'" EXIT INT TERM
fi
RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals -L $SYSROOT/lib/wasm32-wasip2" \
  cargo +nightly build --release --lib --features set \
    --config "patch.crates-io.wasi.path='$PWD/set/wasi-p2-shim'" \
    --target "$PWD/$WORK/wasm32-wasip2-set.json" \
    -Zbuild-std=std,panic_abort -Zjson-target-spec

LIB="target/wasm32-wasip2-set/release/librisc_box.a"
[ -f "$LIB" ] || { echo "[set] no staticlib at $LIB"; exit 1; }

echo "[set] linking through the SET toolchain"
mkdir -p "$WORK/link"
cp "$LIB" "$WORK/link/librisc_box.a"
cp set/main.c "$WORK/link/main.c"
# -z stack-size, because the default is wasm-ld's 64 KiB and this app does not
# fit in it. Rust's own link passes 1 MiB (`-z stack-size=1048576`) and the C
# wrapper has no reason to, so a Rust program linked through it gets a stack an
# order of magnitude smaller than the one it was compiled against. The failure
# is not a nice message — the guard page is just linear memory, so it surfaces
# as `wasm trap: out of bounds memory access` inside whatever function was
# unlucky (here `std::env::var`, which looks like an env bug and is not).
# --max-memory, because the wrapper's default is 1 GiB and this app does not
# fit in it. A shared memory must declare its maximum at LINK time and can
# never grow past it, so the ceiling is fixed in the binary — unlike the
# ordinary build, where wasmtime's `-W max-memory-size` (the deployment's own
# RAM slice) is the only limit. The guest's DRAM alone is 512 MiB; add a
# decompressed rootfs (128 MiB), the image being fetched, and the framebuffer
# buffers, and a clean boot fits in 1 GiB while a boot whose image fetch
# RETRIES does not:
#
#     memory allocation of 536870912 bytes failed   (Mmu::init_memory)
#
# Raising it costs nothing: the engine still enforces the real per-deployment
# ceiling, so this only stops the LINKER from being the smaller of the two.
# Passed after the wrapper's own flag, which is what makes it win.
#
# Now at the wasm32 maximum (4 GiB = 65536 pages) rather than 3 GiB, because a
# configurable `ramMiB` moved the ceiling within reach: booting the alpine
# desktop image (528 MiB) with ramMiB=1792 peaks at ~2.85 GiB, since the disk
# is briefly held twice while virtio packs it into u64 cells. That left ~220
# MiB for everything else, and a shared memory can NEVER grow past its
# link-time max — so a fetch retry or a larger image would trap. The engine's
# own -W max-memory-size (the deployment's RAM slice) is the real limit.
docker run --rm -v "$PWD/$WORK/link":/src "$IMG" \
  main.c librisc_box.a -O2 -Wl,--export=cabi_realloc -Wl,-z,stack-size=1048576 \
  -Wl,--max-memory=4294967296 -o out.wasm
cp "$WORK/link/out.wasm" "$OUT"

python3 - "$OUT" <<'PY'
import sys
b = open(sys.argv[1], "rb").read()
assert b[:4] == b"\x00asm", "not wasm"
layer = b[6] | (b[7] << 8)
assert layer == 1, f"expected a component, got layer {layer}"
assert b"[set-spawn-indirect]" in b, "SET spawn not wired - set-componentize did not run?"
print(f"[set] {sys.argv[1]}: {len(b):,} bytes, component, SET spawn wired")
PY

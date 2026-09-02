#!/bin/sh
# Patch a copy of rust-src's `library/` so std builds for wasm64-wasip2.
#
# std has a handful of `target_arch = "wasm32"` gates that mean "any wasm"
# (fd duplication, the futex/thread shims, the allocator selection); on a
# wasm64 target they select the wrong arm or nothing. Widen exactly those to
# `any(wasm32, wasm64)`. Everything else in std already knows wasm64.
#
# std's WASI bindings come from the `wasip2` and `wit-bindgen` crates, whose
# published sources stub every import with unreachable!() off wasm32 and
# ship a wasm32-only cabi_realloc; the library workspace gets a [patch]
# pointing both at the toolchain's widened copies (prepare-toolchain.sh).
#
#   std-wasm64.sh <rust-src library dir> <output dir> <W64 toolchain dir>
set -e
src="$1"; out="$2"; w64="$3"
[ -d "$src/std" ] || { echo "usage: std-wasm64.sh <rust-src/library> <out> <W64>"; exit 2; }
[ -d "$w64/wasip2" ] && [ -d "$w64/wit-bindgen" ] || { echo "std-wasm64.sh: $w64/wasip2 and $w64/wit-bindgen must exist first"; exit 2; }
rm -rf "$out" && mkdir -p "$out" && cp -r "$src"/. "$out"/
for f in std/src/os/fd/owned.rs std/src/os/unix/io/mod.rs std/src/sys/thread/wasm.rs \
         std/src/sys/alloc/mod.rs std/src/sys/sync/futex/wasm.rs std/src/os/fd/raw.rs; do
  sed -i 's/target_arch = "wasm32"/any(target_arch = "wasm32", target_arch = "wasm64")/g; s/any(any(target_arch = "wasm32", target_arch = "wasm64"), target_arch = "wasm64")/any(target_arch = "wasm32", target_arch = "wasm64")/g' "$out/$f"
done
# The library workspace already has a [patch.crates-io] table (the
# rustc-std-workspace shims); add ours under the same header.
python3 - "$out/Cargo.toml" "$w64" <<'PYIN'
import sys
p, w64 = sys.argv[1], sys.argv[2]
s = open(p).read()
add = ('[patch.crates-io]\n'
       f'wasip2 = {{ path = "{w64}/wasip2" }}\n'
       f'wit-bindgen = {{ path = "{w64}/wit-bindgen" }}\n')
assert '[patch.crates-io]' in s, "library/Cargo.toml has no [patch.crates-io] table"
s = s.replace('[patch.crates-io]\n', add, 1)
open(p, 'w').write(s)
PYIN
echo "std patched for wasm64 at $out (wasip2 + wit-bindgen patched from $w64)"

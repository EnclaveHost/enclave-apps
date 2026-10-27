# Qualified private-snapshot build

The production candidate preserves memory64, SET shared threads, the 149-region
AOT dispatcher and dynamic code generation. The ordinary wasm32-wasip2 build is
a compatibility test artifact, not a replacement for this production profile.
`qualified.json` records the compiled implementation, artifact and runtime pins.
Later commits add test/recipe/documentation files without changing compiled app
or crypto sources.

These scripts preserve the commands actually qualified on 2026-10-27. They
require retained custom compiler/SDK/encoder inputs; they are not a claim that a
stock Rust installation or a fresh checkout alone reproduces the component.
The container image must already exist at the exact digest in qualified.json.
Run offline, with no network, no added capabilities and no swap. Limit CPU,
container memory, total scratch, root free space and wall time externally.

Mount conventions:

- `/build/src` contains the exported `risc-box` and `snapshot-crypto` directories.
- `/borrow` is the read-only retained SET toolchain: sysroot64-set, wasm-tools-set,
  wac-set, libclang_rt.builtins-wasm64.a, wasi-libc component type object and
  set-componentize/release/set-componentize.
- `/encoder` is the pinned codegen-componentize executable, read-only.
- `/build/recipe` contains these scripts and a `rustc-host.txt` compiler identity.
- The image supplies `/opt/w64`, `/opt/rustup` and the patched Rust source.
- Cargo cache and index are read-only; extracted registry source and all build
  outputs live in the private writable Cargo home. The exact matching compiler's
  wasm32-unknown-unknown standard library is mounted read-only for the proxy.

First run `compile-link.sh`, then `encode.sh` in the private build container.
The lockfile is restored after patched dependency resolution; preserve the
resolved copy from `/build/out` as build evidence. This stage peaked at 5.32 GiB
under an 8 GiB cap, one CPU. The former 3 GiB attempt was killed at its cap.

Package the resulting `risc-box64-codegen-raw.wasm` in a separate fresh container
using the host UID/GID and `proxy.sh`. Mount source, raw input and pinned tools
read-only. `/build/tools` holds wasm-tools and wac. **Precreate** the writable
Cargo home and `registry/{cache,index,src}` as that UID before Docker creates
read-only mount targets. Keep executable permissions on tools. The proxy's
TemporaryDirectory is mode 0700: using the host UID keeps resource enumeration
accurate. Do not hide permission failures or modify borrowed inputs.

Packaging peaked at 716 MiB under a 1536 MiB cap and finished in 30.7 seconds.
The final validation passed and its 26 WASI imports were preserved exactly;
`enclave:codegen/compiler@0.1.0` remains a native host import.

## HTTP qualification

`../../scripts/private-snapshot-http-test.py` exercises authentication, encrypted
S3 PUT, restore-path acceptance, tampering, explicit cold boot, missing-key refusal
and wrong-owner refusal using only a synthetic RISC guest and a loopback S3
fixture. With the SET runtime it also requires the actual main-thread and SET
worker generated-code/revocation selftests to pass and verifies JIT is enabled.
The separate Rust unit test proves instruction, RAM and disk-delta continuation.

```sh
python3 risc-box/scripts/private-snapshot-http-test.py   --artifact /absolute/path/to/risc-box64-private-snapshots.wasm   --sha256 43adfb522f3de592d04129457ea7f729c5a592cf295824bf817be55458b840d6   --set-runtime-dir /absolute/path/to/pinned/runtime-set   --output-dir /absolute/path/to/new-empty-results
```

Use a whole-process-group/cgroup timeout and memory guard: the qualified run
used 4 GiB, one CPU and 300 seconds, peaked at 604 MiB and took 83.4 seconds.
The fixture disables DNS and UDP, bypasses HTTP proxies and gives the runtime
only its test configuration. The runtime CLI grants broad network capability;
all fixture URLs are loopback. No real credentials or model graphs are used.

For the ordinary WASI component, omit `--set-runtime-dir` and select `--wasmtime`.
Its existing wasi:nn imports require the nn linker feature, even though the
fixture does not load any model. Existing GameStream identity writes are counted
separately from snapshot writes and listed by path/size only in puts.json.

This demonstrates compatibility with the pinned **historical** SET runtime.
It does not prove current fleet runtime equivalence, a live desktop restore,
private secret delivery, migration or native KVM checkpoint support. Keep those
release gates distinct. Preserve original snapshots until encrypted copies and
application restore are verified. Never put a snapshot master key in catalog
configuration, build logs or a public artifact.

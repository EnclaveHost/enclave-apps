# wasiproxy: the wasm32 pass-through under a memory64 component

A wasm64 (memory64) wasip2 component cannot talk to the engine's WASI host
functions directly: wasmtime 49's host-side typed canonical ABI still reads
every pointer and length as 32 bits (wasmtime FIXME #4311), so anything
returned through a return-area, or living above 4 GiB, is misread. What the
engine *does* implement completely is the component-to-component adapter
("FACT" trampolines), which transcode between a 64-bit caller and a 32-bit
callee.

So the wasm64 app is composed on top of this component: a wasm32 component
that imports every stable wasi 0.2.12 interface from the host and exports
the same interfaces, forwarding each call verbatim. The app's WASI imports
are plugged into the proxy's exports (`wac plug`), the proxy's imports
become the composed component's imports, and the host only ever sees a
32-bit caller.

Costs: one extra copy per list/string crossing (the adapter copies between
the two linear memories), and one resource-handle indirection per stream,
socket or pollable (the proxy wraps each host handle in its own resource).

Files:

* `wit/deps/*.wit`: the wasi 0.2.12 packages as shipped in the `wasip2`
  crate, with every `@unstable`-gated item removed (`strip_unstable.py`);
  the proxy only ever exports the stable surface, and the build's
  wit-bindgen refuses gated world imports.
* `wit/world.wit`: the world — import + export of every interface.
* `gen.py`: emits `src/lib.rs` from the resolved WIT JSON
  (`wasm-tools component wit --json wit`). One wrapper struct per exported
  resource holding the imported handle, one `Guest` impl per interface,
  typed converters between the import-side and export-side Rust types.
* `src/lib.rs`: the generated crate (checked in so a plain cargo build
  reproduces the proxy; `wasm64/build.sh` regenerates it when the WIT
  changes).

Build: `cargo build --release --target wasm32-wasip2` (stock toolchain; the
proxy itself is ordinary wasm32).

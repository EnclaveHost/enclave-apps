//! image-generator: text-to-image as a wasm component on Enclave's wasi-nn
//! GPU interface. Ships NO weights - models arrive as attached Modelwrap
//! volumes served through the host's stable-diffusion.cpp backend (the node
//! preloads each volume's checkpoint components at startup; the guest
//! load_by_name()s them and one compute() runs the whole pipeline). The
//! default catalog serves z-image-turbo (Tongyi Z-Image-Turbo, 6B) and
//! qwen-image-2512 (Qwen-Image-2512 + Lightning 8-step, 20B - the flagship).
//!
//! Routes (see src/app.rs):
//!   GET  /            - image playground (self-contained HTML; shows a
//!                       model dropdown when the config lists several).
//!   GET  /ping        - liveness, touches no wasi-nn.
//!   GET  /info        - config the UI needs (steps, sizes, target, and the
//!                       `models` catalog with per-model limits).
//!   GET  /warmup      - load the weights and run one tiny generation so the
//!                       model is resident before the first real prompt
//!                       (?target=gpu|cpu, ?size=, ?model=). The playground
//!                       fires it on page load.
//!   GET  /image       - ?prompt=...&steps=&seed=&w=&h=&target=&model= ->
//!                       image/png.
//!   POST /generate    - {prompt, model?, steps?, seed?, width?, height?,
//!                       target?, ancestral?, cfg?} -> SSE: {status} lines
//!                       while loading/generating, then {done, image:
//!                       <b64 png>, model, seed, timings}. The playground's
//!                       endpoint.
//!   POST /v1/images/generations - OpenAI-compatible: {prompt, model?, n?,
//!                       size?, seed?} -> {created, data: [{b64_json, seed}]}.
//!                       Always returns b64_json (no url storage in an
//!                       ephemeral enclave). If the config sets api_key,
//!                       requires `Authorization: Bearer <key>`.
//!
//! `model` selects an entry from the config's `models` catalog (each entry
//! overlays the top-level template - see src/config.rs); absent means the
//! largest attached model.
//!
//! The config module is host-compilable so `cargo test` runs natively;
//! everything touching wasi bindings is gated to wasm32.

#[cfg(target_arch = "wasm32")]
#[allow(warnings)]
mod bindings;

pub mod config;

#[cfg(target_arch = "wasm32")]
mod pipeline;

#[cfg(target_arch = "wasm32")]
mod app;

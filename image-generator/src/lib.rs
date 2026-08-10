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
//!   GET  /warmup      - warm models before the first prompt. With ?model=:
//!                       that one (a tiny 1-step generation). BARE - the
//!                       manager's boot warmup, the playground's page load -
//!                       it is a LADDER: every ATTACHED catalog model tried
//!                       smallest-volume-first, one at a time; a model that
//!                       does not fit the share is reported unfit and
//!                       skipped, not fatal, so one published app serves
//!                       whatever the deployment can hold (the playground
//!                       disables the rest in its picker).
//!                       (?target=gpu|cpu, ?size= for single-model mode.)
//!   GET  /image       - ?prompt=...&steps=&seed=&w=&h=&target=&model= ->
//!                       image/png.
//!   POST /generate    - {prompt, model?, steps?, seed?, width?, height?,
//!                       target?, ancestral?, cfg?, upscale?, upscaler?} ->
//!                       SSE: {status} lines while loading/generating, then
//!                       {done, image: <b64 png>, model, seed, timings}.
//!                       upscale: true runs the result through an upscaler
//!                       volume first (best effort: a failed upscale reports
//!                       next to the BASE image instead of discarding the
//!                       generation). The playground's endpoint.
//!   POST /upscale     - raw PNG body -> ESRGAN-upscaled PNG (the stock
//!                       catalog: Real-ESRGAN x4plus, 4x). ?upscaler= picks
//!                       a catalog entry; ?factor= asks for a divisor of
//!                       the native scale (2 on the 4x model = the native
//!                       output box-averaged down: supersampled, so quality
//!                       meets or beats a native 2x model; 1 = a same-size
//!                       cleanup pass). Output geometry rides x-width /
//!                       x-height / x-upscale-factor response headers.
//!   POST /v1/images/upscale - the /v1-shaped (api_key-gated) twin of
//!                       /upscale: {image: <base64 png>, upscaler?, factor?}
//!                       -> {created, data: [{b64_json, width, height,
//!                       factor}]}. Nonstandard route; OpenAI's schema has
//!                       no upscale concept.
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

pub mod b64;
pub mod config;
pub mod imageops;

#[cfg(target_arch = "wasm32")]
mod pipeline;

#[cfg(target_arch = "wasm32")]
mod app;

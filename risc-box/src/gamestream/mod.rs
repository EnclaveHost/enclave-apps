//! The GameStream host, in-guest.
//!
//! PLATFORM.md §4: with hardware encode reachable through wasi-nn, the whole
//! host moves inside the CVM — pairing, the HTTPS control surface, RTSP, RTP
//! with FEC and the encrypted control channel — and the framebuffer stops
//! making an HTTP round trip out of the enclave, because the emulator and the
//! streaming host become the same module. Pixels, keystrokes and session keys
//! stay inside the boundary and under attestation.
//!
//! Ported from `gs-bridge/`, which ran these same protocols natively. Two
//! things had to change and nothing else: openssl became the RustCrypto stack
//! (openssl is C and will not build for wasm32-wasip2), and the framebuffer
//! source became a direct read instead of `GET /fb.rgb`.

pub mod audio;
pub mod control;
pub mod crypto;
pub mod enet;
pub mod enet_sys;
pub mod fec;
pub mod host;
pub mod httpx;
pub mod pair;
pub mod ping;
pub mod rtsp;
pub mod session;
pub mod video;
pub mod x509gen;

//! The ordinary wasip2 component's entry point.
//!
//! Everything lives in the library (src/lib.rs) so the same code can also be
//! linked as a staticlib behind a C `main` for the shared-everything-threads
//! build — see set/ and the `[lib]` note in Cargo.toml.

fn main() {
    risc_box::run();
}

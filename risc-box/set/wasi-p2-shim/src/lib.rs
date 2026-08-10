//! One function, so a shared-everything-threads build has no preview1 in it.
//!
//! `getrandom` 0.2 is the only thing in this app's graph that speaks WASI
//! preview1 — it arrives under the TLS stack (rustls-rustcrypto -> p256 ->
//! rand_core -> getrandom) and has no p2 backend at any 0.2 version. Its whole
//! use of the `wasi` crate is `random_get`.
//!
//! That one import is enough to sink a SET build. A core module importing
//! `wasi_snapshot_preview1` makes wasm-component-ld attach the preview1
//! ADAPTER, the adapter imports `env::memory` as NON-shared, and a SET guest's
//! memory is shared — so the component will not instantiate:
//!
//!     mismatch in the shared flag for memories
//!
//! and the platform's engine refuses memory-needing adapters in a SET
//! component deliberately, so this is by design rather than a bug to route
//! around. The fix is to have no preview1 import at all.
//!
//! `getentropy` is the way out: wasi-libc's p2-flavored build implements it
//! over `wasi:random`, with no preview1 anywhere (checked — the SET sysroot's
//! libc.a contains zero `wasi_snapshot_preview1` strings). So randomness comes
//! from libc, the adapter never gets attached, and the crypto stack is none
//! the wiser.
//!
//! The real fix upstream is getrandom 0.3, which has a native wasip2 backend;
//! that needs the whole rand_core/p256/ecdsa stack moved up a major, which is
//! a separate change with its own risk. This shim is deliberately the smaller
//! one, and it is confined to the SET build.

/// wasi 0.11's `Errno`, reduced to the one method `getrandom` calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(u16);

impl Errno {
    pub const fn raw(self) -> u16 {
        self.0
    }
}

/// POSIX EIO — what a failed `getentropy` is reported as. Any non-zero value
/// works here (getrandom only needs it to be non-zero), but a real errno keeps
/// the eventual error message honest.
const EIO: u16 = 29;

extern "C" {
    fn getentropy(buf: *mut u8, len: usize) -> i32;
}

/// # Safety
/// `buf` must be valid for writes of `len` bytes.
pub unsafe fn random_get(buf: *mut u8, len: usize) -> Result<(), Errno> {
    // getentropy is specified to fail above 256 bytes, so fill in chunks.
    let mut written = 0usize;
    while written < len {
        let take = core::cmp::min(256, len - written);
        if getentropy(buf.add(written), take) != 0 {
            return Err(Errno(EIO));
        }
        written += take;
    }
    Ok(())
}

/* The SET build's entry point.
 *
 * The platform's shared-everything-threads toolchain links a C `main` against
 * the SET sysroot (see enclave wasm/Dockerfile.wasipsetc-build); Rust is a
 * staticlib alongside it. This file exists only to hand control straight to
 * the app — everything real is in Rust, and `risc_box_main` is the same
 * `run()` the ordinary wasip2 binary calls.
 *
 * Keeping the C surface to three lines is the point: the two builds must not
 * be able to drift apart. */
#include <stdlib.h>

int risc_box_main(void);

int main(void) {
  /* Populate libc's `environ` before Rust looks at it.
   *
   * In the ordinary build Rust owns `_start` and std's runtime initialization
   * runs on the way in. Here the entry point is C, so it does not — and
   * `std::env::var` on wasi reads libc's `environ` directly rather than going
   * through `getenv`, so it walks a pointer wasi-libc has not filled in yet
   * and traps:
   *
   *     wasm trap: out of bounds memory access
   *       std::sys::env::wasi::getenv / std::env::var::inner
   *
   * wasi-libc populates `environ` lazily on first use from inside `getenv`,
   * so one call here is enough to make it real for everyone afterwards. The
   * app reads ENCLAVE_CONFIG on its very first line, so this is not a corner
   * case — without it the SET build cannot start at all. */
  (void)getenv("PATH");

  return risc_box_main();
}

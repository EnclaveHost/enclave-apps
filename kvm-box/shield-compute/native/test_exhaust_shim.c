/* Test-only failure propagation fixture. Never package in a desktop image.
 * Returns the real pad-exhaustion error after one successful backend call.
 * Actual finite-counter boundary tests are in test_maskbank.c.
 */
#define _GNU_SOURCE
#include "shielded-tee.h"
#include <dlfcn.h>
#include <stdlib.h>
#include <stdio.h>

int sh_link_gemm(sh_link *link, const int *nodes, size_t count,
                 const int64_t *input, int32_t rows, int64_t **outputs) {
  static unsigned calls;
  if (++calls >= 2) {
    fputs("[test-only] pad exhaustion injected\n", stderr);
    return SH_ERR_EXHAUST;
  }
  typedef int (*real_fn)(sh_link *, const int *, size_t, const int64_t *, int32_t, int64_t **);
  real_fn real = (real_fn)dlsym(RTLD_NEXT, "sh_link_gemm");
  if (!real && getenv("ENCLAVE_SHIELD_BACKEND")) {
    void *handle = dlopen(getenv("ENCLAVE_SHIELD_BACKEND"), RTLD_NOW | RTLD_NOLOAD);
    if (handle) real = (real_fn)dlsym(handle, "sh_link_gemm");
  }
  if (!real) abort();
  return real(link, nodes, count, input, rows, outputs);
}

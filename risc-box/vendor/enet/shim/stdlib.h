/* Minimal stdlib for the vendored ENet: it allocates through ENet's own
   callbacks, so this is malloc/free and nothing more. Symbols resolve at the
   final link against the wasi-libc the Rust side already carries -- the same
   arrangement vendor/minih264/shim uses. */
#ifndef _RBX_SHIM_STDLIB_H
#define _RBX_SHIM_STDLIB_H
#include <stddef.h>
void * malloc (size_t);
void   free (void *);
void   abort (void);
#endif

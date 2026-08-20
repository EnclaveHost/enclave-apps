/* Freestanding libc surface for the minih264 build (see wrapper.c). The wasm
 * targets this app builds for have no C sysroot on the machine that runs
 * cargo; the encoder only needs these five declarations, and the symbols
 * resolve at link time against the wasi-libc the Rust side already links. */
#ifndef RBX_SHIM_STRING_H
#define RBX_SHIM_STRING_H
#include <stddef.h>
void *memcpy(void *dest, const void *src, size_t n);
void *memmove(void *dest, const void *src, size_t n);
void *memset(void *s, int c, size_t n);
int memcmp(const void *s1, const void *s2, size_t n);
size_t strlen(const char *s);
#endif

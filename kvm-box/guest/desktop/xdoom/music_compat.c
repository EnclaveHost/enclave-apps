// risc-box: the handful of symbols the vendored music sources expect but a
// NOSDL fbDOOM does not provide.
//
// Two lineages meet here. midifile.c reaches for i_swap.h's byte swappers,
// which fbDOOM maps onto SDL's SDL_SwapBE* — present only in the SDL build,
// so a NOSDL link comes up short. And modern chocolate-doom's i_oplmusic.c
// calls M_remove(), a wrapper fbDOOM's m_misc.c predates.
//
// Defining them here keeps every vendored file byte-identical to upstream,
// which is what makes re-syncing them later a copy rather than a merge.

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

// Big-endian to host. RISC-V is little-endian, so these always swap; written
// as an explicit byte shuffle rather than a builtin so the file has no
// dependencies at all.
uint16_t SDL_SwapBE16(uint16_t x)
{
    return (uint16_t) (((x & 0x00ffu) << 8) | ((x & 0xff00u) >> 8));
}

uint32_t SDL_SwapBE32(uint32_t x)
{
    return ((x & 0x000000ffu) << 24)
         | ((x & 0x0000ff00u) << 8)
         | ((x & 0x00ff0000u) >> 8)
         | ((x & 0xff000000u) >> 24);
}

// Little-endian to host: identity here, but referenced by the same header.
uint16_t SDL_SwapLE16(uint16_t x) { return x; }
uint32_t SDL_SwapLE32(uint32_t x) { return x; }

// chocolate-doom's M_remove is remove() with wide-character handling on
// Windows. On this machine it is remove().
int M_remove(const char *path)
{
    return remove(path);
}

// chocolate-doom's fopen wrapper (UTF-8 paths on Windows); plain fopen here.
FILE *M_fopen(const char *filename, const char *mode)
{
    return fopen(filename, mode);
}

// realloc that aborts rather than returning NULL, matching I_Realloc's
// contract: callers do not check, so a silent NULL would be a crash further
// away from the cause.
void *I_Realloc(void *ptr, size_t size)
{
    void *p = realloc(ptr, size);
    if (p == NULL && size > 0)
    {
        fprintf(stderr, "I_Realloc: failed on %zu bytes\n", size);
        abort();
    }
    return p;
}

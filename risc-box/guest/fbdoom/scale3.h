#ifndef RBX_SCALE3_H
#define RBX_SCALE3_H

#include <stddef.h>
#include <stdint.h>

/* Little-endian RGBX, an even source width, and 8-byte-aligned output rows.
 * Expand two palette pixels into three 64-bit words on each of three rows.
 * This avoids the generic per-pixel repeat loop and two subsequent row copies.
 * Only the inclusive dirty source-row range is written. */
static inline void rbx_scale3(uint8_t *dst, size_t stride,
                              const uint8_t *src, const uint32_t *pal,
                              int width, int first, int last)
{
    for (int y = first; y <= last; y++) {
        uint64_t *a = (uint64_t *)(dst + (size_t)y * 3 * stride);
        uint64_t *b = (uint64_t *)((uint8_t *)a + stride);
        uint64_t *c = (uint64_t *)((uint8_t *)b + stride);
        const uint8_t *in = src + (size_t)y * width;
        for (int x = 0; x < width; x += 2) {
            uint64_t p = pal[in[x]], q = pal[in[x + 1]];
            uint64_t pp = p | (p << 32);
            uint64_t pq = p | (q << 32);
            uint64_t qq = q | (q << 32);
            a[0] = b[0] = c[0] = pp;
            a[1] = b[1] = c[1] = pq;
            a[2] = b[2] = c[2] = qq;
            a += 3; b += 3; c += 3;
        }
    }
}

#endif

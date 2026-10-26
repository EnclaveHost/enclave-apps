#ifndef MUSIC_COMPAT_H
#define MUSIC_COMPAT_H
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
uint16_t SDL_SwapBE16(uint16_t);
uint32_t SDL_SwapBE32(uint32_t);
uint16_t SDL_SwapLE16(uint16_t);
uint32_t SDL_SwapLE32(uint32_t);
int M_remove(const char *);
FILE *M_fopen(const char *, const char *);
void *I_Realloc(void *, size_t);
#endif

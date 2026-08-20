/* Freestanding stdio for the minih264 build: the header includes <stdio.h>
 * but the encoder never calls a stdio function, so an empty header satisfies
 * the include without dragging in a C sysroot. */
#ifndef RBX_SHIM_STDIO_H
#define RBX_SHIM_STDIO_H
#endif

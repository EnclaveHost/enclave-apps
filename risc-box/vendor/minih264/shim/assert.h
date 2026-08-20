/* Freestanding assert for the minih264 build: compiled with NDEBUG always —
 * a failed assert in a video encoder is not worth trapping the whole app
 * (and the emulator inside it) over. */
#ifndef RBX_SHIM_ASSERT_H
#define RBX_SHIM_ASSERT_H
#define assert(x) ((void)0)
#endif

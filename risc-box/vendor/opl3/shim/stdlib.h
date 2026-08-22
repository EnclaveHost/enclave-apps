/* risc-box: freestanding stand-in for <stdlib.h>.
 *
 * opl3.c includes it out of habit and uses nothing from it — no malloc, no
 * abort, no exit (checked). The wasm build has no C sysroot at cargo time
 * (see build.rs), so the include has to resolve to something; this is that
 * something. Deliberately empty rather than declaring stubs: a symbol
 * declared here would link, and silently, against nothing.
 */
#ifndef RBX_OPL3_SHIM_STDLIB_H
#define RBX_OPL3_SHIM_STDLIB_H
#endif

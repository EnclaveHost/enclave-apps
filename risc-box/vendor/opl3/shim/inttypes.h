/* risc-box: freestanding stand-in for <inttypes.h>.
 *
 * opl3.h reaches for it, but the code only uses the fixed-width types from
 * <stdint.h>, which clang provides freestanding — none of the PRI* format
 * macros are referenced. Same reasoning as shim/stdlib.h next door.
 */
#ifndef RBX_OPL3_SHIM_INTTYPES_H
#define RBX_OPL3_SHIM_INTTYPES_H
#include <stdint.h>
#endif

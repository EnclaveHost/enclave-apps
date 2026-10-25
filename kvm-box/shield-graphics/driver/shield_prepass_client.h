/* Experimental exact-integer prepass adapter; not general shader lowering. */
#ifndef SHIELD_PREPASS_CLIENT_H
#define SHIELD_PREPASS_CLIENT_H
#include <stdint.h>
enum shield_prepass_result {
    SHIELD_PREPASS_FAILED = -1,
    SHIELD_PREPASS_UNAVAILABLE = 0, /* No connection/request; use original CPU shader. */
    SHIELD_PREPASS_LOCAL = 1,       /* Trusted service explicitly reports CPU execution. */
    SHIELD_PREPASS_MASKED = 2       /* Service reports verified masked exchanges. */
};
/* Input is an owned 64x4 snapshot for the exact qualification shader. Output
 * is unchanged unless the entire response is validated. The caller must prove
 * shader equivalence, descriptor extent, synchronization and non-aliasing. */
enum shield_prepass_result shield_prepass_execute(const uint32_t input[256], int32_t output[256]);
#endif

/* Exact bounded-integer affine adapter; not arbitrary shader lowering. */
#ifndef SHIELD_PREPASS_CLIENT_H
#define SHIELD_PREPASS_CLIENT_H
#include <stdint.h>
#include "shield_affine64.h"
enum shield_prepass_result {
    SHIELD_PREPASS_FAILED = -1,
    SHIELD_PREPASS_UNAVAILABLE = 0, /* No connection/request; use original CPU shader. */
    SHIELD_PREPASS_LOCAL = 1,       /* Trusted service explicitly reports CPU execution. */
    SHIELD_PREPASS_MASKED = 2       /* Service reports verified masked exchanges. */
};
/* Input is an owned 64x4 snapshot. The legacy entry uses the fixed qualification transform. Output
 * is unchanged unless the entire response is validated. The caller must prove
 * shader equivalence, descriptor extent, synchronization and non-aliasing. */
enum shield_prepass_result shield_prepass_execute(const uint32_t input[256], int32_t output[256]);
/* Caller must supply an immutable transform from an accepted conditional proof
 * and revalidate the full dispatch/layout contract. Bounds validation here is
 * defense in depth, not semantic proof or public-matrix authorization. */
enum shield_prepass_result shield_prepass_execute_transform(const struct shield_affine64 *transform, const uint32_t input[256], int32_t output[256]);
/* Contiguous count x 4 owned input snapshot, 1..4096 rows. The caller must
 * possess the matching multi-workgroup proof before using count > 64.
 * One graph request uses independently verified tiles on a single channel. */
enum shield_prepass_result shield_prepass_execute_rows(const struct shield_affine64 *transform, const uint32_t *input, unsigned count, int32_t *output);
#endif

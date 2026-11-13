/* Conditional structural proof for the masked prepass driver. */
#ifndef SHIELD_PREPASS_NIR_H
#define SHIELD_PREPASS_NIR_H
#include "nir.h"
#include "shield_affine64.h"
/* max_workgroups is an in-memory conditional bound: 1 or 64. The latter
 * proves global row = 64*workgroup.x + local.x, including input/output addresses
 * and row terms. Runtime must still enforce block64x1x1, gridY/Z=1, base0,
 * gridX<=max_workgroups, and exact gridX*1024-byte non-overlapping ranges.
 * A zero bound is never eligible. It cannot be recovered from serialized data. */
/* Conditional proof, not authorization. FIXED64_V1 requires EVERY dispatch to
 * revalidate: ordinary direct dispatch, gridX 1..max_workgroups, gridY/Z=1, base0, block64x1x1; no active
 * queries/generated commands; descriptor-buffer mode absent; exactly two
 * non-dynamic non-mutable STORAGE_BUFFER bindings at set0 bindings0/1, arrays
 * of length1, each range gridX*1024 bytes, aligned/non-null/non-overlapping/non-wrapping.
 * Pipeline metadata/entrypoint/specialization must bind this exact NIR.
 * Service approval and verified output publication remain independent.
 * Never serialize this result or accept it from shader-object/cache binaries.
 * NIR's descriptor type does NOT prove the pipeline/bound descriptor layout.
 * Normalization follows upstream compiler refinement of valid shader behavior;
 * this does not promise to preserve undefined behavior or nontermination. */
/* ZINK64_V1 shares all dispatch/buffer/approval requirements, but uses set3,
 * binding0, a two-element ordinary SSBO descriptor array: input[0], output[1].
 * Its shader writes four scalar words per row. Descriptor-buffer and dynamic
 * layouts still must be rejected, regardless of the shader's descriptor type. */
/* V2 contracts share their corresponding V1 dispatch/layout/normalization
 * invariants. The copied transform is in-memory only, never loaded from shader
 * objects/cache binaries. Its matrix requires independent exact-digest service
 * approval; uint32 bias and row terms are applied locally after verification. */
enum shield_nir_contract { SHIELD_NIR_NONE, SHIELD_NIR_FIXED64_V1, SHIELD_NIR_ZINK64_V1, SHIELD_NIR_AFFINE64_V2, SHIELD_NIR_ZINK_AFFINE64_V2 };
enum shield_nir_reason {
 SHIELD_NIR_OK, SHIELD_NIR_METADATA, SHIELD_NIR_LIMIT, SHIELD_NIR_CONTROL_FLOW,
 SHIELD_NIR_DEREF_MODE, SHIELD_NIR_EXPRESSION, SHIELD_NIR_INPUT_ACCESS,
 SHIELD_NIR_VOLATILE_ACCESS, SHIELD_NIR_STORE_SHAPE, SHIELD_NIR_OUTPUT_LAYOUT,
 SHIELD_NIR_OUTPUT_EQUATION, SHIELD_NIR_INTRINSIC, SHIELD_NIR_INSTRUCTION
};
struct shield_nir_proof { enum shield_nir_contract contract; enum shield_nir_reason reason; struct shield_affine64 transform; unsigned max_workgroups; };
struct shield_nir_proof shield_prepass_prove_nir(nir_shader *nir);
const char *shield_nir_reason_name(enum shield_nir_reason reason);
#endif

/* Conditional exact integer lowering. Matrix approval is a separate service policy. */
#ifndef SHIELD_AFFINE64_H
#define SHIELD_AFFINE64_H
#include <stdint.h>
#include <stdbool.h>
#include <string.h>

/* x[r,k] = (input[r*4+k] & 255) - 128, global row r in [0,64*gridX-1], with gridX within the proved bound.
 * y[r,c] = sum_k(x[r,k]*matrix[k][c]) + bias[c] + r*row[c] in Z/(2^32).
 * Only the bounded dot product is sent to the existing Shield service.
 * Bias/row coefficients stay in the trusted caller and are applied afterward.
 * The service must approve the exact padded 32x4 matrix before GPU execution.
 * Neither this structure nor a successful proof grants that approval. */
struct shield_affine64 {
   int32_t matrix[4][4];
   uint32_t bias[4];
   uint32_t row[4];
};
static inline bool shield_affine64_valid(const struct shield_affine64 *t) {
   if (!t) return false;
   for (unsigned k=0;k<4;k++)
      for (unsigned c=0;c<4;c++)
         if (t->matrix[k][c] < -119 || t->matrix[k][c] > 119) return false;
   return true;
}
static inline struct shield_affine64 shield_affine64_fixed(void) {
   return (struct shield_affine64){.matrix={{1,-1,0,0},{1,1,0,0},{0,0,1,0},{0,0,0,1}}};
}
static inline bool shield_affine64_is_fixed(const struct shield_affine64 *t) {
   const struct shield_affine64 fixed=shield_affine64_fixed();
   return !memcmp(t->matrix,fixed.matrix,sizeof fixed.matrix) &&
      !memcmp(t->bias,fixed.bias,sizeof fixed.bias) && !memcmp(t->row,fixed.row,sizeof fixed.row);
}
#endif

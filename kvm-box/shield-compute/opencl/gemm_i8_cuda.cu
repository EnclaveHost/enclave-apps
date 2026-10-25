#include <cuda_runtime.h>

extern "C" __global__ void gemm(const signed char *a, const signed char *b,
                                long long *c, unsigned m, unsigned n, unsigned k) {
  size_t row = size_t(blockIdx.x) * blockDim.x + threadIdx.x;
  size_t col = size_t(blockIdx.y) * blockDim.y + threadIdx.y;
  if (row < m && col < n) {
    long long sum = 0;
    for (unsigned z = 0; z < k; ++z)
      sum += (long long)a[row * k + z] * (long long)b[z * n + col];
    c[row * n + col] = sum;
  }
}

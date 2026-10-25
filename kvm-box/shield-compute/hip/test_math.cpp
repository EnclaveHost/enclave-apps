#include <hip/hip_runtime.h>
#include <cassert>
#include <cmath>
#include <cstdio>
#include <cstdlib>

static void check(hipError_t error) {
  if (error != hipSuccess) {
    std::fprintf(stderr, "%s\n", hipGetErrorString(error));
    std::abort();
  }
}
__global__ void functions(const double *input, double *output) {
  unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
  if (i < 7) {
    double x = input[i];
    output[4 * i] = sinpi(x);
    output[4 * i + 1] = cospi(x);
    output[4 * i + 2] = sinpif(float(x));
    output[4 * i + 3] = cospif(float(x));
  }
}
int main() {
  double input[] = {-2.5, -1, -0.5, 0, 0.5, 1, 2.5}, output[28];
  double *in, *out;
  check(hipMalloc((void **)&in, sizeof(input)));
  check(hipMalloc((void **)&out, sizeof(output)));
  check(hipMemcpy(in, input, sizeof(input), hipMemcpyHostToDevice));
  functions<<<1, 8>>>(in, out);
  check(hipGetLastError());
  check(hipMemcpy(output, out, sizeof(output), hipMemcpyDeviceToHost));
  for (unsigned i = 0; i < 7; ++i) {
    double s = std::sin(input[i] * std::acos(-1.0));
    double c = std::cos(input[i] * std::acos(-1.0));
    std::printf("x=%g host sinpif=%g cospif=%g device sinpi=%g cospi=%g sinpif=%g cospif=%g\n",
                input[i], sinpif(float(input[i])), cospif(float(input[i])),
                output[4*i], output[4*i+1], output[4*i+2], output[4*i+3]);
    std::fflush(stdout);
    assert(std::abs(output[4 * i] - s) < 1e-12);
    assert(std::abs(output[4 * i + 1] - c) < 1e-12);
    assert(std::abs(output[4 * i + 2] - s) < 2e-6);
    assert(std::abs(output[4 * i + 3] - c) < 2e-6);
    assert(std::abs(sinpif(float(input[i])) - s) < 2e-6);
    assert(std::abs(cospif(float(input[i])) - c) < 2e-6);
  }
  check(hipFree(in));
  check(hipFree(out));
  std::puts("HIP sinpi/cospi host and device: float/double reference checks passed");
}

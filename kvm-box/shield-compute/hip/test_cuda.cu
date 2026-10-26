#include <cuda_runtime.h>
#include <cassert>
#include <cstdio>
#include <cstdlib>
#include <vector>
#include <string>
#include <chrono>
#include <cmath>
#include <thread>

static void check(cudaError_t error) {
  if (error != cudaSuccess) {
    std::fprintf(stderr, "CUDA compatibility error: %s\n", cudaGetErrorString(error));
    std::abort();
  }
}

extern "C" __global__ void gemm(const signed char *a, const signed char *b,
                                long long *c, unsigned m, unsigned n, unsigned k) {
  size_t row = size_t(blockIdx.x) * blockDim.x + threadIdx.x;
  size_t col = size_t(blockIdx.y) * blockDim.y + threadIdx.y;
  if (row < m && col < n) {
    long long sum =
#ifdef SHIELD_CUDA_CHANGED
        1;
#else
        0;
#endif
    for (unsigned z = 0; z < k; ++z)
      sum += (long long)a[row * k + z] * (long long)b[z * n + col];
    c[row * n + col] = sum;
  }
}

int main(int argc, char **argv) {
  const bool recover = argc > 1 && std::string(argv[1]) == "cuda-recovery";
  const bool fail = recover || (argc > 1 && std::string(argv[1]) == "failure");
  cudaDeviceProp device{};
  check(cudaGetDeviceProperties(&device, 0));
  std::printf("Logical device: %s\n", device.name);
  constexpr unsigned m = 4, n = 16;
  // The CPU-only suite deliberately uses a shape outside the masked contract.
  const unsigned k = argc > 1 && std::string(argv[1]) == "cpu-compat" ? 31 : 32;
  std::vector<signed char> a(m * k), b(k * n);
  std::vector<long long> c(m * n, 123456789);
  for (size_t i = 0; i < a.size(); ++i) a[i] = int(i % 17) - 8;
  for (size_t i = 0; i < b.size(); ++i) b[i] = int(i % 5) - 2;
  signed char *da, *db;
  long long *dc;
  check(cudaMalloc((void **)&da, a.size()));
  check(cudaMalloc((void **)&db, b.size()));
  check(cudaMalloc((void **)&dc, c.size() * sizeof(long long)));
  cudaStream_t stream;
  check(cudaStreamCreateWithFlags(&stream, cudaStreamNonBlocking));
  cudaEvent_t profile = nullptr, profile_end = nullptr;
  if (argc > 1 && std::string(argv[1]) == "profiling") {
    check(cudaEventCreate(&profile));
    check(cudaEventCreate(&profile_end));
    check(cudaEventRecord(profile, stream));
  }
  check(cudaMemcpyAsync(da, a.data(), a.size(), cudaMemcpyHostToDevice, stream));
  check(cudaMemcpyAsync(db, b.data(), b.size(), cudaMemcpyHostToDevice, stream));
  check(cudaMemcpyAsync(dc, c.data(), c.size() * sizeof(long long), cudaMemcpyHostToDevice, stream));
  gemm<<<dim3(2, 4), dim3(4, 8), 0, stream>>>(da, db, dc, m, n, k);
  check(cudaGetLastError());
  auto copied = cudaMemcpyAsync(c.data(), dc, c.size() * sizeof(long long), cudaMemcpyDeviceToHost, stream);
  if (profile_end) check(cudaEventRecord(profile_end, stream));
  auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(30);
  cudaError_t queried;
  while ((queried = cudaStreamQuery(stream)) == cudaErrorNotReady &&
         std::chrono::steady_clock::now() < deadline)
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  auto synchronized = cudaStreamSynchronize(stream);
  if (fail) {
    assert(queried != cudaSuccess && queried != cudaErrorNotReady);
    assert(copied != cudaSuccess || synchronized != cudaSuccess);
    cudaStream_t inspect;
    check(cudaStreamCreateWithFlags(&inspect, cudaStreamNonBlocking));
    check(cudaMemcpyAsync(c.data(), dc, c.size() * sizeof(long long), cudaMemcpyDeviceToHost, inspect));
    check(cudaStreamSynchronize(inspect));
    for (auto value : c) assert(value == 123456789);
    std::puts("CUDA failed offload: visible runtime error and untouched output passed");
    // An unrelated stream must remain independently destroyable. Destroying
    // the failed stream reports its error but must release its queue resources.
    check(cudaStreamDestroy(inspect));
    assert(cudaStreamDestroy(stream) == cudaErrorLaunchFailure);
    (void)cudaGetLastError(); // The failed command has already been observed.
    check(cudaStreamCreateWithFlags(&stream, cudaStreamNonBlocking));
    if (!recover) {
      check(cudaMemsetAsync(dc, 0, c.size() * sizeof(long long), stream));
      check(cudaMemcpyAsync(c.data(), dc, c.size() * sizeof(long long), cudaMemcpyDeviceToHost, stream));
      check(cudaStreamSynchronize(stream));
      for (auto value : c) assert(value == 0);
      check(cudaStreamDestroy(stream));
      check(cudaFree(da)); check(cudaFree(db)); check(cudaFree(dc));
      std::puts("CUDA stream disposal and fresh-stream CPU recovery passed");
      return 0;
    }
    gemm<<<dim3(2, 4), dim3(4, 8), 0, stream>>>(da, db, dc, m, n, k);
    check(cudaGetLastError());
    copied = cudaMemcpyAsync(c.data(), dc, c.size() * sizeof(long long), cudaMemcpyDeviceToHost, stream);
    synchronized = cudaStreamSynchronize(stream);
    queried = cudaStreamQuery(stream);
    std::puts("CUDA fresh-stream masked recovery completed");
  }
  check(queried);
  check(copied);
  check(synchronized);
  for (unsigned row = 0; row < m; ++row)
    for (unsigned col = 0; col < n; ++col) {
      long long expected = 0;
      for (unsigned z = 0; z < k; ++z)
        expected += (long long)a[row * k + z] * b[z * n + col];
#ifdef SHIELD_CUDA_CHANGED
      ++expected;
#endif
      assert(c[row * n + col] == expected);
    }
  check(cudaFree(da));
  check(cudaFree(db));
  check(cudaFree(dc));
  if (profile) {
    check(cudaEventSynchronize(profile_end));
    float milliseconds = -1;
    check(cudaEventElapsedTime(&milliseconds, profile, profile_end));
    assert(std::isfinite(milliseconds) && milliseconds >= 0);
    if (const char *minimum = std::getenv("SHIELD_TEST_MIN_DURATION_NS"))
      assert(milliseconds * 1e6 >= std::strtoull(minimum, nullptr, 10));
    std::printf("CUDA logical event interval: %.3f ms (includes protected service work)\n", milliseconds);
    check(cudaEventDestroy(profile_end));
    check(cudaEventDestroy(profile));
  }
  check(cudaStreamDestroy(stream));
  std::puts("CUDA source GEMM: exact result through standard allocation, stream and launch APIs");
  // This message intentionally makes no claim about which execution route ran.
  // The service's trusted counters must establish any masked GPU offload.
}

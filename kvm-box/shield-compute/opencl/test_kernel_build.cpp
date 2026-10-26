// Reproduce application compiler failures through ordinary OpenCL APIs.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <cstdio>
#include <cstdlib>
#include <csignal>
#include <execinfo.h>
#include <unistd.h>
#include <fstream>
#include <iterator>
#include <string>
#include <vector>

static void check(cl_int e, const char *phase) {
  if (e) { std::fprintf(stderr, "%s: %d\n", phase, e); std::exit(1); }
  std::printf("PASS: %s\n", phase); std::fflush(stdout);
}

int main(int argc, char **argv) {
  if (argc != 3) { std::fprintf(stderr, "usage: test-kernel-build SOURCE OPTIONS\n"); return 2; }
  std::ifstream stream(argv[1]);
  if (!stream) return 2;
  std::string source((std::istreambuf_iterator<char>(stream)), {});
  const char *text = source.c_str();
  size_t length = source.size();
  cl_platform_id platform;
  cl_device_id device;
  check(clGetPlatformIDs(1, &platform, nullptr), "platform");
  check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr), "device");
  cl_int error;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &error);
  check(error, "context");
  auto program = clCreateProgramWithSource(context, 1, &text, &length, &error);
  check(error, "source program");
  error = clBuildProgram(program, 1, &device, argv[2], nullptr, nullptr);
  if (error) {
    size_t size = 0;
    clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &size);
    std::vector<char> log(size + 1, 0);
    clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, size, log.data(), nullptr);
    std::fprintf(stderr, "%s\n", log.data());
  }
  check(error, "build program");
  auto dump = [](int signal) {
    void *frames[48];
    backtrace_symbols_fd(frames, backtrace(frames, 48), STDERR_FILENO);
    _exit(128 + signal);
  };
  std::signal(SIGABRT, dump);
  std::signal(SIGSEGV, dump);
  size_t size = 0;
  check(clGetProgramInfo(program, CL_PROGRAM_BINARY_SIZES, sizeof(size), &size, nullptr), "binary size");
  if (size == 0 || size > 64 * 1024 * 1024) return 1;
  std::vector<unsigned char> binary(size);
  unsigned char *data = binary.data();
  check(clGetProgramInfo(program, CL_PROGRAM_BINARIES, sizeof(data), &data, nullptr), "binary serialization");
  cl_uint count = 0;
  check(clCreateKernelsInProgram(program, 0, nullptr, &count), "kernel count");
  if (count == 0 || count > 1024) return 1;
  std::vector<cl_kernel> kernels(count);
  check(clCreateKernelsInProgram(program, count, kernels.data(), nullptr), "create kernels");
  for (auto kernel : kernels) clReleaseKernel(kernel);
  clReleaseProgram(program); clReleaseContext(context);
  std::printf("PASS: %u kernels, %zu binary bytes; compilation only, no execution\n", count, size);
}

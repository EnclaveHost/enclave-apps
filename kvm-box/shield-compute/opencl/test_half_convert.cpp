// Exercise float-to-half rounding at every binary16 boundary and random float bits.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <CL/cl_half.h>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <string>
#include <vector>

static void check(cl_int e, const char *operation) {
  if (e) { std::fprintf(stderr, "%s: %d\n", operation, e); std::exit(1); }
}

int main() {
  std::vector<float> input;
  auto signs = [&](float x) { input.push_back(x); input.push_back(-x); };
  // Every positive finite half, and floats just below/at/above each midpoint.
  // Include the overflow midpoint separately, because the next half is infinity.
  for (unsigned bits = 0; bits < 0x7c00; ++bits) {
    float lo = cl_half_to_float(static_cast<cl_half>(bits));
    signs(lo);
    float hi = bits == 0x7bff ? 65536.0f : cl_half_to_float(static_cast<cl_half>(bits + 1));
    float midpoint = (lo + hi) / 2;
    signs(std::nextafter(midpoint, -INFINITY));
    signs(midpoint);
    signs(std::nextafter(midpoint, INFINITY));
  }
  for (float x : {std::numeric_limits<float>::denorm_min(),
                  std::numeric_limits<float>::min(),
                  std::numeric_limits<float>::max(), INFINITY}) signs(x);
  uint32_t random = 0x9e3779b9;
  for (unsigned i = 0; i < 65536; ++i) {
    random ^= random << 13; random ^= random >> 17; random ^= random << 5;
    float x; std::memcpy(&x, &random, sizeof(x)); input.push_back(x);
  }
  // Every NaN half payload promoted to a float (NaN payload preservation is not required).
  for (unsigned i = 0x7c01; i <= 0x7fff; ++i) signs(cl_half_to_float(i));

  cl_platform_id platform;
  cl_device_id device;
  check(clGetPlatformIDs(1, &platform, nullptr), "platform");
  check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr), "device");
  cl_int e;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &e);
  check(e, "context");
  auto queue = clCreateCommandQueueWithProperties(context, device, nullptr, &e);
  check(e, "queue");
  auto a = clCreateBuffer(context, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR,
                         input.size() * sizeof(float), input.data(), &e);
  check(e, "input");
  std::vector<cl_half> output(input.size()), scalar;
  auto b = clCreateBuffer(context, CL_MEM_WRITE_ONLY, output.size() * sizeof(cl_half), nullptr, &e);
  check(e, "output");
  struct Rounding { const char *name; cl_half_rounding_mode mode; };
  size_t evaluated = 0;
  for (auto rounding : {Rounding{"rte", CL_HALF_RTE}, Rounding{"rtz", CL_HALF_RTZ},
                        Rounding{"rtp", CL_HALF_RTP}, Rounding{"rtn", CL_HALF_RTN}}) {
    for (unsigned width : {1, 2, 3, 4, 8, 16}) {
      std::string suffix = width == 1 ? "" : std::to_string(width);
      std::string type = "float" + suffix;
      std::string source = "#pragma OPENCL EXTENSION cl_khr_fp16 : enable\n"
          "kernel void probe(global const float *a, global ushort *b) { uint base=get_global_id(0)*" +
          std::to_string(width) + "; " + type + " x; ";
      if (width == 1) source += "x=a[base]; half y=convert_half_" + std::string(rounding.name) + "(x); b[base]=as_ushort(y);";
      else source += "for(uint j=0;j<" + suffix + ";++j) x[j]=base+j<" + std::to_string(input.size()) +
          "?a[base+j]:0.0f; half" + suffix + " y=convert_half" + suffix + "_" + rounding.name +
          "(x); for(uint j=0;j<" + suffix + ";++j) if(base+j<" + std::to_string(input.size()) +
          ") b[base+j]=as_ushort(y[j]);";
      source += "}\n";
      const char *text = source.c_str();
      auto program = clCreateProgramWithSource(context, 1, &text, nullptr, &e);
      check(e, "program");
      e = clBuildProgram(program, 1, &device, "-cl-std=CL3.0", nullptr, nullptr);
      if (e) {
        size_t size;
        clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &size);
        std::vector<char> log(size);
        clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, size, log.data(), nullptr);
        std::fprintf(stderr, "%s\n", log.data());
      }
      check(e, "build");
      auto kernel = clCreateKernel(program, "probe", &e);
      check(e, "kernel");
      check(clSetKernelArg(kernel, 0, sizeof(a), &a), "arg a");
      check(clSetKernelArg(kernel, 1, sizeof(b), &b), "arg b");
      const size_t count = (input.size() + width - 1) / width;
      check(clEnqueueNDRangeKernel(queue, kernel, 1, nullptr, &count, nullptr, 0, nullptr, nullptr), "launch");
      check(clEnqueueReadBuffer(queue, b, CL_TRUE, 0, output.size() * sizeof(cl_half), output.data(), 0, nullptr, nullptr), "read");
      unsigned nan_count = 0;
      for (size_t i = 0; i < input.size(); ++i) {
        if (std::isnan(input[i])) {
          if (!std::isnan(cl_half_to_float(output[i]))) { std::fprintf(stderr, "NaN lost\n"); return 1; }
          ++nan_count;
          continue;
        }
        const cl_half expected = cl_half_from_float(input[i], rounding.mode);
        if (output[i] != expected) {
          std::fprintf(stderr, "%s width=%u index=%zu input=%a expected=%04x actual=%04x\n",
                       rounding.name, width, i, input[i], expected, output[i]);
          return 1;
        }
      }
      if (width == 1) scalar = output;
      else if (scalar != output) {
        std::fprintf(stderr, "vector/scalar bit mismatch\n"); return 1;
      }
      evaluated += input.size();
      std::printf("rounding=%s width=%u inputs=%zu NaNs=%u exact=yes\n", rounding.name, width, input.size(), nan_count);
      clReleaseKernel(kernel); clReleaseProgram(program);
    }
  }
  clReleaseMemObject(a); clReleaseMemObject(b);
  clReleaseCommandQueue(queue); clReleaseContext(context);
  std::printf("PASS: %zu float-to-half conversions; four rounding modes and six vector widths\n", evaluated);
}

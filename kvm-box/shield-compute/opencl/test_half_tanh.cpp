// Exhaust every binary16 encoding through scalar and vector OpenCL tanh.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <CL/cl_ext.h>
#include <CL/cl_half.h>
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

static void check(cl_int e, const char *operation) {
  if (e) { std::fprintf(stderr, "%s: %d\n", operation, e); std::exit(1); }
}

int main() {
  cl_platform_id platform;
  cl_device_id device;
  check(clGetPlatformIDs(1, &platform, nullptr), "platform");
  check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr), "device");
  cl_int e;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &e);
  check(e, "context");
  auto queue = clCreateCommandQueueWithProperties(context, device, nullptr, &e);
  check(e, "queue");
  std::vector<cl_half> input(65536), output(65536), scalar;
  for (unsigned i = 0; i < input.size(); ++i) input[i] = static_cast<cl_half>(i);
  auto a = clCreateBuffer(context, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR,
                         input.size() * 2, input.data(), &e);
  check(e, "input");
  auto b = clCreateBuffer(context, CL_MEM_WRITE_ONLY, input.size() * 2, nullptr, &e);
  check(e, "output");
  cl_device_fp_config half_config;
  check(clGetDeviceInfo(device, CL_DEVICE_HALF_FP_CONFIG, sizeof(half_config), &half_config, nullptr), "half config");
  unsigned aggregate_max = 0;
  for (unsigned width : {1, 2, 3, 4, 8, 16}) {
    std::string type = width == 1 ? "half" : "half" + std::to_string(width);
    std::string source = "#pragma OPENCL EXTENSION cl_khr_fp16 : enable\n"
        "kernel void probe(global const ushort *a, global ushort *b) { uint base=get_global_id(0)*" +
        std::to_string(width) + "; " + type + " x; ";
    if (width == 1) source += "x=as_half(a[base]); half y=tanh(x); b[base]=as_ushort(y);";
    else source += "for(uint j=0;j<" + std::to_string(width) +
        ";++j) x[j]=base+j<65536?as_half(a[base+j]):(half)0; " + type +
        " y=tanh(x); for(uint j=0;j<" + std::to_string(width) +
        ";++j) if(base+j<65536) b[base+j]=as_ushort(y[j]);";
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
    const size_t count = (65536 + width - 1) / width;
    check(clEnqueueNDRangeKernel(queue, kernel, 1, nullptr, &count, nullptr, 0, nullptr, nullptr), "launch");
    check(clEnqueueReadBuffer(queue, b, CL_TRUE, 0, output.size() * 2, output.data(), 0, nullptr, nullptr), "read");
    unsigned max_ulp = 0, inexact = 0, nan_count = 0, flush_count = 0;
    for (unsigned i = 0; i < input.size(); ++i) {
      const double x = cl_half_to_float(input[i]);
      const double actual = cl_half_to_float(output[i]);
      if (std::isnan(x)) {
        if (!std::isnan(actual)) { std::fprintf(stderr, "NaN lost at %04x\n", i); return 1; }
        ++nan_count;
        continue;
      }
      const cl_half expected = cl_half_from_double(std::tanh(x), CL_HALF_RTE);
      // A device without half denormals may flush a subnormal result to signed zero.
      if (!(half_config & CL_FP_DENORM) && (expected & 0x7c00) == 0 &&
          (expected & 0x3ff) && output[i] == (expected & 0x8000)) {
        ++flush_count;
        continue;
      }
      if ((output[i] & 0x8000) != (expected & 0x8000) || !std::isfinite(actual)) {
        std::fprintf(stderr, "sign/class mismatch at %04x\n", i); return 1;
      }
      const unsigned ulp = std::abs(int(output[i]) - int(expected));
      max_ulp = std::max(max_ulp, ulp);
      inexact += ulp != 0;
      if (ulp > 1) {
        std::fprintf(stderr, "width=%u input=%04x expected=%04x actual=%04x ulp=%u\n",
                     width, i, expected, output[i], ulp);
        return 1;
      }
    }
    if (width == 1) scalar = output;
    else if (scalar != output) {
      std::fprintf(stderr, "vector/scalar bit mismatch at width %u\n", width); return 1;
    }
    aggregate_max = std::max(aggregate_max, max_ulp);
    std::printf("width=%u encodings=65536 maxUlp=%u nonExact=%u NaNs=%u permittedFlushes=%u\n",
                width, max_ulp, inexact, nan_count, flush_count);
    clReleaseKernel(kernel);
    clReleaseProgram(program);
  }
  clReleaseMemObject(a); clReleaseMemObject(b);
  clReleaseCommandQueue(queue); clReleaseContext(context);
  std::printf("PASS: 393216 half tanh evaluations; maximum %u ULP versus double-libm reference\n", aggregate_max);
}

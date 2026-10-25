#define CL_TARGET_OPENCL_VERSION 300
#define CL_USE_DEPRECATED_OPENCL_1_2_APIS
#include <CL/cl.h>
#include <cassert>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <iterator>
#include <string>
#include <vector>
#ifndef SHIELD_TEST_KERNEL
#define SHIELD_TEST_KERNEL "/src/gemm_i8.cl"
#endif
static void check(cl_int error) {
  if (error) {
    std::fprintf(stderr, "OpenCL error: %d\n", error);
    std::abort();
  }
}
int main(int argc, char **argv) {
  const std::string mode = argc > 1 ? argv[1] : "svm";
  const bool fail = mode == "failure" || mode == "dependency-failure";
  cl_platform_id platform;
  cl_device_id device;
  check(clGetPlatformIDs(1, &platform, nullptr));
  check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr));
  cl_device_svm_capabilities capabilities;
  check(clGetDeviceInfo(device, CL_DEVICE_SVM_CAPABILITIES, sizeof(capabilities), &capabilities, nullptr));
  assert(capabilities & CL_DEVICE_SVM_COARSE_GRAIN_BUFFER);
  cl_int error;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &error);
  check(error);
  auto queue = clCreateCommandQueue(context, device, 0, &error);
  check(error);
  std::ifstream file(SHIELD_TEST_KERNEL);
  assert(file.good());
  std::string source((std::istreambuf_iterator<char>(file)), {});
  const char *text = source.c_str();
  auto program = clCreateProgramWithSource(context, 1, &text, nullptr, &error);
  check(error);
  check(clBuildProgram(program, 1, &device, "-cl-std=CL1.2", nullptr, nullptr));
  auto kernel = clCreateKernel(program, "gemm", &error);
  check(error);
  const cl_uint m = 4, n = 16, k = 32;
  std::vector<cl_char> a(m * k), b(k * n);
  std::vector<cl_long> c(m * n, 123456789);
  for (size_t i = 0; i < a.size(); ++i) a[i] = int(i % 17) - 8;
  for (size_t i = 0; i < b.size(); ++i) b[i] = int(i % 5) - 2;
  const size_t lengths[] = {a.size(), b.size(), c.size() * sizeof(cl_long)};
  void *host[] = {a.data(), b.data(), c.data()};
  void *base[3], *pointers[3];
  cl_mem buffers[3];
  const size_t padding = mode == "svm-subrange" ? 128 : 0;
  for (unsigned i = 0; i < 3; ++i) {
    buffers[i] = clCreateBuffer(context, CL_MEM_READ_WRITE | CL_MEM_COPY_HOST_PTR,
                                lengths[i], host[i], &error);
    check(error);
    check(clSetKernelArg(kernel, i, sizeof(cl_mem), buffers + i));
    base[i] = clSVMAlloc(context, CL_MEM_READ_WRITE, lengths[i] + 2 * padding, 128);
    assert(base[i]);
    pointers[i] = static_cast<unsigned char *>(base[i]) + padding;
    if (padding) {
      const unsigned char pattern = 91;
      check(clEnqueueSVMMemFill(queue, base[i], &pattern, 1, lengths[i] + 2 * padding, 0, nullptr, nullptr));
    }
    check(clEnqueueSVMMemcpy(queue, CL_TRUE, pointers[i], host[i], lengths[i], 0, nullptr, nullptr));
    // Deliberately replace already-bound cl_mem arguments with SVM pointers.
    check(clSetKernelArgSVMPointer(kernel, i, pointers[i]));
  }
  if (mode == "svm-mixed")
    check(clSetKernelArg(kernel, 1, sizeof(cl_mem), buffers + 1));
  if (mode == "svm-rebind")
    for (unsigned i = 0; i < 3; ++i)
      check(clSetKernelArg(kernel, i, sizeof(cl_mem), buffers + i));
  check(clSetKernelArg(kernel, 3, sizeof(m), &m));
  check(clSetKernelArg(kernel, 4, sizeof(n), &n));
  check(clSetKernelArg(kernel, 5, sizeof(k), &k));
  auto gate = clCreateUserEvent(context, &error);
  check(error);
  const size_t grid[] = {8, 32, 1};
  cl_event done;
  auto before = std::chrono::steady_clock::now();
  check(clEnqueueNDRangeKernel(queue, kernel, 3, nullptr, grid, nullptr, 1, &gate, &done));
  assert(std::chrono::steady_clock::now() - before < std::chrono::seconds(1));
  check(clSetUserEventStatus(gate, mode == "dependency-failure" ? -1 : CL_COMPLETE));
  check(clReleaseEvent(gate));
  error = clWaitForEvents(1, &done);
  if (fail) assert(error == CL_EXEC_STATUS_ERROR_FOR_EVENTS_IN_WAIT_LIST);
  else check(error);
  // A separate queue can inspect output even when the launch queue failed.
  auto read_queue = clCreateCommandQueue(context, device, 0, &error);
  check(error);
  if (mode == "svm-rebind")
    check(clEnqueueReadBuffer(read_queue, buffers[2], CL_TRUE, 0, lengths[2], c.data(), 0, nullptr, nullptr));
  else
    check(clEnqueueSVMMemcpy(read_queue, CL_TRUE, c.data(), pointers[2], lengths[2], 0, nullptr, nullptr));
  for (size_t i = 0; i < m; ++i)
    for (size_t j = 0; j < n; ++j) {
      cl_long expected = fail ? 123456789 : 0;
      if (!fail)
        for (size_t z = 0; z < k; ++z) expected += cl_long(a[i * k + z]) * b[z * n + j];
      assert(c[i * n + j] == expected);
    }
  if (padding) {
    std::vector<unsigned char> contents(lengths[2] + 2 * padding);
    check(clEnqueueSVMMemcpy(read_queue, CL_TRUE, contents.data(), base[2], contents.size(), 0, nullptr, nullptr));
    for (size_t i = 0; i < padding; ++i)
      assert(contents[i] == 91 && contents[contents.size() - i - 1] == 91);
  }
  if (mode == "svm-free") {
    cl_event freed;
    check(clEnqueueSVMFree(read_queue, 3, base, nullptr, nullptr, 0, nullptr, &freed));
    check(clWaitForEvents(1, &freed));
    check(clReleaseEvent(freed));
  } else {
    for (auto p : base) clSVMFree(context, p);
  }
  for (auto buffer : buffers) check(clReleaseMemObject(buffer));
  check(clReleaseEvent(done));
  check(clReleaseKernel(kernel));
  check(clReleaseProgram(program));
  check(clReleaseCommandQueue(read_queue));
  check(clReleaseCommandQueue(queue));
  check(clReleaseContext(context));
  std::printf("SVM %s: exact result, dependency and output boundaries passed\n", mode.c_str());
}

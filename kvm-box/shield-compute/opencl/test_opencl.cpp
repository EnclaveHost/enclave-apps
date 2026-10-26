#define CL_TARGET_OPENCL_VERSION 300
#define CL_USE_DEPRECATED_OPENCL_1_2_APIS
#include <CL/cl.h>
#include <cassert>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <future>
#include <iterator>
#include <string>
#include <thread>
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
static cl_ulong minimum_profile_time() {
  const char *value = std::getenv("SHIELD_TEST_MIN_DURATION_NS");
  return value ? std::strtoull(value, nullptr, 10) : 0;
}
static cl_kernel build(cl_context c, cl_device_id d, const std::string &s,
                       const char *name, bool from_binary = false) {
  const char *p = s.c_str();
  cl_int err;
  auto program = clCreateProgramWithSource(c, 1, &p, nullptr, &err);
  check(err);
  err = clBuildProgram(program, 1, &d, "-cl-std=CL1.2", nullptr, nullptr);
  if (err) {
    char log[16384];
    clGetProgramBuildInfo(program, d, CL_PROGRAM_BUILD_LOG, sizeof(log), log,
                          nullptr);
    std::fputs(log, stderr);
    check(err);
  }
  if (from_binary) {
    size_t bytes;
    check(clGetProgramInfo(program, CL_PROGRAM_BINARY_SIZES, sizeof(bytes), &bytes, nullptr));
    std::vector<unsigned char> binary(bytes);
    unsigned char *destination = binary.data();
    check(clGetProgramInfo(program, CL_PROGRAM_BINARIES, sizeof(destination), &destination, nullptr));
    check(clReleaseProgram(program));
    const unsigned char *data = binary.data();
    cl_int binary_status;
    program = clCreateProgramWithBinary(c, 1, &d, &bytes, &data, &binary_status, &err);
    check(err);
    check(binary_status);
    check(clBuildProgram(program, 1, &d, "", nullptr, nullptr));
  }
  auto kernel = clCreateKernel(program, name, &err);
  check(err);
  check(clReleaseProgram(program));
  return kernel;
}
int main(int argc, char **argv) {
  const std::string mode = argc > 1 ? argv[1] : "masked";
  const bool profiling = mode.starts_with("profiling");
  bool dependency_failure =
      mode == "dependency-failure" || mode == "profiling-dependency-failure";
  bool fail =
      dependency_failure || mode == "failure" || mode == "profiling-failure" || mode == "profiling-early-failure";
  cl_platform_id platform;
  cl_device_id device;
  cl_uint count;
  check(clGetPlatformIDs(1, &platform, &count));
  check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, &count));
  cl_device_type type;
  check(clGetDeviceInfo(device, CL_DEVICE_TYPE, sizeof(type), &type, nullptr));
  assert(type == CL_DEVICE_TYPE_GPU);
  char name[256];
  check(clGetDeviceInfo(device, CL_DEVICE_NAME, sizeof(name), name, nullptr));
  std::printf("Device: %s\n", name);
  cl_int err;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &err);
  check(err);
  const cl_command_queue_properties properties = (profiling ? CL_QUEUE_PROFILING_ENABLE : 0) |
      (mode == "out-of-order" || mode == "profiling-out-of-order" ? CL_QUEUE_OUT_OF_ORDER_EXEC_MODE_ENABLE : 0);
  auto queue = clCreateCommandQueue(context, device, properties, &err);
  check(err);
  // A real float/transcendental kernel exercises the unmodified CPU compiler.
  auto floating =
      build(context, device,
            "__kernel void nonlinear(__global const float*x,__global "
            "float*y){size_t i=get_global_id(0);y[i]=sin(x[i])+x[i]*x[i];}",
            "nonlinear");
  std::vector<float> x(64), y(64);
  for (size_t i = 0; i < x.size(); i++)
    x[i] = float(i) / 64;
  auto input = clCreateBuffer(context, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR,
                              x.size() * sizeof(float), x.data(), &err);
  check(err);
  auto output = clCreateBuffer(context, CL_MEM_WRITE_ONLY,
                               y.size() * sizeof(float), nullptr, &err);
  check(err);
  check(clSetKernelArg(floating, 0, sizeof(input), &input));
  check(clSetKernelArg(floating, 1, sizeof(output), &output));
  size_t size = x.size();
  cl_event done;
  check(clEnqueueNDRangeKernel(queue, floating, 1, nullptr, &size, nullptr, 0,
                               nullptr, &done));
  check(clEnqueueReadBuffer(queue, output, CL_TRUE, 0, y.size() * sizeof(float),
                            y.data(), 1, &done, nullptr));
  for (size_t i = 0; i < y.size(); i++)
    assert(std::abs(y[i] - (std::sin(x[i]) + x[i] * x[i])) < 2e-6f);
  cl_ulong previous_end = 0;
  if (profiling)
    check(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_END, sizeof(previous_end), &previous_end, nullptr));
  check(clReleaseEvent(done));
  check(clReleaseMemObject(input));
  check(clReleaseMemObject(output));
  check(clReleaseKernel(floating));
  std::puts("Float/transcendental CPU fallback: exact tolerance passed");
  std::ifstream source(SHIELD_TEST_KERNEL);
  assert(source.good());
  std::string text((std::istreambuf_iterator<char>(source)), {});
  auto gemm = build(context, device, text, "gemm", mode == "binary");
  const cl_uint m = 4, n = 16, k = 32;
  std::vector<cl_char> a(m * k), b(k * n);
  std::vector<cl_long> c(m * n, 123456789);
  for (size_t i = 0; i < a.size(); i++)
    a[i] = int(i % 17) - 8;
  for (size_t i = 0; i < b.size(); i++)
    b[i] = int(i % 5) - 2;
  cl_mem buffers[3];
  buffers[0] = clCreateBuffer(context, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR,
                              a.size(), a.data(), &err);
  check(err);
  buffers[1] = clCreateBuffer(context, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR,
                              b.size(), b.data(), &err);
  check(err);
  buffers[2] = clCreateBuffer(context, CL_MEM_READ_WRITE | CL_MEM_COPY_HOST_PTR,
                              c.size() * sizeof(cl_long), c.data(), &err);
  check(err);
  cl_mem output_parent = nullptr;
  size_t padding = 0;
  if (mode == "subbuffers") {
    cl_uint alignment;
    check(clGetDeviceInfo(device, CL_DEVICE_MEM_BASE_ADDR_ALIGN, sizeof(alignment), &alignment, nullptr));
    padding = (alignment + 7) / 8;
    const void *data[] = {a.data(), b.data(), c.data()};
    const size_t bytes[] = {a.size(), b.size(), c.size() * sizeof(cl_long)};
    for (int i = 0; i < 3; i++) {
      check(clReleaseMemObject(buffers[i]));
      std::vector<unsigned char> storage(padding + bytes[i] + padding, 91);
      std::memcpy(storage.data() + padding, data[i], bytes[i]);
      auto parent = clCreateBuffer(context, CL_MEM_READ_WRITE | CL_MEM_COPY_HOST_PTR,
                                   storage.size(), storage.data(), &err);
      check(err);
      cl_buffer_region region{padding, bytes[i]};
      buffers[i] = clCreateSubBuffer(parent, CL_MEM_READ_WRITE, CL_BUFFER_CREATE_TYPE_REGION, &region, &err);
      check(err);
      if (i == 2)
        output_parent = parent;
      else
        check(clReleaseMemObject(parent));
    }
  }
  for (cl_uint i = 0; i < 3; i++)
    check(clSetKernelArg(gemm, i, sizeof(cl_mem), buffers + i));
  check(clSetKernelArg(gemm, 3, sizeof(m), &m));
  check(clSetKernelArg(gemm, 4, sizeof(n), &n));
  check(clSetKernelArg(gemm, 5, sizeof(k), &k));
  if (mode == "clone") {
    auto cloned = clCloneKernel(gemm, &err);
    check(err);
    check(clReleaseKernel(gemm));
    gemm = cloned;
  }
  auto gate = clCreateUserEvent(context, &err);
  check(err);
  size_t grid[] = {8, 32}; // padded grid must not change the result
  cl_ulong before_enqueue = 0, after_enqueue = 0, gate_time = 0, host_time;
  if (profiling) check(clGetDeviceAndHostTimer(device, &before_enqueue, &host_time));
  auto before = std::chrono::steady_clock::now();
  check(clEnqueueNDRangeKernel(queue, gemm, 2, nullptr, grid, nullptr, 1, &gate,
                               &done));
  assert(std::chrono::steady_clock::now() - before < std::chrono::seconds(1));
  if (profiling) check(clGetDeviceAndHostTimer(device, &after_enqueue, &host_time));
  cl_command_type command;
  check(clGetEventInfo(done, CL_EVENT_COMMAND_TYPE, sizeof(command), &command,
                       nullptr));
  assert(command == CL_COMMAND_NDRANGE_KERNEL);
  cl_ulong timestamp;
  assert(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_START, sizeof(timestamp), &timestamp, nullptr) == CL_PROFILING_INFO_NOT_AVAILABLE);
  struct CallbackState {
    std::promise<void> done;
    bool fail, retain;
  } callback_state{{}, fail, mode == "profiling-callback-retain"};
  auto callback_future = callback_state.done.get_future();
  const bool early_release = mode == "profiling-early-release" || mode == "profiling-early-failure" || mode == "profiling-callback-retain";
  if (early_release) {
    check(clSetEventCallback(done, CL_COMPLETE, [](cl_event e, cl_int status, void *data) {
      auto *state = static_cast<CallbackState *>(data);
      assert(state->fail ? status < 0 : status == CL_COMPLETE);
      cl_command_type type;
      check(clGetEventInfo(e, CL_EVENT_COMMAND_TYPE, sizeof(type), &type, nullptr));
      assert(type == CL_COMMAND_NDRANGE_KERNEL);
      cl_ulong start, end;
      if (state->fail) {
        assert(clGetEventProfilingInfo(e, CL_PROFILING_COMMAND_END, sizeof(end), &end, nullptr) == CL_PROFILING_INFO_NOT_AVAILABLE);
      } else {
        check(clGetEventProfilingInfo(e, CL_PROFILING_COMMAND_START, sizeof(start), &start, nullptr));
        check(clGetEventProfilingInfo(e, CL_PROFILING_COMMAND_END, sizeof(end), &end, nullptr));
        assert(end >= start && end - start >= minimum_profile_time());
      }
      if (state->retain) check(clRetainEvent(e));
      state->done.set_value();
    }, &callback_state));
    check(clReleaseEvent(done));
  } else {
    check(clRetainEvent(done));
    check(clReleaseEvent(done)); // Retain/release must preserve logical metadata.
  }
  if (profiling) {
    std::this_thread::sleep_for(std::chrono::milliseconds(30));
    check(clGetDeviceAndHostTimer(device, &gate_time, &host_time));
  }
  check(clSetUserEventStatus(gate, dependency_failure ? -1 : CL_COMPLETE));
  check(clReleaseEvent(gate));
  if (early_release) {
    assert(callback_future.wait_for(std::chrono::seconds(30)) == std::future_status::ready);
    auto read_queue = clCreateCommandQueue(context, device, 0, &err);
    check(err);
    check(clEnqueueReadBuffer(read_queue, buffers[2], CL_TRUE, 0, c.size()*sizeof(cl_long), c.data(), 0, nullptr, nullptr));
    for (size_t i=0; i<m; ++i) for (size_t j=0; j<n; ++j) {
      cl_long expected = 0;
      for (size_t z=0; z<k; ++z) expected += cl_long(a[i*k+z])*b[z*n+j];
      assert(c[i*n+j] == (fail ? 123456789 : expected));
    }
    check(clReleaseCommandQueue(read_queue));
    if (callback_state.retain) {
      check(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_END, sizeof(timestamp), &timestamp, nullptr));
      check(clReleaseEvent(done));
    }
    for (auto buffer : buffers) check(clReleaseMemObject(buffer));
    check(clReleaseKernel(gemm)); check(clReleaseCommandQueue(queue)); check(clReleaseContext(context));
    std::puts("Profiling callback after caller release: logical type, timing/error, output and retained lifetime passed");
    return 0;
  }
  err = clWaitForEvents(1, &done);
  if (fail) {
    assert(err == CL_EXEC_STATUS_ERROR_FOR_EVENTS_IN_WAIT_LIST);
    assert(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_END, sizeof(timestamp), &timestamp, nullptr) == CL_PROFILING_INFO_NOT_AVAILABLE);
    auto read_queue = clCreateCommandQueue(context, device, 0, &err);
    check(err);
    check(clEnqueueReadBuffer(read_queue, buffers[2], CL_TRUE, 0,
                              c.size() * sizeof(cl_long), c.data(), 0, nullptr,
                              nullptr));
    for (auto value : c)
      assert(value == 123456789);
    check(clReleaseCommandQueue(read_queue));
    std::puts("Failed service: error event and untouched output passed");
  } else {
    check(err);
    check(clEnqueueReadBuffer(queue, buffers[2], CL_TRUE, 0,
                              c.size() * sizeof(cl_long), c.data(), 1, &done,
                              nullptr));
    for (size_t i = 0; i < m; i++)
      for (size_t j = 0; j < n; j++) {
        cl_long expected = 0;
        for (size_t z = 0; z < k; z++)
          expected += cl_long(a[i * k + z]) * b[z * n + j];
        assert(c[i * n + j] == expected);
      }
    std::puts("Integer GEMM: padded grid, asynchronous dependency, exact "
              "reference passed");
    if (profiling) {
      cl_ulong times[5];
      for (unsigned i = 0; i < 5; ++i) {
        size_t bytes = 0;
        check(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_QUEUED+i, 0, nullptr, &bytes));
        assert(bytes == sizeof(cl_ulong));
        check(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_QUEUED+i, sizeof(times[i]), times+i, nullptr));
        if (i) assert(times[i-1] <= times[i]);
      }
      assert(times[2] >= previous_end);
      assert(before_enqueue <= times[0] && times[0] <= after_enqueue);
      assert(times[1] <= after_enqueue && times[2] >= gate_time);
      assert(times[3]-times[2] >= minimum_profile_time());
      assert(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_START, 1, &timestamp, nullptr) == CL_INVALID_VALUE);
      assert(clGetEventProfilingInfo(done, 0, sizeof(timestamp), &timestamp, nullptr) == CL_INVALID_VALUE);
      cl_event after;
      check(clEnqueueMarkerWithWaitList(queue, 1, &done, &after));
      check(clWaitForEvents(1, &after));
      check(clGetEventProfilingInfo(after, CL_PROFILING_COMMAND_START, sizeof(timestamp), &timestamp, nullptr));
      assert(timestamp >= times[4]);
      check(clReleaseEvent(after));
      std::printf("Logical profiling: ordered timestamps, service delay included (%llu ns), CPU clock continuity passed\n", (unsigned long long)(times[3]-times[2]));
    } else {
      assert(clGetEventProfilingInfo(done, CL_PROFILING_COMMAND_END, sizeof(timestamp), &timestamp, nullptr) == CL_PROFILING_INFO_NOT_AVAILABLE);
    }
    if (output_parent) {
      std::vector<unsigned char> contents(padding + c.size() * sizeof(cl_long) + padding);
      check(clEnqueueReadBuffer(queue, output_parent, CL_TRUE, 0, contents.size(), contents.data(), 0, nullptr, nullptr));
      for (size_t i = 0; i < padding; i++) {
        assert(contents[i] == 91);
        assert(contents[contents.size() - 1 - i] == 91);
      }
      check(clReleaseMemObject(output_parent));
    }
  }
  if (mode == "profiling-late-release") {
    check(clSetEventCallback(done, CL_COMPLETE, [](cl_event e, cl_int status, void *data) {
      assert(status == CL_COMPLETE);
      cl_ulong end;
      check(clGetEventProfilingInfo(e, CL_PROFILING_COMMAND_END, sizeof(end), &end, nullptr));
      check(clReleaseEvent(e)); // Consume the caller's reference from a late callback.
      static_cast<CallbackState *>(data)->done.set_value();
    }, &callback_state));
    assert(callback_future.wait_for(std::chrono::seconds(30)) == std::future_status::ready);
  } else check(clReleaseEvent(done));
  for (auto buffer : buffers)
    check(clReleaseMemObject(buffer));
  check(clReleaseKernel(gemm));
  check(clReleaseCommandQueue(queue));
  check(clReleaseContext(context));
  std::puts("All OpenCL checks passed");
}

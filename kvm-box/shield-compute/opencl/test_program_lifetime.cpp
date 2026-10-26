// Deterministic driver-boundary interleavings. The fake driver recycles one
// program address after its last reference is freed, before release returns.
// No GPU, compiler semantics, or confidential execution is simulated here.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <atomic>
#include <cassert>
#include <cstring>
#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <thread>

#ifdef SHIELD_TEST_DRIVER
namespace {
alignas(void *) char program_storage[8], device_storage[8];
std::atomic<unsigned> references{0};
void (*release_hook)() = nullptr;
void (*compile_hook)() = nullptr;
void (*link_hook)() = nullptr;
alignas(void *) char linked_storage[8];
std::atomic<unsigned> linked_references{0};
unsigned source_queries = 0;
unsigned queue_queries = 0, native_launches = 0;
alignas(void *) char kernel_storage[8];
cl_int info(const void *value, size_t bytes, size_t size, void *out, size_t *actual) {
  if (actual) *actual = bytes;
  if (out) {
    if (size < bytes) return CL_INVALID_VALUE;
    std::memcpy(out, value, bytes);
  }
  return CL_SUCCESS;
}
}
extern "C" {
void shield_test_release_hook(void (*hook)()) { release_hook = hook; }
void shield_test_compile_hook(void (*hook)()) { compile_hook = hook; }
void shield_test_link_hook(void (*hook)()) { link_hook = hook; }
unsigned shield_test_source_queries() { return source_queries; }
unsigned shield_test_queue_queries() { return queue_queries; }
unsigned shield_test_native_launches() { return native_launches; }
cl_int clSetKernelArg(cl_kernel, cl_uint, size_t, const void *) { return CL_SUCCESS; }
cl_int clGetCommandQueueInfo(cl_command_queue, cl_command_queue_info, size_t, void *, size_t *) {
  ++queue_queries;
  return CL_INVALID_COMMAND_QUEUE;
}
cl_int clEnqueueNDRangeKernel(cl_command_queue, cl_kernel, cl_uint, const size_t *,
                              const size_t *, const size_t *, cl_uint,
                              const cl_event *, cl_event *) {
  ++native_launches;
  return CL_SUCCESS;
}
cl_kernel clCreateKernel(cl_program, const char *, cl_int *error) {
  if (error) *error = CL_SUCCESS;
  return reinterpret_cast<cl_kernel>(kernel_storage);
}
cl_int clReleaseKernel(cl_kernel) { return CL_SUCCESS; }
cl_program clCreateProgramWithIL(cl_context, const void *, size_t, cl_int *error) {
  references = 1;
  if (error) *error = CL_SUCCESS;
  return reinterpret_cast<cl_program>(program_storage);
}
cl_program clCreateProgramWithSource(cl_context, cl_uint, const char **,
                                     const size_t *, cl_int *error) {
  references = 1;
  if (error) *error = CL_SUCCESS;
  return reinterpret_cast<cl_program>(program_storage);
}
cl_int clRetainProgram(cl_program) { ++references; return CL_SUCCESS; }
cl_int clReleaseProgram(cl_program program) {
  auto &count = program == reinterpret_cast<cl_program>(linked_storage)
                    ? linked_references : references;
  assert(count);
  if (!--count && release_hook) {
    auto hook = release_hook;
    release_hook = nullptr;
    // Join forces the new allocation/build to finish while the old frontend
    // release call is still in the driver. Ordinary allocator reuse permits it.
    std::thread replacement(hook);
    replacement.join();
  }
  return CL_SUCCESS;
}
cl_int clBuildProgram(cl_program, cl_uint, const cl_device_id *, const char *,
                     void (CL_CALLBACK *)(cl_program, void *), void *) {
  return CL_SUCCESS;
}
cl_int clCompileProgram(cl_program, cl_uint, const cl_device_id *, const char *,
                        cl_uint, const cl_program *, const char **,
                        void (CL_CALLBACK *)(cl_program, void *), void *) {
  if (compile_hook) {
    auto hook = compile_hook;
    compile_hook = nullptr;
    std::thread(hook).join();
  }
  return CL_SUCCESS;
}
cl_program clLinkProgram(cl_context, cl_uint, const cl_device_id *, const char *,
                         cl_uint, const cl_program *,
                         void (CL_CALLBACK *)(cl_program, void *), void *, cl_int *error) {
  if (link_hook) {
    auto hook = link_hook;
    link_hook = nullptr;
    std::thread(hook).join();
  }
  linked_references = 1;
  if (error) *error = CL_SUCCESS;
  return reinterpret_cast<cl_program>(linked_storage);
}
cl_int clGetProgramInfo(cl_program, cl_program_info name, size_t size,
                       void *out, size_t *actual) {
  cl_uint count = 1;
  auto device = reinterpret_cast<cl_device_id>(device_storage);
  if (name == CL_PROGRAM_NUM_DEVICES) return info(&count, sizeof(count), size, out, actual);
  if (name == CL_PROGRAM_DEVICES) return info(&device, sizeof(device), size, out, actual);
  if (name == CL_PROGRAM_SOURCE) {
    ++source_queries;
    if (std::getenv("SHIELD_LIFETIME_SOURCE_PLAN")) {
      const char text[] = "kernel void gemm() {}";
      return info(text, sizeof(text), size, out, actual);
    }
  }
  return CL_INVALID_VALUE;
}
cl_int clGetProgramBuildInfo(cl_program, cl_device_id, cl_program_build_info name,
                             size_t size, void *out, size_t *actual) {
  const char options[] = "-cl-std=CL1.2";
  if (name == CL_PROGRAM_BUILD_OPTIONS) return info(options, sizeof(options), size, out, actual);
  return CL_INVALID_VALUE;
}
cl_int clGetDeviceInfo(cl_device_id, cl_device_info name, size_t size,
                      void *out, size_t *actual) {
  cl_device_type type = CL_DEVICE_TYPE_CPU;
  if (name == CL_DEVICE_TYPE) return info(&type, sizeof(type), size, out, actual);
  return CL_INVALID_VALUE;
}
}
#else
extern "C" void shield_test_release_hook(void (*)());
extern "C" void shield_test_compile_hook(void (*)());
extern "C" void shield_test_link_hook(void (*)());
extern "C" unsigned shield_test_source_queries();
extern "C" unsigned shield_test_queue_queries();
extern "C" unsigned shield_test_native_launches();
static cl_program replacement;
static cl_program il(unsigned char marker) {
  cl_int error;
  auto p = clCreateProgramWithIL(nullptr, &marker, 1, &error);
  assert(p && error == CL_SUCCESS);
  return p;
}
static cl_program source() {
  const char *text = "kernel void replacement() {}";
  cl_int error;
  auto p = clCreateProgramWithSource(nullptr, 1, &text, nullptr, &error);
  assert(p && error == CL_SUCCESS);
  return p;
}
static void build(cl_program p) {
  assert(clBuildProgram(p, 0, nullptr, nullptr, nullptr, nullptr) == CL_SUCCESS);
}
static void compile(cl_program p, const char *options = nullptr) {
  assert(clCompileProgram(p, 0, nullptr, options, 0, nullptr, nullptr, nullptr, nullptr) == CL_SUCCESS);
}
int main(int argc, char **argv) {
  assert(argc == 2);
  if (!std::strcmp(argv[1], "source-plan-cache")) {
    auto p = source(); build(p);
    // Repeat kernel creation to exercise both uncached and cached selection.
    for (unsigned repeat=0; repeat<3; ++repeat) {
      cl_int error;
      auto kernel = clCreateKernel(p, "gemm", &error);
      assert(kernel && error == CL_SUCCESS);
      cl_mem memory = reinterpret_cast<cl_mem>(uintptr_t(1));
      const cl_uint shape[3] = {4, 4, 32};
      for (unsigned i=0; i<3; ++i) {
        assert(clSetKernelArg(kernel,i,sizeof(memory),&memory) == CL_SUCCESS);
        assert(clSetKernelArg(kernel,i+3,sizeof(shape[i]),&shape[i]) == CL_SUCCESS);
      }
      const size_t global[2] = {4, 4};
      assert(clEnqueueNDRangeKernel(nullptr,kernel,2,nullptr,global,nullptr,0,nullptr,nullptr) == CL_SUCCESS);
      assert(clReleaseKernel(kernel) == CL_SUCCESS);
    }
    // An accepted integer plan reaches queue validation, while all other plans
    // must go straight to native execution. No fake GPU results are supplied.
    assert(shield_test_native_launches() == 3);
    const bool positive = !std::strcmp(std::getenv("SHIELD_LIFETIME_SOURCE_PLAN"), "integer");
    assert(shield_test_queue_queries() == (positive ? 3u : 0u));
    assert(clReleaseProgram(p) == CL_SUCCESS);
    return 0;
  }
  auto old = il(0x11);
  if (!std::strcmp(argv[1], "overlap-compile") ||
      !std::strcmp(argv[1], "overlap-link") ||
      !std::strcmp(argv[1], "stable-link")) {
    replacement = old;
    if (!std::strcmp(argv[1], "overlap-compile"))
      shield_test_compile_hook([] { compile(replacement, "-cl-fast-relaxed-math"); });
    compile(old);
    if (!std::strcmp(argv[1], "overlap-link"))
      shield_test_link_hook([] { compile(replacement, "-cl-fast-relaxed-math"); });
    cl_int error;
    auto linked = clLinkProgram(nullptr, 0, nullptr, nullptr, 1, &old, nullptr, nullptr, &error);
    assert(linked && error == CL_SUCCESS);
    auto kernel = clCreateKernel(linked, "gemm", &error);
    assert(kernel && error == CL_SUCCESS);
    assert(shield_test_source_queries() == (std::strcmp(argv[1], "stable-link") ? 1u : 0u));
    assert(clReleaseKernel(kernel) == CL_SUCCESS);
    assert(clReleaseProgram(linked) == CL_SUCCESS);
    assert(clReleaseProgram(old) == CL_SUCCESS);
    return 0;
  }
  if (!std::strcmp(argv[1], "overlap-build")) {
    std::filesystem::path barrier = std::getenv("SHIELD_LIFETIME_BARRIER");
    std::thread first([&] { build(old); });
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    while (!std::filesystem::exists(barrier.string() + ".ready")) {
      assert(std::chrono::steady_clock::now() < deadline);
      std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    // The first driver's build has returned, but its frontend analysis is
    // pending. A successful rebuild with unapproved options supersedes it.
    assert(clBuildProgram(old, 0, nullptr, "-cl-fast-relaxed-math", nullptr, nullptr) == CL_SUCCESS);
    std::ofstream(barrier.string() + ".continue").put('\n');
    first.join();
    cl_int error;
    auto kernel = clCreateKernel(old, "gemm", &error);
    assert(kernel && error == CL_SUCCESS);
    // Without an IL plan, recognition must fall back to inspecting source.
    assert(shield_test_source_queries() == 1);
    assert(clReleaseKernel(kernel) == CL_SUCCESS);
    assert(clReleaseProgram(old) == CL_SUCCESS);
    return 0;
  }
  if (!std::strcmp(argv[1], "retain")) {
    assert(clRetainProgram(old) == CL_SUCCESS);
    assert(clReleaseProgram(old) == CL_SUCCESS);
    build(old);
    assert(clReleaseProgram(old) == CL_SUCCESS);
    replacement = source();
    build(replacement);
  } else {
    build(old);
    if (!std::strcmp(argv[1], "replace-source"))
      shield_test_release_hook([] { replacement = source(); build(replacement); });
    else {
      assert(!std::strcmp(argv[1], "replace-il"));
      shield_test_release_hook([] { replacement = il(0x22); });
    }
    assert(clReleaseProgram(old) == CL_SUCCESS);
    assert(replacement == old); // The adverse allocator interleaving happened.
    if (!std::strcmp(argv[1], "replace-il")) build(replacement);
  }
  assert(clReleaseProgram(replacement) == CL_SUCCESS);
}
#endif

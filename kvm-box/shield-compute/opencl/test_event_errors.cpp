#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <atomic>
#include <cassert>
#include <chrono>
#include <cstdio>
#include <thread>

static void CL_CALLBACK callback(cl_event, cl_int status, void *data) {
  static_cast<std::atomic<int> *>(data)->store(status);
}
static void CL_CALLBACK release_callback(cl_event event, cl_int status, void *data) {
  assert(clReleaseEvent(event) == CL_SUCCESS);
  static_cast<std::atomic<int> *>(data)->store(status);
}
static void wait_status(std::atomic<int> &value, int expected) {
  for (int i = 0; i < 200 && value.load() == 999; i++)
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
  assert(value.load() == expected);
}
int main() {
  cl_platform_id platform;
  cl_device_id device;
  assert(clGetPlatformIDs(1, &platform, nullptr) == CL_SUCCESS);
  assert(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr) == CL_SUCCESS);
  cl_int error;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &error);
  assert(context && error == CL_SUCCESS);
  auto event = clCreateUserEvent(context, &error);
  assert(event && error == CL_SUCCESS);
  std::atomic<int> early{999}, late{999};
  assert(clSetEventCallback(event, CL_COMPLETE, callback, &early) == CL_SUCCESS);
  assert(clSetUserEventStatus(event, CL_INVALID_VALUE) == CL_SUCCESS);
  wait_status(early, CL_INVALID_VALUE);
  assert(clSetEventCallback(event, CL_COMPLETE, callback, &late) == CL_SUCCESS);
  wait_status(late, CL_INVALID_VALUE);
  assert(clWaitForEvents(1, &event) == CL_EXEC_STATUS_ERROR_FOR_EVENTS_IN_WAIT_LIST);
  assert(clReleaseEvent(event) == CL_SUCCESS);
  // A callback can consume the application's final event reference, both
  // when queued and when registration immediately invokes the callback.
  for (bool late_registration : {false, true}) {
    event = clCreateUserEvent(context, &error);
    assert(event && error == CL_SUCCESS);
    std::atomic<int> released{999};
    if (late_registration)
      assert(clSetUserEventStatus(event, CL_INVALID_VALUE) == CL_SUCCESS);
    assert(clSetEventCallback(event, CL_COMPLETE, release_callback, &released) == CL_SUCCESS);
    if (!late_registration)
      assert(clSetUserEventStatus(event, CL_INVALID_VALUE) == CL_SUCCESS);
    wait_status(released, CL_INVALID_VALUE);
  }
  assert(clReleaseContext(context) == CL_SUCCESS);
  std::puts("Early and late callbacks receive the actual negative event status");
}

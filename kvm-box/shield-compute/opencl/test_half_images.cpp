// Actual OpenCL image reads/writes; independent host half conversion reference.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <CL/cl_half.h>
#include <array>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

static void check(cl_int error, const char *what) {
  if (error != CL_SUCCESS) {
    std::fprintf(stderr, "%s: OpenCL error %d\n", what, error);
    std::exit(1);
  }
}

struct Shape {
  const char *name;
  cl_mem_object_type type;
  size_t height, depth, layers;
  const char *icoord, *fcoord;
};

int main() {
  cl_platform_id platform;
  cl_device_id device;
  check(clGetPlatformIDs(1, &platform, nullptr), "platform");
  check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr), "logical GPU");
  cl_int error;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &error);
  check(error, "context");
  auto queue = clCreateCommandQueueWithProperties(context, device, nullptr, &error);
  check(error, "queue");
  const Shape shapes[] = {
    {"image1d_t", CL_MEM_OBJECT_IMAGE1D, 0, 0, 0, "i", "(float)i + 0.25f"},
    {"image1d_buffer_t", CL_MEM_OBJECT_IMAGE1D_BUFFER, 0, 0, 0, "i", nullptr},
    {"image1d_array_t", CL_MEM_OBJECT_IMAGE1D_ARRAY, 0, 0, 2, "(int2)(i%4,i/4)", "(float2)(i%4+0.25f,i/4)"},
    {"image2d_t", CL_MEM_OBJECT_IMAGE2D, 2, 0, 0, "(int2)(i%4,i/4)", "(float2)(i%4+0.25f,i/4+0.25f)"},
    {"image2d_array_t", CL_MEM_OBJECT_IMAGE2D_ARRAY, 2, 0, 2, "(int4)(i%4,(i/4)%2,i/8,0)", "(float4)(i%4+0.25f,(i/4)%2+0.25f,i/8,0)"},
    {"image3d_t", CL_MEM_OBJECT_IMAGE3D, 2, 2, 0, "(int4)(i%4,(i/4)%2,i/8,0)", "(float4)(i%4+0.25f,(i/4)%2+0.25f,i/8+0.25f,0)"},
  };
  unsigned cases = 0;
  for (const auto &shape : shapes) {
    const size_t count = 4 * (shape.height ? shape.height : 1) *
        (shape.depth ? shape.depth : 1) * (shape.layers ? shape.layers : 1);
    for (auto channel : {CL_HALF_FLOAT, CL_UNORM_INT8, CL_SNORM_INT8}) {
      std::vector<cl_half> expected(count * 4), half_input(count * 4);
      std::vector<unsigned char> byte_input(count * 4);
      for (size_t i = 0; i < count * 4; ++i) {
        if (channel == CL_HALF_FLOAT) {
          float value = float(int(i % 29) - 14) * 0.125f;
          expected[i] = half_input[i] = cl_half_from_float(value, CL_HALF_RTE);
        } else if (channel == CL_UNORM_INT8) {
          byte_input[i] = static_cast<unsigned char>((i * 37) % 256);
          expected[i] = cl_half_from_float(float(byte_input[i]) / 255.0f, CL_HALF_RTE);
        } else {
          const int value = int((i * 37) % 256) - 128;
          byte_input[i] = static_cast<unsigned char>(value);
          expected[i] = cl_half_from_float(value == -128 ? -1.0f : float(value) / 127.0f, CL_HALF_RTE);
        }
      }
      cl_image_format format{CL_RGBA, static_cast<cl_channel_type>(channel)};
      cl_image_desc descriptor{};
      descriptor.image_type = shape.type;
      descriptor.image_width = 4;
      descriptor.image_height = shape.height;
      descriptor.image_depth = shape.depth;
      descriptor.image_array_size = shape.layers;
      const size_t bytes = count * 4 * (channel == CL_HALF_FLOAT ? 2 : 1);
      void *data = channel == CL_HALF_FLOAT ? static_cast<void *>(half_input.data()) : byte_input.data();
      cl_mem backing = nullptr;
      if (shape.type == CL_MEM_OBJECT_IMAGE1D_BUFFER) {
        backing = clCreateBuffer(context, CL_MEM_READ_WRITE | CL_MEM_COPY_HOST_PTR, bytes, data, &error);
        check(error, "image buffer");
        descriptor.buffer = backing;
      }
      auto image = clCreateImage(context, CL_MEM_READ_WRITE | (backing ? 0 : CL_MEM_COPY_HOST_PTR),
          &format, &descriptor, backing ? nullptr : data, &error);
      check(error, "image");
      auto output = clCreateBuffer(context, CL_MEM_READ_WRITE, expected.size() * 2, nullptr, &error);
      check(error, "output");
      std::string source = "#pragma OPENCL EXTENSION cl_khr_fp16 : enable\n";
      auto read_kernel = [&](const std::string &name, const char *qualifier, const std::string &call) {
        source += "kernel void " + name + "(" + qualifier + " " + shape.name +
            " image, global ushort4 *out) { int i=get_global_id(0); out[i]=as_ushort4(" + call + "); }\n";
      };
      read_kernel("plain", "read_only", std::string("read_imageh(image,") + shape.icoord + ")");
      read_kernel("rw", "read_write", std::string("read_imageh(image,") + shape.icoord + ")");
      if (shape.fcoord) {
        std::string sampler = "CLK_NORMALIZED_COORDS_FALSE|CLK_ADDRESS_CLAMP_TO_EDGE|CLK_FILTER_NEAREST";
        read_kernel("sample_int", "read_only", "read_imageh(image," + sampler + "," + shape.icoord + ")");
        read_kernel("sample_float", "read_only", "read_imageh(image," + sampler + "," + shape.fcoord + ")");
      }
      // Separate commands avoid relying on intra-kernel image read/write ordering.
      for (auto qualifier : {"write_only", "read_write"}) {
        std::string name = qualifier == std::string("write_only") ? "write_wo" : "write_rw";
        source += "kernel void " + name + "(" + qualifier + " " + shape.name +
            " image, global const ushort4 *in) { int i=get_global_id(0); write_imageh(image," +
            shape.icoord + ",as_half4(in[i])); }\n";
      }
      const char *text = source.c_str();
      auto program = clCreateProgramWithSource(context, 1, &text, nullptr, &error);
      check(error, "program");
      error = clBuildProgram(program, 1, &device, "-cl-std=CL3.0", nullptr, nullptr);
      if (error) {
        size_t length;
        clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &length);
        std::vector<char> log(length);
        clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, length, log.data(), nullptr);
        std::fprintf(stderr, "%s\n", log.data());
      }
      check(error, "build");
      auto launch = [&](const char *name) {
        auto kernel = clCreateKernel(program, name, &error);
        check(error, "kernel");
        check(clSetKernelArg(kernel, 0, sizeof(image), &image), "image arg");
        check(clSetKernelArg(kernel, 1, sizeof(output), &output), "buffer arg");
        check(clEnqueueNDRangeKernel(queue, kernel, 1, nullptr, &count, nullptr, 0, nullptr, nullptr), "launch");
        check(clFinish(queue), "finish");
        clReleaseKernel(kernel);
      };
      auto compare = [&](const char *name) {
        std::vector<cl_half> actual(expected.size());
        check(clEnqueueReadBuffer(queue, output, CL_TRUE, 0, actual.size() * 2, actual.data(), 0, nullptr, nullptr), "read result");
        if (actual != expected) {
          for (size_t i = 0; i < actual.size(); ++i) if (actual[i] != expected[i]) {
            std::fprintf(stderr, "%s format=%u %s pixel-channel=%zu expected=%04x actual=%04x\n",
                shape.name, channel, name, i, expected[i], actual[i]);
            break;
          }
          std::exit(1);
        }
        ++cases;
      };
      for (auto name : {"plain", "rw", "sample_int", "sample_float"}) {
        if (!shape.fcoord && std::string(name).find("sample_") == 0) continue;
        launch(name);
        compare(name);
      }
      // Normalized storage has lossy round-trip conversion; writes here use
      // half storage so exact comparison is independent of that quantization.
      if (channel == CL_HALF_FLOAT) {
        for (auto name : {"write_wo", "write_rw"}) {
          for (auto &value : expected) value ^= 0x8000;
          check(clEnqueueWriteBuffer(queue, output, CL_TRUE, 0, expected.size() * 2, expected.data(), 0, nullptr, nullptr), "write input");
          launch(name);
          launch("plain");
          compare(name);
        }
      }
      clReleaseProgram(program);
      clReleaseMemObject(output);
      clReleaseMemObject(image);
      if (backing) clReleaseMemObject(backing);
    }
  }
  clReleaseCommandQueue(queue);
  clReleaseContext(context);
  std::printf("PASS: %u half-image read/write cases across six image shapes\n", cases);
}

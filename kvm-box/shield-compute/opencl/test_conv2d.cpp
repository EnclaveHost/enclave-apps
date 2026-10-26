// Execute pinned llama.cpp conv2d.cl against an independent NCHW convolution.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <CL/cl_half.h>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <iterator>
#include <string>
#include <vector>

static void check(cl_int e, const char *phase) {
  if (e) { std::fprintf(stderr, "%s: %d\n", phase, e); std::exit(1); }
}
struct Shape { unsigned co, ci, n, kw, kh, w, h, sx, sy, px, py, dx, dy; };

int main(int argc, char **argv) {
  if (argc != 2) { std::fprintf(stderr, "usage: test-conv2d PINNED_CONV2D_SOURCE\n"); return 2; }
  std::ifstream stream(argv[1]);
  if (!stream) return 2;
  std::string source((std::istreambuf_iterator<char>(stream)), {});
  const char *text = source.c_str();
  size_t length = source.size();
  cl_platform_id platform; cl_device_id device;
  check(clGetPlatformIDs(1, &platform, nullptr), "platform");
  check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr), "device");
  cl_int e;
  auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &e); check(e, "context");
  auto queue = clCreateCommandQueueWithProperties(context, device, nullptr, &e); check(e, "queue");
  auto program = clCreateProgramWithSource(context, 1, &text, &length, &e); check(e, "program");
  e = clBuildProgram(program, 1, &device,
      "-cl-std=CL3.0 -cl-mad-enable -cl-unsafe-math-optimizations -cl-finite-math-only -cl-fast-relaxed-math -DUSE_FP16=1",
      nullptr, nullptr);
  if (e) {
    size_t bytes = 0; clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &bytes);
    std::vector<char> log(bytes + 1, 0);
    clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, bytes, log.data(), nullptr);
    std::fprintf(stderr, "%s\n", log.data());
  }
  check(e, "build");
  auto kernel = clCreateKernel(program, "kernel_conv_2d", &e); check(e, "kernel");
  size_t total = 0; unsigned cases = 0;
  for (Shape s : {Shape{64,16,1,1,1,8,8,1,1,0,0,1,1},
                 Shape{7,3,1,3,3,7,5,1,1,1,1,1,1},
                 Shape{65,19,2,3,3,9,7,2,2,1,1,1,1},
                 Shape{5,2,2,3,2,11,9,1,2,2,1,2,2},
                 Shape{1,1,1,1,1,1,1,1,1,0,0,1,1}}) {
    unsigned ow = (s.w + 2*s.px - s.dx*(s.kw-1) - 1)/s.sx + 1;
    unsigned oh = (s.h + 2*s.py - s.dy*(s.kh-1) - 1)/s.sy + 1;
    size_t weights = s.co*s.ci*s.kh*s.kw, pixels = s.n*s.ci*s.h*s.w, outputs = s.n*s.co*oh*ow;
    constexpr size_t guard = 8;
    constexpr cl_half sentinel = 0x3555;
    std::vector<cl_half> a(weights + 2*guard, sentinel), b(pixels + 2*guard, sentinel), c(outputs + 2*guard, sentinel);
    for (size_t i = 0; i < weights; ++i) a[i+guard] = cl_half_from_float((int(i%9)-4)/8.0f, CL_HALF_RTE);
    for (size_t i = 0; i < pixels; ++i) b[i+guard] = cl_half_from_float((int(i%17)-8)/16.0f, CL_HALF_RTE);
    auto allocate = [&](std::vector<cl_half> &v) {
      auto memory = clCreateBuffer(context, CL_MEM_READ_WRITE | CL_MEM_COPY_HOST_PTR, v.size()*sizeof(cl_half), v.data(), &e);
      check(e, "buffer"); return memory;
    };
    cl_mem ma=allocate(a), mb=allocate(b), mc=allocate(c);
    cl_ulong offset = guard*sizeof(cl_half);
    check(clSetKernelArg(kernel,0,sizeof(ma),&ma), "weights");
    check(clSetKernelArg(kernel,1,sizeof(offset),&offset), "weight offset");
    check(clSetKernelArg(kernel,2,sizeof(mb),&mb), "input");
    check(clSetKernelArg(kernel,3,sizeof(offset),&offset), "input offset");
    check(clSetKernelArg(kernel,4,sizeof(mc),&mc), "output");
    check(clSetKernelArg(kernel,5,sizeof(offset),&offset), "output offset");
    check(clSetKernelArg(kernel,6,4096,nullptr), "local storage");
    std::vector<cl_uint> args = {s.co,s.ci,s.n,s.kw,s.kh,s.w,s.h,ow,oh,s.sx,s.sy,s.px,s.py,s.dx,s.dy,
        s.kw,s.kw*s.kh,s.kw*s.kh*s.ci,s.w,s.w*s.h,s.w*s.h*s.ci,ow,ow*oh,ow*oh*s.co};
    for (unsigned i=0;i<args.size();++i) check(clSetKernelArg(kernel,7+i,sizeof(cl_uint),&args[i]), "shape argument");
    const size_t local[2]={16,8}, global[2]={((s.co+63)/64)*16,((s.n*oh*ow+63)/64)*8};
    check(clEnqueueNDRangeKernel(queue,kernel,2,nullptr,global,local,0,nullptr,nullptr), "launch");
    check(clEnqueueReadBuffer(queue,mc,CL_TRUE,0,c.size()*sizeof(cl_half),c.data(),0,nullptr,nullptr), "read");
    for (unsigned n=0;n<s.n;++n) for (unsigned co=0;co<s.co;++co)
      for (unsigned y=0;y<oh;++y) for (unsigned x=0;x<ow;++x) {
        double expected=0;
        for (unsigned ci=0;ci<s.ci;++ci) for (unsigned ky=0;ky<s.kh;++ky) for (unsigned kx=0;kx<s.kw;++kx) {
          int iy=int(y*s.sy+ky*s.dy)-int(s.py), ix=int(x*s.sx+kx*s.dx)-int(s.px);
          if (iy<0 || ix<0 || iy>=int(s.h) || ix>=int(s.w)) continue;
          expected += double(cl_half_to_float(a[guard+((co*s.ci+ci)*s.kh+ky)*s.kw+kx])) *
                      cl_half_to_float(b[guard+((n*s.ci+ci)*s.h+iy)*s.w+ix]);
        }
        size_t i=((n*s.co+co)*oh+y)*ow+x;
        cl_half bits=cl_half_from_double(expected, CL_HALF_RTE);
        // Products/sums are exactly representable dyadic numbers; only the final half rounding remains.
        if (c[guard+i]!=bits && !((c[guard+i]&0x7fff)==0 && (bits&0x7fff)==0)) {
          std::fprintf(stderr,"case=%u index=%zu expected=%04x actual=%04x\n",cases,i,bits,c[guard+i]); return 1;
        }
      }
    for (size_t i=0;i<guard;++i) if(c[i]!=sentinel || c[c.size()-1-i]!=sentinel) return 1;
    total+=outputs; ++cases;
    std::printf("PASS: convolution case=%u outputs=%zu; exact half results and output guards\n",cases,outputs);
    clReleaseMemObject(ma);clReleaseMemObject(mb);clReleaseMemObject(mc);
  }
  clReleaseKernel(kernel);clReleaseProgram(program);clReleaseCommandQueue(queue);clReleaseContext(context);
  std::printf("PASS: %u convolution shapes, %zu checked outputs\n",cases,total);
}

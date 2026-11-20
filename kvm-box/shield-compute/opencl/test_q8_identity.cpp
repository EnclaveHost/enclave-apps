// A remembered weight identity must not outlive the bytes it describes.
//
// Runs the pinned MM kernel on the public fixture, then changes a weight buffer
// through each OpenCL write path and runs again. With its scales (or codes)
// zeroed the tensor is no longer the approved one, so the launch must take the
// original CPU kernel and produce exact zeros; a stale identity would return
// the approved tensor's product instead. Restoring the bytes must bring back
// the native fixed-point product. Passes on the queued route as well, which
// authenticates every launch.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <functional>
#include <iterator>
#include <stdexcept>
#include <string>
#include <vector>

static void check(cl_int rc,const char *stage) {
  if(rc) throw std::runtime_error(std::string(stage)+": "+std::to_string(rc));
}
static std::vector<unsigned char> read_file(const std::string &path) {
  std::ifstream stream(path,std::ios::binary);
  if(!stream) throw std::runtime_error("fixture unavailable");
  return {std::istreambuf_iterator<char>(stream),{}};
}

int main(int argc,char **argv) try {
  if(argc!=3) throw std::runtime_error("usage: test-q8-identity REFERENCE_DIR FIXTURE_DIR");
  const std::string reference=argv[1], fixture=argv[2];
  int k=0,n=0,m=0;
  std::ifstream dimensions(fixture+"/shape.txt"); dimensions>>k>>n>>m;
  if(k<=0 || k%32 || n<=0 || m<=0 || m>32) throw std::runtime_error("fixture shape");
  const auto quants=read_file(fixture+"/quants.bin"), scales=read_file(fixture+"/scales.bin");
  const auto raw_input=read_file(fixture+"/input.bin"), expected=read_file(fixture+"/native-reference.bin");
  if(quants.size()!=size_t(k)*n || scales.size()!=size_t(k)*n/16 ||
     raw_input.size()!=size_t(m)*k*4 || expected.size()!=size_t(m)*n*4) throw std::runtime_error("fixture size");
  constexpr float sentinel=-12345.0f;
  const std::vector<float> blank(size_t(m)*n,sentinel);
  const std::vector<unsigned char> zero_scales(scales.size(),0), zero_quants(quants.size(),0);

  cl_platform_id platform; cl_device_id device; cl_int error=0;
  check(clGetPlatformIDs(1,&platform,nullptr),"platform");
  check(clGetDeviceIDs(platform,CL_DEVICE_TYPE_GPU,1,&device,nullptr),"logical GPU");
  auto context=clCreateContext(nullptr,1,&device,nullptr,nullptr,&error); check(error,"context");
  auto queue=clCreateCommandQueueWithProperties(context,device,nullptr,&error); check(error,"queue");
  const std::string name="mul_mm_q8_0_f32_l4_lm";
  auto source=read_file(reference+"/"+name+".cl");
  const char *text=reinterpret_cast<const char *>(source.data()); size_t size=source.size();
  auto program=clCreateProgramWithSource(context,1,&text,&size,&error); check(error,"program");
  check(clBuildProgram(program,1,&device,"-cl-std=CL3.0",nullptr,nullptr),"build");
  auto kernel=clCreateKernel(program,("kernel_"+name).c_str(),&error); check(error,"kernel");
  const char *zero_source="kernel void zero_bytes(global uchar *p) { p[get_global_id(0)] = 0; }";
  auto zero_program=clCreateProgramWithSource(context,1,&zero_source,nullptr,&error); check(error,"zero program");
  check(clBuildProgram(zero_program,1,&device,"",nullptr,nullptr),"zero build");
  auto zero_kernel=clCreateKernel(zero_program,"zero_bytes",&error); check(error,"zero kernel");

  auto make=[&](const void *data,size_t bytes) {
    auto buffer=clCreateBuffer(context,CL_MEM_READ_WRITE|CL_MEM_COPY_HOST_PTR,bytes,const_cast<void *>(data),&error);
    check(error,"buffer"); return buffer;
  };
  cl_mem codes=make(quants.data(),quants.size()), scale=make(scales.data(),scales.size());
  cl_mem input=make(raw_input.data(),raw_input.size()), output=make(blank.data(),blank.size()*4);
  cl_mem zeros=make(zero_scales.data(),zero_scales.size());
  auto bind=[&] {
    check(clSetKernelArg(kernel,0,sizeof(cl_mem),&codes),"codes arg");
    check(clSetKernelArg(kernel,1,sizeof(cl_mem),&scale),"scales arg");
    check(clSetKernelArg(kernel,2,sizeof(cl_mem),&input),"input arg");
    check(clSetKernelArg(kernel,4,sizeof(cl_mem),&output),"output arg");
  };
  bind();
  std::array<uint64_t,19> args{};
  args[6]=k; args[7]=n; args[17]=args[18]=1;
  args[8]=1;args[9]=m;args[10]=1;args[11]=k;args[12]=k;args[13]=n;
  args[14]=uint64_t(k)*n;args[15]=uint64_t(k)*m;args[16]=uint64_t(n)*m;
  for(int i=3;i<19;++i) if(i!=4) {
    if(i==3 || i==5) {cl_ulong value=args[i];check(clSetKernelArg(kernel,i,sizeof(value),&value),"wide arg");}
    else {cl_int value=args[i];check(clSetKernelArg(kernel,i,sizeof(value),&value),"int arg");}
  }
  const std::array<size_t,3> global={size_t((n+63)/64*128),size_t((m+63)/64),1}, local={128,1,1};

  auto run=[&] {
    check(clEnqueueWriteBuffer(queue,output,CL_TRUE,0,blank.size()*4,blank.data(),0,nullptr,nullptr),"reset output");
    cl_event done=nullptr;
    check(clEnqueueNDRangeKernel(queue,kernel,3,nullptr,global.data(),local.data(),0,nullptr,&done),"enqueue");
    check(clWaitForEvents(1,&done),"completion"); check(clReleaseEvent(done),"release event");
    std::vector<float> result(blank.size());
    check(clEnqueueReadBuffer(queue,output,CL_TRUE,0,result.size()*4,result.data(),0,nullptr,nullptr),"read");
    return result;
  };
  auto approved=[&](const char *stage) {
    const auto result=run();
    if(std::memcmp(result.data(),expected.data(),expected.size()))
      throw std::runtime_error(std::string(stage)+": restored tensor did not give the native product");
  };
  auto unapproved=[&](const char *stage) {
    for(float value:run()) if(value!=0.0f)
      throw std::runtime_error(std::string(stage)+": changed tensor did not take the CPU kernel");
  };
  auto restore=[&] {
    check(clEnqueueWriteBuffer(queue,scale,CL_TRUE,0,scales.size(),scales.data(),0,nullptr,nullptr),"restore scales");
    check(clEnqueueWriteBuffer(queue,codes,CL_TRUE,0,quants.size(),quants.data(),0,nullptr,nullptr),"restore codes");
  };

  const std::vector<std::pair<const char *,std::function<void()>>> changes={
    {"write buffer",[&]{check(clEnqueueWriteBuffer(queue,scale,CL_TRUE,0,zero_scales.size(),zero_scales.data(),0,nullptr,nullptr),"write");}},
    {"write codes",[&]{check(clEnqueueWriteBuffer(queue,codes,CL_TRUE,0,zero_quants.size(),zero_quants.data(),0,nullptr,nullptr),"write codes");}},
    {"partial write",[&]{
      // Every scale but the first two bytes: an offset range inside the region.
      check(clEnqueueWriteBuffer(queue,scale,CL_TRUE,2,zero_scales.size()-2,zero_scales.data(),0,nullptr,nullptr),"partial");
      check(clEnqueueWriteBuffer(queue,scale,CL_TRUE,0,2,zero_scales.data(),0,nullptr,nullptr),"partial head");}},
    {"fill buffer",[&]{const unsigned char zero=0;
      check(clEnqueueFillBuffer(queue,scale,&zero,1,0,scales.size(),0,nullptr,nullptr),"fill");check(clFinish(queue),"finish");}},
    {"copy buffer",[&]{check(clEnqueueCopyBuffer(queue,zeros,scale,0,0,scales.size(),0,nullptr,nullptr),"copy");check(clFinish(queue),"finish");}},
    {"write rect",[&]{const size_t origin[3]={0,0,0}, region[3]={scales.size(),1,1};
      check(clEnqueueWriteBufferRect(queue,scale,CL_TRUE,origin,origin,region,0,0,0,0,zero_scales.data(),0,nullptr,nullptr),"rect");}},
    {"copy rect",[&]{const size_t origin[3]={0,0,0}, region[3]={scales.size(),1,1};
      check(clEnqueueCopyBufferRect(queue,zeros,scale,origin,origin,region,0,0,0,0,0,nullptr,nullptr),"copy rect");check(clFinish(queue),"finish");}},
    {"map for writing",[&]{
      void *p=clEnqueueMapBuffer(queue,scale,CL_TRUE,CL_MAP_WRITE,0,scales.size(),0,nullptr,nullptr,&error);check(error,"map");
      std::memset(p,0,scales.size());
      check(clEnqueueUnmapMemObject(queue,scale,p,0,nullptr,nullptr),"unmap");check(clFinish(queue),"finish");}},
    {"sub-buffer write",[&]{
      const cl_buffer_region region{0,scales.size()};
      auto sub=clCreateSubBuffer(scale,CL_MEM_READ_WRITE,CL_BUFFER_CREATE_TYPE_REGION,&region,&error);check(error,"sub-buffer");
      check(clEnqueueWriteBuffer(queue,sub,CL_TRUE,0,zero_scales.size(),zero_scales.data(),0,nullptr,nullptr),"sub write");
      check(clReleaseMemObject(sub),"release sub");}},
    {"another kernel",[&]{
      const size_t bytes=scales.size();
      check(clSetKernelArg(zero_kernel,0,sizeof(cl_mem),&scale),"zero arg");
      check(clEnqueueNDRangeKernel(queue,zero_kernel,1,nullptr,&bytes,nullptr,0,nullptr,nullptr),"zero launch");check(clFinish(queue),"finish");}},
    {"released and recreated",[&]{
      check(clReleaseMemObject(scale),"release scales");
      scale=make(zero_scales.data(),zero_scales.size());
      bind();}},
  };
  approved("first launch");
  approved("repeated launch");
  int approved_runs=2;
  for(const auto &[label,change]:changes) {
    change();
    unapproved(label);
    unapproved(label);  // and it stays unapproved while unchanged
    restore();
    approved(label);
    ++approved_runs;
  }
  for(auto buffer:{codes,scale,input,output,zeros}) check(clReleaseMemObject(buffer),"release buffer");
  check(clReleaseKernel(zero_kernel),"release kernel");check(clReleaseProgram(zero_program),"release program");
  check(clReleaseKernel(kernel),"release kernel");check(clReleaseProgram(program),"release program");
  check(clReleaseCommandQueue(queue),"release queue");check(clReleaseContext(context),"release context");
  std::printf("{\"passed\":true,\"changes\":%zu,\"approvedRuns\":%d}\n",changes.size(),approved_runs);
  return 0;
} catch(const std::exception &e) {std::fprintf(stderr,"%s\n",e.what());return 1;}

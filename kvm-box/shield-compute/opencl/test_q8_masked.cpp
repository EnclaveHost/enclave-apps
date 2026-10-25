// Actual pinned OpenCL Q8 kernels with a public model fixture. Compare against
// a separately executed native Shield result under the SAME explicit arithmetic.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <algorithm>
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fstream>
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
  if(argc!=4 && !(argc==5 && std::string(argv[4])=="--expect-failure")) throw std::runtime_error("usage: test-q8-masked REFERENCE_DIR FIXTURE_DIR mm|mv");
  const bool mm=std::string(argv[3])=="mm";
  if(!mm && std::string(argv[3])!="mv") throw std::runtime_error("variant");
  const std::string fixture=argv[2], reference=argv[1];
  int k=0,n=0,m=0;
  std::ifstream dimensions(fixture+"/shape.txt"); dimensions>>k>>n>>m;
  if(k<=0 || k%32 || n<=0 || n%8 || m<=0 || m>32 || int64_t(k)*n>16*1024*1024)
    throw std::runtime_error("fixture shape");
  auto quants=read_file(fixture+"/quants.bin"), scales=read_file(fixture+"/scales.bin");
  auto raw_input=read_file(fixture+"/input.bin"), expected=read_file(fixture+"/native-reference.bin");
  if(quants.size()!=size_t(k)*n || scales.size()!=size_t(k)*n/16 ||
     raw_input.size()!=size_t(m)*k*4 || expected.size()!=size_t(m)*n*4)
    throw std::runtime_error("fixture size");
  constexpr size_t guard=8; constexpr float sentinel=-12345.0f;
  std::vector<float> input(size_t(m)*k+2*guard,sentinel), output(size_t(m)*n+2*guard,sentinel);
  std::memcpy(input.data()+guard,raw_input.data(),raw_input.size());
  cl_platform_id platform; cl_device_id device; cl_int error=0;
  check(clGetPlatformIDs(1,&platform,nullptr),"platform");
  check(clGetDeviceIDs(platform,CL_DEVICE_TYPE_GPU,1,&device,nullptr),"logical GPU");
  auto context=clCreateContext(nullptr,1,&device,nullptr,nullptr,&error); check(error,"context");
  auto queue=clCreateCommandQueueWithProperties(context,device,nullptr,&error); check(error,"queue");
  const std::string name=mm?"mul_mm_q8_0_f32_l4_lm":"mul_mv_q8_0_f32_flat";
  auto source=read_file(reference+"/"+name+".cl");
  const char *text=reinterpret_cast<const char *>(source.data()); size_t size=source.size();
  auto program=clCreateProgramWithSource(context,1,&text,&size,&error); check(error,"program");
  check(clBuildProgram(program,1,&device,"-cl-std=CL3.0",nullptr,nullptr),"build");
  auto kernel=clCreateKernel(program,("kernel_"+name).c_str(),&error); check(error,"kernel");
  std::array<size_t,4> sizes={quants.size(),scales.size(),input.size()*4,output.size()*4};
  std::array<void *,4> data={quants.data(),scales.data(),input.data(),output.data()};
  std::array<cl_mem,4> buffers{};
  std::array<int,4> indices={0,1,2,4};
  for(size_t i=0;i<4;++i) {
    buffers[i]=clCreateBuffer(context,CL_MEM_READ_WRITE|CL_MEM_COPY_HOST_PTR,sizes[i],data[i],&error);
    check(error,"buffer"); check(clSetKernelArg(kernel,indices[i],sizeof(cl_mem),&buffers[i]),"buffer arg");
  }
  std::array<uint64_t,19> args{};
  args[3]=args[5]=guard*4; args[6]=k; args[7]=n; args[17]=args[18]=1;
  if(mm) {
    args[8]=1;args[9]=m;args[10]=1;args[11]=k;args[12]=k;args[13]=n;
    args[14]=uint64_t(k)*n;args[15]=uint64_t(k)*m;args[16]=uint64_t(n)*m;
  } else {
    args[8]=uint64_t(k)/32*34;args[9]=args[10]=args[8]*n;args[11]=1;
    args[12]=uint64_t(k)*4;args[13]=args[14]=uint64_t(m)*k*4;args[15]=n;args[16]=m;
  }
  for(int i=3;i<19;++i) if(i!=4) {
    const bool wide=i==3 || i==5 || (!mm && (i==8 || i==9 || i==10 || i==12 || i==13 || i==14));
    if(wide) {cl_ulong value=args[i];check(clSetKernelArg(kernel,i,sizeof(value),&value),"wide arg");}
    else {cl_int value=args[i];check(clSetKernelArg(kernel,i,sizeof(value),&value),"int arg");}
  }
  std::array<size_t,3> global=mm?std::array<size_t,3>{size_t((n+63)/64*128),size_t((m+63)/64),1}:
      std::array<size_t,3>{size_t((n+7)/8*16),size_t(m*2),1};
  std::array<size_t,3> local=mm?std::array<size_t,3>{128,1,1}:std::array<size_t,3>{16,2,1};
  cl_event done=nullptr;
  check(clEnqueueNDRangeKernel(queue,kernel,3,nullptr,global.data(),local.data(),0,nullptr,&done),"enqueue");
  if(argc==5) {
    const auto rc=clWaitForEvents(1,&done);
    cl_int status=0;
    check(clGetEventInfo(done,CL_EVENT_COMMAND_EXECUTION_STATUS,sizeof(status),&status,nullptr),"event status");
    if(rc!=CL_EXEC_STATUS_ERROR_FOR_EVENTS_IN_WAIT_LIST || status>=0)
      throw std::runtime_error("expected failed completion");
    auto fresh=clCreateCommandQueueWithProperties(context,device,nullptr,&error);check(error,"fresh queue");
    check(clEnqueueReadBuffer(fresh,buffers[3],CL_TRUE,0,sizes[3],output.data(),0,nullptr,nullptr),"failed output read");
    for(float value:output) if(value!=sentinel) throw std::runtime_error("failed command modified output");
    check(clReleaseEvent(done),"release failed event");done=nullptr;
    // A retired service must refuse another attempt; no silent CPU retry.
    check(clEnqueueNDRangeKernel(fresh,kernel,3,nullptr,global.data(),local.data(),0,nullptr,&done),"reenqueue");
    if(clWaitForEvents(1,&done)!=CL_EXEC_STATUS_ERROR_FOR_EVENTS_IN_WAIT_LIST)
      throw std::runtime_error("retired service accepted operation");
    auto reader=clCreateCommandQueueWithProperties(context,device,nullptr,&error);check(error,"reader queue");
    check(clEnqueueReadBuffer(reader,buffers[3],CL_TRUE,0,sizes[3],output.data(),0,nullptr,nullptr),"retired output read");
    for(float value:output) if(value!=sentinel) throw std::runtime_error("retired command modified output");
    std::printf("{\"passed\":true,\"variant\":\"%s\",\"failureLeavesOutputUntouched\":true,\"retiredRetryRefused\":true}\n",mm?"mm":"mv");
    check(clReleaseEvent(done),"release failed event");
    for(auto buffer:buffers)check(clReleaseMemObject(buffer),"release buffer");
    check(clReleaseCommandQueue(reader),"release reader");check(clReleaseCommandQueue(fresh),"release fresh");
    check(clReleaseKernel(kernel),"release kernel");check(clReleaseProgram(program),"release program");
    check(clReleaseCommandQueue(queue),"release queue");check(clReleaseContext(context),"release context");
    return 0;
  }
  check(clWaitForEvents(1,&done),"completion");
  check(clEnqueueReadBuffer(queue,buffers[3],CL_TRUE,0,sizes[3],output.data(),0,nullptr,nullptr),"read");
  if(std::memcmp(output.data()+guard,expected.data(),expected.size())) throw std::runtime_error("native arithmetic mismatch");
  for(size_t i=0;i<guard;++i) if(output[i]!=sentinel || output[output.size()-1-i]!=sentinel)
    throw std::runtime_error("guard modified");
  for(auto buffer:buffers) check(clReleaseMemObject(buffer),"release buffer");
  check(clReleaseEvent(done),"release event");check(clReleaseKernel(kernel),"release kernel");
  check(clReleaseProgram(program),"release program");check(clReleaseCommandQueue(queue),"release queue");
  check(clReleaseContext(context),"release context");
  std::printf("{\"passed\":true,\"variant\":\"%s\",\"outputs\":%d,\"matchesNativeFixedPoint\":true,\"guards\":true}\n",mm?"mm":"mv",m*n);
  return 0;
} catch(const std::exception &e) {std::fprintf(stderr,"%s\n",e.what());return 1;}

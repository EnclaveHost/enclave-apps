// Execute the exact pinned Q8 kernels; independent dyadic scalar reference.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <CL/cl_half.h>
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <iterator>
#include <string>
#include <vector>
#ifdef SHIELD_Q8_SNAPSHOT_TEST
#include <atomic>
#include <cstring>
static std::atomic<unsigned> snapshot_count{0};
extern "C" void shield_q8_snapshot_test(size_t k,size_t n,size_t m,const unsigned char *q,
                                        const unsigned char *d,const unsigned char *x) {
    for(size_t i=0;i<k*n;++i) {
        cl_char actual; std::memcpy(&actual,q+i,1);
        if(actual!=int((i*37)%255)-127) std::abort();
    }
    for(size_t i=0;i<k*n/32;++i) {
        cl_half actual; std::memcpy(&actual,d+i*2,2);
        if(actual!=cl_half_from_float((int(i%7)-3)/64.0f,CL_HALF_RTE)) std::abort();
    }
    for(size_t i=0;i<m*k;++i) {
        float actual; std::memcpy(&actual,x+i*4,4);
        if(actual!=(int((i*13)%17)-8)/8.0f) std::abort();
    }
    ++snapshot_count;
}
#endif

static void check(cl_int code, const char *phase) {
    if (code) { std::fprintf(stderr, "%s: %d\n", phase, code); std::exit(1); }
}
struct Shape { int k, n, m; };
int main(int argc, char **argv) {
    if (argc != 2) { std::fprintf(stderr, "usage: test-q8-kernels Q8_REFERENCE_DIRECTORY\n"); return 2; }
    cl_platform_id platform; cl_device_id device; cl_int error;
    check(clGetPlatformIDs(1, &platform, nullptr), "platform");
    check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr), "device");
    auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &error); check(error, "context");
    auto queue = clCreateCommandQueueWithProperties(context, device, nullptr, &error); check(error, "queue");
    for (bool fast : {false, true}) for (bool mm : {true, false}) {
        const char *file = mm ? "mul_mm_q8_0_f32_l4_lm.cl" : "mul_mv_q8_0_f32_flat.cl";
        const char *name = mm ? "kernel_mul_mm_q8_0_f32_l4_lm" : "kernel_mul_mv_q8_0_f32_flat";
        std::ifstream stream(std::string(argv[1])+"/"+file);
        if (!stream) return 2;
        std::string source((std::istreambuf_iterator<char>(stream)), {});
        const char *text = source.c_str(); size_t length = source.size();
        auto program = clCreateProgramWithSource(context, 1, &text, &length, &error); check(error, "program");
        const char *options = fast ? "-cl-std=CL3.0 -cl-mad-enable -cl-unsafe-math-optimizations -cl-finite-math-only -cl-fast-relaxed-math" : "-cl-std=CL3.0";
        error = clBuildProgram(program, 1, &device, options, nullptr, nullptr);
        if (error) {
            size_t bytes = 0; clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &bytes);
            std::vector<char> log(bytes+1, 0);
            clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, bytes, log.data(), nullptr);
            std::fprintf(stderr, "%s\n", log.data());
        }
        check(error, "build");
        auto kernel = clCreateKernel(program, name, &error); check(error, "kernel");
        std::vector<Shape> shapes = mm ? std::vector<Shape>{{32,8,1}, {96,73,67}, {896,128,32}}
                                       : std::vector<Shape>{{32,8,1}, {96,72,3}, {896,128,1}};
        for (Shape shape : shapes) {
            const int k=shape.k, n=shape.n, m=shape.m;
            constexpr size_t guard = 8;
            constexpr float sentinel = -12345.0f;
            std::vector<cl_char> quants(k*n);
            std::vector<cl_half> scales(k*n/32);
            std::vector<float> input(m*k+2*guard, sentinel), output(m*n+2*guard, sentinel);
            for (size_t i=0; i<quants.size(); ++i) quants[i] = int((i*37)%255)-127;
            for (size_t i=0; i<scales.size(); ++i)
                scales[i] = cl_half_from_float((int(i%7)-3)/64.0f, CL_HALF_RTE);
            for (int i=0; i<m*k; ++i) input[i+guard] = (int((i*13)%17)-8)/8.0f;
            auto allocate = [&](void *data, size_t bytes) {
                auto mem = clCreateBuffer(context, CL_MEM_READ_WRITE|CL_MEM_COPY_HOST_PTR, bytes, data, &error);
                check(error, "buffer"); return mem;
            };
            std::array<size_t,4> sizes = {quants.size(), scales.size()*2, input.size()*4, output.size()*4};
            std::array<cl_mem,4> buffers = {allocate(quants.data(),sizes[0]), allocate(scales.data(),sizes[1]),
                                          allocate(input.data(),sizes[2]), allocate(output.data(),sizes[3])};
#ifdef SHIELD_Q8_SNAPSHOT_TEST
            // The capture must wait for a delayed write. Starting from distinct
            // bytes makes an early snapshot fail even if later CPU execution
            // happens to wait and produces the expected final output.
            std::vector<float> stale(input.size(), sentinel);
            check(clEnqueueWriteBuffer(queue,buffers[2],CL_TRUE,0,sizes[2],stale.data(),0,nullptr,nullptr), "stale input");
            cl_event gate=clCreateUserEvent(context,&error); check(error,"user gate");
            cl_event written=nullptr;
            check(clEnqueueWriteBuffer(queue,buffers[2],CL_FALSE,0,sizes[2],input.data(),1,&gate,&written), "delayed input");
#endif
            std::array<int,4> indices = {0,1,2,4};
            for (size_t i=0; i<4; ++i) check(clSetKernelArg(kernel, indices[i], sizeof(cl_mem), &buffers[i]), "buffer arg");
            std::array<uint64_t,19> args{};
            args[3]=args[5]=guard*4; args[6]=k; args[7]=n; args[17]=args[18]=1;
            if (mm) {
                args[8]=1; args[9]=m; args[10]=1; args[11]=k; args[12]=k;
                args[13]=n; args[14]=k*n; args[15]=k*m; args[16]=n*m;
            } else {
                args[8]=k/32*34; args[9]=args[10]=args[8]*n; args[11]=1;
                args[12]=k*4; args[13]=args[14]=m*k*4; args[15]=n; args[16]=m;
            }
            for (int i=3; i<19; ++i) if (i != 4) {
                bool wide = i==3 || i==5 || (!mm && (i==8 || i==9 || i==10 || i==12 || i==13 || i==14));
                if (wide) { cl_ulong v=args[i]; check(clSetKernelArg(kernel,i,sizeof(v),&v), "ulong arg"); }
                else { cl_int v=args[i]; check(clSetKernelArg(kernel,i,sizeof(v),&v), "int arg"); }
            }
            std::array<size_t,3> grid = mm ? std::array<size_t,3>{size_t((n+63)/64*128),size_t((m+63)/64),1}
                                          : std::array<size_t,3>{size_t((n+7)/8*16),size_t(m*2),1};
            std::array<size_t,3> local = mm ? std::array<size_t,3>{128,1,1} : std::array<size_t,3>{16,2,1};
#ifdef SHIELD_Q8_SNAPSHOT_TEST
            // Cloned kernels must inherit analysis and recorded arguments.
            auto launch_kernel=clCloneKernel(kernel,&error); check(error,"application clone");
            cl_event done=nullptr;
            check(clEnqueueNDRangeKernel(queue,launch_kernel,3,nullptr,grid.data(),local.data(),1,&written,&done), "launch");
            cl_ulong changed_offset=0;
            check(clSetKernelArg(launch_kernel,5,sizeof(changed_offset),&changed_offset), "mutate original kernel after enqueue");
            check(clReleaseKernel(launch_kernel),"early release kernel");
            check(clSetUserEventStatus(gate,CL_COMPLETE),"open gate");
            clReleaseEvent(gate); clReleaseEvent(written);
            check(clWaitForEvents(1,&done),"capture completion");
            cl_command_type command=0;
            check(clGetEventInfo(done,CL_EVENT_COMMAND_TYPE,sizeof(command),&command,nullptr),"event type");
            if(command!=CL_COMMAND_NDRANGE_KERNEL) return 1;
            clReleaseEvent(done);
#else
            check(clEnqueueNDRangeKernel(queue,kernel,3,nullptr,grid.data(),local.data(),0,nullptr,nullptr), "launch");
#endif
            check(clEnqueueReadBuffer(queue,buffers[3],CL_TRUE,0,sizes[3],output.data(),0,nullptr,nullptr), "read");
            for (int row=0; row<m; ++row) for (int col=0; col<n; ++col) {
                double expected=0;
                for (int j=0; j<k; ++j)
                    expected += double(quants[col*k+j])*cl_half_to_float(scales[(col*k+j)/32])*input[guard+row*k+j];
                if (output[guard+row*n+col] != expected) {
                    std::fprintf(stderr,"%s fast=%d k=%d n=%d m=%d row=%d col=%d expected=%g actual=%g\n",
                                 name,fast,k,n,m,row,col,expected,output[guard+row*n+col]); return 1;
                }
            }
            for (size_t i=0; i<guard; ++i)
                if (output[i]!=sentinel || output[output.size()-1-i]!=sentinel) return 1;
            // Preserve the actual clSetKernelArg/launch inputs for Python gate replay.
            std::printf("{\"variant\":\"%s\",\"fast\":%s,\"outputs\":%d,\"exact\":true,\"guards\":true,\"scalars\":{", mm?"mm":"mv",fast?"true":"false",m*n);
            bool first=true;
            for(int i=3;i<19;++i) if(i!=4) {
                std::printf("%s\"%d\":%llu",first?"":",",i,(unsigned long long)args[i]); first=false;
            }
            std::printf("},\"bufferBytes\":[%zu,%zu,%zu,%zu],\"global\":[%zu,%zu,%zu],\"local\":[%zu,%zu,%zu]}\n",
                        sizes[0],sizes[1],sizes[2],sizes[3],grid[0],grid[1],grid[2],local[0],local[1],local[2]);
            for(auto mem:buffers) clReleaseMemObject(mem);
        }
        clReleaseKernel(kernel); clReleaseProgram(program);
    }
    clReleaseCommandQueue(queue); clReleaseContext(context);
#ifdef SHIELD_Q8_SNAPSHOT_TEST
    if(snapshot_count!=12) { std::fprintf(stderr,"snapshot count: %u\n",snapshot_count.load()); return 1; }
#endif
}

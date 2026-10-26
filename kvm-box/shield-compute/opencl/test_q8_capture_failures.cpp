// Capture-path failure, lifetime and ordering tests for the experimental Q8
// frontend. Exact pinned MM kernel; independent dyadic double reference.
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <CL/cl_half.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

constexpr size_t guard = 8;
constexpr float sentinel = -12345.0f;

// Public fixtures. The snapshot hook identifies which one it received by
// exact byte comparison, so a capture of the wrong state counts as unmatched
// or credits another fixture.
struct Fixture {
    const char *label; int k, n, m;
    std::vector<cl_char> quants; std::vector<cl_half> scales; std::vector<float> input;
    std::atomic<unsigned> hits{0};
};
static std::mutex registry_mutex;
static std::vector<std::shared_ptr<Fixture>> registry;
static std::atomic<unsigned> callbacks{0}, unmatched{0};

extern "C" __attribute__((visibility("default"), used))
void shield_q8_snapshot_test(size_t k,size_t n,size_t m,const unsigned char *q,
                             const unsigned char *d,const unsigned char *x) {
    {
        std::lock_guard lock(registry_mutex);
        bool matched=false;
        for (auto &f : registry)
            if (size_t(f->k)==k && size_t(f->n)==n && size_t(f->m)==m &&
                !std::memcmp(q,f->quants.data(),k*n) && !std::memcmp(d,f->scales.data(),k*n/32*2) &&
                !std::memcmp(x,f->input.data(),m*k*4)) { ++f->hits; matched=true; break; }
        if (!matched) ++unmatched;
    }
    ++callbacks;
}

static void check(cl_int code, const char *phase) {
    if (code) { std::fprintf(stderr, "%s: %d\n", phase, code); std::exit(1); }
}
static std::shared_ptr<Fixture> make_fixture(const char *label, int k, int n, int m, int seed, bool record=true) {
    auto f=std::make_shared<Fixture>();
    f->label=label; f->k=k; f->n=n; f->m=m;
    f->quants.resize(k*n); f->scales.resize(k*n/32); f->input.resize(m*k);
    for (size_t i=0; i<f->quants.size(); ++i) f->quants[i] = int((i*37+seed*11)%255)-127;
    for (size_t i=0; i<f->scales.size(); ++i)
        f->scales[i] = cl_half_from_float((int((i+seed)%7)-3)/64.0f, CL_HALF_RTE);
    for (size_t i=0; i<f->input.size(); ++i) f->input[i] = (int((i*13+seed*5)%17)-8)/8.0f;
    if (record) { std::lock_guard lock(registry_mutex); registry.push_back(f); }
    return f;
}
static std::vector<float> padded_input(const Fixture &f) {
    std::vector<float> input(f.m*f.k+2*guard, sentinel);
    std::copy(f.input.begin(), f.input.end(), input.begin()+guard);
    return input;
}

struct Buffers { std::array<cl_mem,4> mem{}; std::array<size_t,4> bytes{}; };
static Buffers make_buffers(cl_context context, const Fixture &f, bool stale_input=false) {
    Buffers b;
    std::vector<cl_char> quants=f.quants; std::vector<cl_half> scales=f.scales;
    std::vector<float> input = stale_input ? std::vector<float>(f.m*f.k+2*guard, sentinel) : padded_input(f);
    std::vector<float> output(f.m*f.n+2*guard, sentinel);
    b.bytes = {quants.size(), scales.size()*2, input.size()*4, output.size()*4};
    std::array<void*,4> data = {quants.data(), scales.data(), input.data(), output.data()};
    for (size_t i=0; i<4; ++i) {
        cl_int error;
        b.mem[i] = clCreateBuffer(context, CL_MEM_READ_WRITE|CL_MEM_COPY_HOST_PTR, b.bytes[i], data[i], &error);
        check(error, "buffer");
    }
    return b;
}
static void release_buffers(Buffers &b) {
    for (auto &mem : b.mem) if (mem) { check(clReleaseMemObject(mem), "release buffer"); mem=nullptr; }
}
// The 19-argument MM ABI exactly as in test_q8_kernels.cpp.
static void set_args(cl_kernel kernel, const Buffers &b, const Fixture &f) {
    std::array<int,4> indices = {0,1,2,4};
    for (size_t i=0; i<4; ++i) check(clSetKernelArg(kernel, indices[i], sizeof(cl_mem), &b.mem[i]), "buffer arg");
    const int k=f.k, n=f.n, m=f.m;
    std::array<uint64_t,19> args{};
    args[3]=args[5]=guard*4; args[6]=k; args[7]=n; args[17]=args[18]=1;
    args[8]=1; args[9]=m; args[10]=1; args[11]=k; args[12]=k;
    args[13]=n; args[14]=k*n; args[15]=k*m; args[16]=n*m;
    for (int i=3; i<19; ++i) if (i != 4) {
        if (i==3 || i==5) { cl_ulong v=args[i]; check(clSetKernelArg(kernel,i,sizeof(v),&v), "ulong arg"); }
        else { cl_int v=args[i]; check(clSetKernelArg(kernel,i,sizeof(v),&v), "int arg"); }
    }
}
static void launch(cl_command_queue queue, cl_kernel kernel, const Fixture &f,
                   cl_uint count, const cl_event *wait, cl_event *done) {
    std::array<size_t,3> grid = {size_t((f.n+63)/64*128), size_t((f.m+63)/64), 1};
    std::array<size_t,3> local = {128,1,1};
    check(clEnqueueNDRangeKernel(queue,kernel,3,nullptr,grid.data(),local.data(),count,wait,done), "launch");
}
static cl_int status_of(cl_event event) {
    cl_int status=0;
    check(clGetEventInfo(event,CL_EVENT_COMMAND_EXECUTION_STATUS,sizeof(status),&status,nullptr), "event status");
    return status;
}
// Bounded poll; the parent also applies an outer timeout.
static cl_int wait_terminal(cl_event event, const char *what) {
    auto deadline = std::chrono::steady_clock::now()+std::chrono::seconds(60);
    for (;;) {
        cl_int status=status_of(event);
        if (status<=CL_COMPLETE) return status;
        if (std::chrono::steady_clock::now()>deadline) {
            std::fprintf(stderr, "%s: timeout, status=%d\n", what, status); std::exit(1);
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
}
static std::vector<float> read_output(cl_command_queue queue, const Buffers &b) {
    std::vector<float> output(b.bytes[3]/4);
    check(clEnqueueReadBuffer(queue,b.mem[3],CL_TRUE,0,b.bytes[3],output.data(),0,nullptr,nullptr), "read");
    return output;
}
static bool all_sentinel(const std::vector<float> &output) {
    for (float value : output) if (value != sentinel) return false;
    return true;
}
static bool exact(const Fixture &f, const std::vector<float> &output) {
    const int k=f.k, n=f.n, m=f.m;
    for (int row=0; row<m; ++row) for (int col=0; col<n; ++col) {
        double expected=0;
        for (int j=0; j<k; ++j)
            expected += double(f.quants[col*k+j])*cl_half_to_float(f.scales[(col*k+j)/32])*f.input[row*k+j];
        if (output[guard+row*n+col] != expected) {
            std::fprintf(stderr,"%s k=%d n=%d m=%d row=%d col=%d expected=%g actual=%g\n",
                         f.label,k,n,m,row,col,expected,output[guard+row*n+col]);
            return false;
        }
    }
    for (size_t i=0; i<guard; ++i)
        if (output[i]!=sentinel || output[output.size()-1-i]!=sentinel) {
            std::fprintf(stderr,"%s guard overwritten\n",f.label); return false;
        }
    return true;
}

struct Env { cl_context context; cl_device_id device; cl_program program; };
static const char *kernel_name = "kernel_mul_mm_q8_0_f32_l4_lm";
static cl_kernel make_kernel(const Env &env) {
    cl_int error; auto kernel=clCreateKernel(env.program,kernel_name,&error); check(error,"kernel"); return kernel;
}
static cl_command_queue make_queue(const Env &env, cl_command_queue_properties properties=0) {
    cl_int error;
    cl_queue_properties list[] = {CL_QUEUE_PROPERTIES, static_cast<cl_queue_properties>(properties), 0};
    auto queue=clCreateCommandQueueWithProperties(env.context,env.device,properties?list:nullptr,&error);
    check(error,"queue"); return queue;
}
static cl_event make_gate(const Env &env) {
    cl_int error; auto gate=clCreateUserEvent(env.context,&error); check(error,"user gate"); return gate;
}

// 1. A failed user-event dependency must fail the command, never snapshot,
// and never write output. A control launch on a fresh queue proves the same
// kernel/argument state is otherwise captured.
static bool case_failed_dependency(const Env &env) {
    auto f=make_fixture("failed-dependency",32,8,2,1);
    auto queue=make_queue(env); auto kernel=make_kernel(env);
    auto buffers=make_buffers(env.context,*f);
    set_args(kernel,buffers,*f);
    auto gate=make_gate(env);
    const unsigned before=callbacks;
    cl_event done=nullptr;
    launch(queue,kernel,*f,1,&gate,&done);
    check(clFlush(queue),"flush");
    const cl_int pending=status_of(done);
    check(clSetUserEventStatus(gate,-1),"fail gate");
    const cl_int failed=wait_terminal(done,"failed dependency");
    // The job thread finishes before the completion event; this settle only
    // guards against a late snapshot from an unexpected path.
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
    const unsigned after_failure=callbacks-before, failure_hits=f->hits;
    check(clReleaseEvent(done),"release done"); check(clReleaseEvent(gate),"release gate");

    // Some runtimes poison an in-order queue after a failed command.
    auto fresh=make_queue(env);
    const bool untouched=all_sentinel(read_output(fresh,buffers));
    cl_event control=nullptr;
    launch(fresh,kernel,*f,0,nullptr,&control);
    check(clFlush(fresh),"flush");
    const cl_int control_status=wait_terminal(control,"control");
    check(clReleaseEvent(control),"release control");
    const bool control_exact=control_status==CL_COMPLETE && exact(*f,read_output(fresh,buffers));
    const unsigned control_hits=f->hits;

    const bool ok = pending>CL_COMPLETE && failed<0 && after_failure==0 && failure_hits==0 && untouched &&
                    control_status==CL_COMPLETE && control_exact && control_hits==1;
    std::printf("case1 failed-dependency: %s pending=%d done=%d callbacks_after_failure=%u output=%s "
                "control=%d control_exact=%d control_hits=%u\n", ok?"PASS":"FAIL", pending, failed,
                after_failure, untouched?"sentinel":"MODIFIED", control_status, control_exact, control_hits);
    release_buffers(buffers);
    check(clReleaseKernel(kernel),"release kernel");
    check(clReleaseCommandQueue(fresh),"release fresh queue"); check(clReleaseCommandQueue(queue),"release queue");
    return ok;
}

// 2. Application releases inputs and kernel while the capture is gated. The
// job must retain them. Decoy allocations of identical sizes encourage reuse
// of freed storage, which would surface as unmatched snapshot bytes.
static bool case_input_lifetime(const Env &env) {
    auto f=make_fixture("input-lifetime",96,16,3,2);
    auto queue=make_queue(env); auto kernel=make_kernel(env);
    auto buffers=make_buffers(env.context,*f);
    set_args(kernel,buffers,*f);
    auto gate=make_gate(env);
    const unsigned before=callbacks;
    cl_event done=nullptr;
    launch(queue,kernel,*f,1,&gate,&done);
    check(clFlush(queue),"flush");
    check(clReleaseKernel(kernel),"early release kernel");
    for (size_t i=0; i<3; ++i) { check(clReleaseMemObject(buffers.mem[i]),"early release input"); buffers.mem[i]=nullptr; }
    auto decoy_fixture=make_fixture("decoy",96,16,3,77,false);
    auto decoys=make_buffers(env.context,*decoy_fixture);
    const cl_int pending=status_of(done);
    const unsigned early=callbacks-before;
    check(clSetUserEventStatus(gate,CL_COMPLETE),"open gate");
    const cl_int status=wait_terminal(done,"lifetime");
    check(clReleaseEvent(done),"release done"); check(clReleaseEvent(gate),"release gate");
    const bool output_exact=status==CL_COMPLETE && exact(*f,read_output(queue,buffers));
    const unsigned hits=f->hits, total=callbacks-before;
    const bool ok = pending>CL_COMPLETE && early==0 && status==CL_COMPLETE && output_exact && hits==1 && total==1;
    std::printf("case2 input-lifetime: %s pending=%d done=%d early_callbacks=%u hits=%u callbacks=%u exact=%d\n",
                ok?"PASS":"FAIL", pending, status, early, hits, total, output_exact);
    release_buffers(decoys); release_buffers(buffers);
    check(clReleaseCommandQueue(queue),"release queue");
    return ok;
}

// 3. Two gated launches of one kernel object with different argument states,
// followed by mutation to decoy buffers/scalars before the gate opens. All
// clSetKernelArg calls are on this thread.
static bool case_two_launches(const Env &env) {
    auto fa=make_fixture("launch-a",32,8,2,3), fb=make_fixture("launch-b",32,8,2,4);
    auto decoy_fixture=make_fixture("decoy",32,8,2,78,false);
    auto queue=make_queue(env); auto kernel=make_kernel(env);
    auto ba=make_buffers(env.context,*fa), bb=make_buffers(env.context,*fb);
    auto decoys=make_buffers(env.context,*decoy_fixture);
    auto gate=make_gate(env);
    const unsigned before=callbacks;
    std::array<cl_event,2> done{};
    set_args(kernel,ba,*fa); launch(queue,kernel,*fa,1,&gate,&done[0]);
    set_args(kernel,bb,*fb); launch(queue,kernel,*fb,1,&gate,&done[1]);
    set_args(kernel,decoys,*decoy_fixture);
    cl_ulong zero_offset=0; cl_int other_m=1;
    check(clSetKernelArg(kernel,5,sizeof(zero_offset),&zero_offset),"mutate offset");
    check(clSetKernelArg(kernel,9,sizeof(other_m),&other_m),"mutate m");
    check(clFlush(queue),"flush");
    const cl_int pending_a=status_of(done[0]), pending_b=status_of(done[1]);
    const unsigned early=callbacks-before;
    check(clSetUserEventStatus(gate,CL_COMPLETE),"open gate");
    const cl_int status_a=wait_terminal(done[0],"launch a"), status_b=wait_terminal(done[1],"launch b");
    for (auto e : done) check(clReleaseEvent(e),"release done");
    check(clReleaseEvent(gate),"release gate");
    const bool exact_a=status_a==CL_COMPLETE && exact(*fa,read_output(queue,ba));
    const bool exact_b=status_b==CL_COMPLETE && exact(*fb,read_output(queue,bb));
    const bool decoy_untouched=all_sentinel(read_output(queue,decoys));
    const unsigned hits_a=fa->hits, hits_b=fb->hits, total=callbacks-before;
    const bool ok = pending_a>CL_COMPLETE && pending_b>CL_COMPLETE && early==0 &&
                    exact_a && exact_b && decoy_untouched && hits_a==1 && hits_b==1 && total==2;
    std::printf("case3 two-launches: %s pending=%d/%d done=%d/%d early_callbacks=%u hits=%u/%u callbacks=%u "
                "exact=%d/%d decoy_output=%s\n", ok?"PASS":"FAIL", pending_a, pending_b, status_a, status_b,
                early, hits_a, hits_b, total, exact_a, exact_b, decoy_untouched?"sentinel":"MODIFIED");
    release_buffers(ba); release_buffers(bb); release_buffers(decoys);
    check(clReleaseKernel(kernel),"release kernel"); check(clReleaseCommandQueue(queue),"release queue");
    return ok;
}

// 4. Out-of-order queue: gated input write -> launch -> non-blocking read,
// ordered only by explicit events. Input starts stale, so a snapshot taken
// before the write completes is unmatched.
static bool case_out_of_order(const Env &env, bool &skipped) {
    cl_command_queue_properties supported=0;
    check(clGetDeviceInfo(env.device,CL_DEVICE_QUEUE_ON_HOST_PROPERTIES,sizeof(supported),&supported,nullptr),
          "queue properties");
    if (!(supported & CL_QUEUE_OUT_OF_ORDER_EXEC_MODE_ENABLE)) {
        skipped=true;
        std::printf("case4 out-of-order: SKIP device queue properties=0x%llx\n",(unsigned long long)supported);
        return true;
    }
    auto f=make_fixture("out-of-order",96,16,3,5);
    auto queue=make_queue(env,CL_QUEUE_OUT_OF_ORDER_EXEC_MODE_ENABLE); auto kernel=make_kernel(env);
    auto buffers=make_buffers(env.context,*f,true);
    set_args(kernel,buffers,*f);
    auto gate=make_gate(env);
    const unsigned before=callbacks;
    auto input=padded_input(*f);
    std::vector<float> output(buffers.bytes[3]/4, 0.0f);
    cl_event written=nullptr, done=nullptr, read=nullptr;
    check(clEnqueueWriteBuffer(queue,buffers.mem[2],CL_FALSE,0,buffers.bytes[2],input.data(),1,&gate,&written),
          "delayed input");
    launch(queue,kernel,*f,1,&written,&done);
    check(clEnqueueReadBuffer(queue,buffers.mem[3],CL_FALSE,0,buffers.bytes[3],output.data(),1,&done,&read),
          "dependent read");
    check(clFlush(queue),"flush");
    const cl_int pending=status_of(done), pending_read=status_of(read);
    const unsigned early=callbacks-before;
    check(clSetUserEventStatus(gate,CL_COMPLETE),"open gate");
    const cl_int read_status=wait_terminal(read,"ooo read");
    const cl_int write_status=status_of(written), done_status=status_of(done);
    for (auto e : {written,done,read,gate}) check(clReleaseEvent(e),"release event");
    const bool output_exact=read_status==CL_COMPLETE && done_status==CL_COMPLETE && exact(*f,output);
    const unsigned hits=f->hits, total=callbacks-before;
    const bool ok = pending>CL_COMPLETE && pending_read>CL_COMPLETE && early==0 && write_status==CL_COMPLETE &&
                    output_exact && hits==1 && total==1;
    std::printf("case4 out-of-order: %s pending=%d/%d write=%d done=%d read=%d early_callbacks=%u hits=%u "
                "callbacks=%u exact=%d\n", ok?"PASS":"FAIL", pending, pending_read, write_status, done_status,
                read_status, early, hits, total, output_exact);
    check(clFinish(queue),"finish");
    release_buffers(buffers);
    check(clReleaseKernel(kernel),"release kernel"); check(clReleaseCommandQueue(queue),"release queue");
    return ok;
}

// More than the old 16-job limit may be enqueued behind a user event without
// blocking the enqueue thread or changing later launches to a different route.
static bool case_pending_pressure(const Env &env) {
    auto f=make_fixture("pending-pressure",96,16,3,6);
    auto queue=make_queue(env);auto kernel=make_kernel(env);auto buffers=make_buffers(env.context,*f);
    set_args(kernel,buffers,*f);
    auto gate=make_gate(env);
    const unsigned before=callbacks;
    std::array<cl_event,40> events{};
    for(auto &event:events)launch(queue,kernel,*f,1,&gate,&event);
    check(clFlush(queue),"flush pending jobs");
    const unsigned early=callbacks-before;
    check(clSetUserEventStatus(gate,CL_COMPLETE),"open pending gate");
    bool completed=true;
    for(auto event:events)completed=wait_terminal(event,"pending pressure")==CL_COMPLETE && completed;
    const bool output_exact=completed && exact(*f,read_output(queue,buffers));
    const unsigned count=callbacks-before,hits=f->hits;
    const bool ok=early==0 && completed && output_exact && count==40 && hits==40;
    std::printf("case5 pending-pressure: %s callbacks=%u hits=%u early=%u exact=%d\n",
                ok?"PASS":"FAIL",count,hits,early,output_exact);
    for(auto event:events)check(clReleaseEvent(event),"release pending event");
    check(clReleaseEvent(gate),"release gate");release_buffers(buffers);
    check(clReleaseKernel(kernel),"release kernel");check(clReleaseCommandQueue(queue),"release queue");
    return ok;
}

// A deliberately unusable log target must fail before the original CPU
// kernel touches output. Run separately with /evidence/q8-capture.jsonl bound
// to a directory; the public-fixture hook proves snapshot capture was reached.
static bool case_log_failure(const Env &env) {
    auto f=make_fixture("log-failure",32,8,2,6);
    auto queue=make_queue(env); auto fresh=make_queue(env); auto kernel=make_kernel(env);
    auto buffers=make_buffers(env.context,*f);
    set_args(kernel,buffers,*f);
    cl_event done=nullptr;
    launch(queue,kernel,*f,0,nullptr,&done);
    check(clFlush(queue),"flush");
    const auto status=wait_terminal(done,"log failure");
    const bool untouched=all_sentinel(read_output(fresh,buffers));
    const bool ok=status<0 && untouched && f->hits==1 && callbacks==1 && unmatched==0;
    std::printf("log-failure: %s done=%d callbacks=%u unmatched=%u output=%s\n",
                ok?"PASS":"FAIL",status,callbacks.load(),unmatched.load(),untouched?"sentinel":"MODIFIED");
    check(clReleaseEvent(done),"release done");
    release_buffers(buffers);
    check(clReleaseKernel(kernel),"release kernel");
    check(clReleaseCommandQueue(fresh),"release fresh"); check(clReleaseCommandQueue(queue),"release queue");
    return ok;
}

int main(int argc, char **argv) {
    if (argc != 2 && !(argc == 3 && std::string(argv[2]) == "--log-failure")) {
        std::fprintf(stderr, "usage: test-q8-capture-failures MM_SOURCE.cl|Q8_REFERENCE_DIRECTORY [--log-failure]\n"); return 2;
    }
    std::string path = argv[1];
    if (path.size()<3 || path.compare(path.size()-3,3,".cl")) path += "/mul_mm_q8_0_f32_l4_lm.cl";
    std::ifstream stream(path);
    if (!stream) { std::fprintf(stderr, "cannot open %s\n", path.c_str()); return 2; }
    std::string source((std::istreambuf_iterator<char>(stream)), {});

    cl_platform_id platform; cl_device_id device; cl_int error;
    check(clGetPlatformIDs(1, &platform, nullptr), "platform");
    check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr), "device");
    auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &error); check(error, "context");
    const char *text = source.c_str(); size_t length = source.size();
    auto program = clCreateProgramWithSource(context, 1, &text, &length, &error); check(error, "program");
    error = clBuildProgram(program, 1, &device, "-cl-std=CL3.0", nullptr, nullptr);
    if (error) {
        size_t bytes = 0; clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &bytes);
        std::vector<char> log(bytes+1, 0);
        clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, bytes, log.data(), nullptr);
        std::fprintf(stderr, "%s\n", log.data());
    }
    check(error, "build");

    Env env{context, device, program};
    if (argc == 3) {
        const bool ok=case_log_failure(env);
        check(clReleaseProgram(program),"release program"); check(clReleaseContext(context),"release context");
        return ok?0:1;
    }
    bool skipped=false;
    int failures=0;
    failures += !case_failed_dependency(env);
    failures += !case_input_lifetime(env);
    failures += !case_two_launches(env);
    failures += !case_out_of_order(env, skipped);
    failures += !case_pending_pressure(env);

    check(clReleaseProgram(program),"release program"); check(clReleaseContext(context),"release context");
    // Late snapshots from any detached job would show up here.
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
    const unsigned expected = 44 + (skipped ? 0 : 1);
    const bool totals = callbacks==expected && unmatched==0;
    if (!totals) ++failures;
    std::printf("summary: %s callbacks=%u expected=%u unmatched=%u failures=%d ooo=%s\n", failures?"FAIL":"PASS",
                callbacks.load(), expected, unmatched.load(), failures, skipped?"skipped":"run");
    return failures ? 1 : 0;
}

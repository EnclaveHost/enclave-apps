// Experimental only: included inside the frontend namespace with
// SHIELD_Q8_CAPTURE. Captures inputs after queue dependencies, authenticates
// public weights, then executes the original kernel on the CPU ICD.
// Without SHIELD_Q8_MASKED it never invokes Shield or a physical GPU.
// The separate masked build requires explicit arithmetic opt-in for that route.
#ifndef SHIELD_Q8_CAPTURE_LOG
#define SHIELD_Q8_CAPTURE_LOG "/evidence/q8-capture.jsonl"
#endif
#include "q8_admission.inc"
std::atomic<size_t> q8_pending_jobs{0};
// Optional aggregate stage timing (SHIELD_OPENCL_TIMING=1): durations and
// counts only, printed once at exit. Never tensor names, shapes or values.
struct Q8Timing {
  enum Stage { Enqueue, Wait, Snapshot, Authenticate, Service, Write, Cpu, Count };
  std::array<std::atomic<uint64_t>, Count> ns{}, calls{};
  const bool enabled = std::getenv("SHIELD_OPENCL_TIMING") != nullptr;
  // SHIELD_OPENCL_TIMING=N leaves the first N launches (load, warm-up) out.
  const uint64_t skip = enabled ? std::strtoull(std::getenv("SHIELD_OPENCL_TIMING"), nullptr, 10) : 0;
  std::atomic<uint64_t> launches{0}, first{0}, last{0};
  static uint64_t now() {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
  }
  void launch() {
    if (!enabled) return;
    if (launches.fetch_add(1) == skip) {
      first = now();
      timing_steady = true;
    }
  }
  void add(Stage stage, uint64_t start) {
    if (!enabled || launches.load() <= skip) return;
    last = now();
    ns[stage] += last - start; calls[stage]++;
  }
  ~Q8Timing() {
    if (!enabled) return;
    static const char *names[Count] = {"enqueue","wait","snapshot","authenticate","service","write","cpu"};
    for (int i = 0; i < Count; ++i)
      std::fprintf(stderr, "[shield-opencl-timing] %s calls=%llu ms=%.1f us/call=%.1f\n", names[i],
                   (unsigned long long)calls[i].load(), ns[i].load() / 1e6,
                   calls[i].load() ? ns[i].load() / 1e3 / calls[i].load() : 0.0);
    std::fprintf(stderr, "[shield-opencl-timing] span ms=%.1f\n", (last.load() - first.load()) / 1e6);
  }
};
Q8Timing q8_timing;
std::mutex q8_log_mutex;

struct Q8Hash {
  std::unique_ptr<EVP_MD_CTX, decltype(&EVP_MD_CTX_free)> ctx{EVP_MD_CTX_new(), EVP_MD_CTX_free};
  Q8Hash() {
    if (!ctx || EVP_DigestInit_ex(ctx.get(), EVP_sha256(), nullptr) != 1)
      throw std::runtime_error("hash init");
  }
  void update(const void *data, size_t size) {
    if (EVP_DigestUpdate(ctx.get(), data, size) != 1) throw std::runtime_error("hash update");
  }
  std::string finish() {
    unsigned char bytes[EVP_MAX_MD_SIZE]; unsigned length = 0;
    if (EVP_DigestFinal_ex(ctx.get(), bytes, &length) != 1 || length != 32)
      throw std::runtime_error("hash finish");
    constexpr char hex[]="0123456789abcdef";
    std::string output;
    for (unsigned i=0;i<length;++i) { output.push_back(hex[bytes[i]>>4]); output.push_back(hex[bytes[i]&15]); }
    return output;
  }
};
std::string q8_read_file(const std::string &path, size_t maximum) {
  std::ifstream input(path, std::ios::binary);
  if (!input) throw std::runtime_error("policy file unavailable");
  std::string value(maximum+1, '\0');
  input.read(value.data(), value.size());
  value.resize(size_t(input.gcount()));
  if (input.bad() || value.size()>maximum) throw std::runtime_error("policy file limit");
  return value;
}
std::string q8_sha(const std::string &value) {
  Q8Hash hash; hash.update(value.data(), value.size()); return hash.finish();
}
struct Q8PublicTensor {
  int type;  // GGML tensor type: 8 Q8_0, 11 Q3_K, 12 Q4_K, 13 Q5_K, 14 Q6_K, 20 IQ4_NL, 21 IQ3_S, 23 IQ4_XS
  size_t k, n;
  std::string sha, name, engine, policy_sha;
};
const std::vector<Q8PublicTensor> &q8_catalog() {
  // The application-side audit is not a replacement for service admission.
  // A future GPU request must be independently authenticated by the service.
  static const std::vector<Q8PublicTensor> entries = [] {
    std::vector<Q8PublicTensor> result;
    auto config = json::parse(q8_read_file("/shield/service.json", 65536));
    for (const auto &[engine, spec] : config.at("models").items()) {
      auto bytes = q8_read_file(spec.at("policy").get<std::string>(), 4<<20);
      const auto digest = q8_sha(bytes);
      if (digest != spec.at("policySha256").get<std::string>()) throw std::runtime_error("policy digest");
      auto policy = json::parse(bytes);
      if (policy.at("schema") != "shield-public-tensors/1" || policy.at("modelSha256") != spec.at("modelSha256"))
        throw std::runtime_error("policy binding");
      if (q8_sha(q8_read_file(spec.at("calibration").get<std::string>(), 4<<20)) !=
          policy.at("calibrationSha256").get<std::string>()) throw std::runtime_error("calibration digest");
      const auto &tensors = policy.at("tensors");
      if (!tensors.is_array() || tensors.size()>100000) throw std::runtime_error("policy tensor limit");
      for (const auto &tensor : tensors) {
        if (tensor.at("type") != 8 && tensor.at("type") != 12 && tensor.at("type") != 13 &&
            tensor.at("type") != 14 && tensor.at("type") != 20 && tensor.at("type") != 23 &&
            tensor.at("type") != 11 && tensor.at("type") != 21) continue;
        const int type = tensor.at("type").get<int>();
        const auto &shape = tensor.at("ne");
        if (!shape.is_array() || shape.size()!=4) throw std::runtime_error("policy shape");
        if (shape[2] != 1 || shape[3] != 1) continue;
        size_t k=shape[0].get<size_t>(), n=shape[1].get<size_t>();
        // A K-quant tensor whose rows are not whole super-blocks is never lowered.
        if (type != 8 && type != 20 && k%256) continue;
        if (!k || k%32 || !n || k>(size_t(2)<<30)/n ||
            tensor.at("nbytes")!=(type == 12 ? k*n/256*144 : type == 13 ? k*n/256*176 :
                                  type == 14 ? k*n/256*210 : type == 23 ? k*n/256*136 :
                                  type == 11 || type == 21 ? k*n/256*110 :
                                  type == 20 ? k*n/32*18 : k*n/32*34))
          throw std::runtime_error("policy tensor dimensions");
        result.push_back({type,k,n,tensor.at("sha256").get<std::string>(),tensor.at("name").get<std::string>(),engine,digest});
      }
    }
    return result;
  }();
  return entries;
}
// Set (by q8_preauthenticate in q8_direct.inc) while weights are authenticated
// before any launch has given their shape: a catalogued tensor of the type
// with as many weights and the digest answers, and the answer has its shape.
thread_local bool q8_any_shape = false;
json q8_catalog_answer(int type, size_t k, size_t n, const std::string &digest) {
  for (const auto &tensor:q8_catalog()) {
    if (tensor.type!=type || tensor.sha!=digest) continue;
    if (q8_any_shape ? tensor.k*tensor.n!=k*n : tensor.k!=k || tensor.n!=n) continue;
    json answer={{"approved",true},{"engine",tensor.engine},{"tensor",tensor.name},
                 {"sha256",digest},{"policySha256",tensor.policy_sha}};
    if (q8_any_shape) { answer["k"]=tensor.k; answer["n"]=tensor.n; }
    return answer;
  }
  // Unknown weights may be private. Do not expose their digest or contents.
  return {{"approved",false}};
}
json q8_authenticate(const std::vector<unsigned char> &quants,
                     const std::vector<unsigned char> &scales, size_t k, size_t n) {
  if (quants.size()!=k*n || scales.size()!=k*n/16) throw std::runtime_error("snapshot shape");
  Q8Hash hash;
  std::array<unsigned char, 4096*34> packed{};
  for (size_t first=0;first<k*n/32;first+=4096) {
    size_t count=std::min(size_t(4096), k*n/32-first);
    for (size_t block=0;block<count;++block) {
      std::memcpy(packed.data()+block*34, scales.data()+(first+block)*2, 2);
      std::memcpy(packed.data()+block*34+2, quants.data()+(first+block)*32, 32);
    }
    hash.update(packed.data(), count*34);
  }
  const auto digest=hash.finish();
  return q8_catalog_answer(8, k, n, digest);
}
// An IQ4_NL tensor: half d, then 16 quant bytes, for each 32 weights.
json iq4nl_authenticate(const std::vector<unsigned char> &quants, const std::vector<unsigned char> &d,
                        size_t k, size_t n) {
  const size_t blocks=k*n/32;
  if (k%32 || quants.size()!=blocks*16 || d.size()!=blocks*2) throw std::runtime_error("snapshot shape");
  Q8Hash hash;
  std::vector<unsigned char> packed(4096*18);
  for (size_t first=0;first<blocks;first+=4096) {
    const size_t count=std::min(size_t(4096), blocks-first);
    for (size_t block=0;block<count;++block) {
      std::memcpy(packed.data()+block*18, d.data()+(first+block)*2, 2);
      std::memcpy(packed.data()+block*18+2, quants.data()+(first+block)*16, 16);
    }
    hash.update(packed.data(), count*18);
  }
  const auto digest=hash.finish();
  return q8_catalog_answer(20, k, n, digest);
}
// An IQ4_XS, Q3_K or IQ3_S tensor is kept as its GGUF bytes: the digest is of
// the buffer. `type` is the GGML type and `block` its bytes for 256 weights.
json raw_authenticate(const std::vector<unsigned char> &bytes, size_t k, size_t n, int type, size_t block) {
  if (k%256 || bytes.size()!=k*n/256*block) throw std::runtime_error("snapshot shape");
  Q8Hash hash;
  hash.update(bytes.data(), bytes.size());
  const auto digest=hash.finish();
  return q8_catalog_answer(type, k, n, digest);
}
// For a Q5_K tensor: half d, half dmin, 12 scale bytes, 32 high-bit bytes and
// 128 quant bytes for each 256 weights.
json q5k_authenticate(const std::vector<unsigned char> &quants, const std::vector<unsigned char> &high,
                      const std::vector<unsigned char> &scales, const std::vector<unsigned char> &d,
                      const std::vector<unsigned char> &dmin, size_t k, size_t n) {
  const size_t blocks=k*n/256;
  if (k%256 || quants.size()!=blocks*128 || high.size()!=blocks*32 || scales.size()!=blocks*12 ||
      d.size()!=blocks*2 || dmin.size()!=blocks*2) throw std::runtime_error("snapshot shape");
  Q8Hash hash;
  std::vector<unsigned char> packed(1024*176);
  for (size_t first=0;first<blocks;first+=1024) {
    const size_t count=std::min(size_t(1024), blocks-first);
    for (size_t block=0;block<count;++block) {
      unsigned char *out=packed.data()+block*176;
      std::memcpy(out, d.data()+(first+block)*2, 2);
      std::memcpy(out+2, dmin.data()+(first+block)*2, 2);
      std::memcpy(out+4, scales.data()+(first+block)*12, 12);
      std::memcpy(out+16, high.data()+(first+block)*32, 32);
      std::memcpy(out+48, quants.data()+(first+block)*128, 128);
    }
    hash.update(packed.data(), count*176);
  }
  const auto digest=hash.finish();
  return q8_catalog_answer(13, k, n, digest);
}
// And for a Q6_K tensor: 128 low-bit bytes, 64 high-bit bytes, 16 scale bytes
// and half d for each 256 weights.
json q6k_authenticate(const std::vector<unsigned char> &low, const std::vector<unsigned char> &high,
                      const std::vector<unsigned char> &scales, const std::vector<unsigned char> &d,
                      size_t k, size_t n) {
  const size_t blocks=k*n/256;
  if (k%256 || low.size()!=blocks*128 || high.size()!=blocks*64 || scales.size()!=blocks*16 ||
      d.size()!=blocks*2) throw std::runtime_error("snapshot shape");
  Q8Hash hash;
  std::vector<unsigned char> packed(1024*210);
  for (size_t first=0;first<blocks;first+=1024) {
    const size_t count=std::min(size_t(1024), blocks-first);
    for (size_t block=0;block<count;++block) {
      unsigned char *out=packed.data()+block*210;
      std::memcpy(out, low.data()+(first+block)*128, 128);
      std::memcpy(out+128, high.data()+(first+block)*64, 64);
      std::memcpy(out+192, scales.data()+(first+block)*16, 16);
      std::memcpy(out+208, d.data()+(first+block)*2, 2);
    }
    hash.update(packed.data(), count*210);
  }
  const auto digest=hash.finish();
  return q8_catalog_answer(14, k, n, digest);
}
// The same question for a Q4_K tensor held as the four flat buffers of the
// OpenCL backend: the digest is of the GGUF block order (half d, half dmin,
// 12 scale bytes, 128 quant bytes for each 256 weights), exact bytes.
json q4k_authenticate(const std::vector<unsigned char> &quants, const std::vector<unsigned char> &scales,
                      const std::vector<unsigned char> &d, const std::vector<unsigned char> &dmin,
                      size_t k, size_t n) {
  const size_t blocks=k*n/256;
  if (k%256 || quants.size()!=blocks*128 || scales.size()!=blocks*12 || d.size()!=blocks*2 ||
      dmin.size()!=blocks*2) throw std::runtime_error("snapshot shape");
  Q8Hash hash;
  std::vector<unsigned char> packed(1024*144);
  for (size_t first=0;first<blocks;first+=1024) {
    const size_t count=std::min(size_t(1024), blocks-first);
    for (size_t block=0;block<count;++block) {
      unsigned char *out=packed.data()+block*144;
      std::memcpy(out, d.data()+(first+block)*2, 2);
      std::memcpy(out+2, dmin.data()+(first+block)*2, 2);
      std::memcpy(out+4, scales.data()+(first+block)*12, 12);
      std::memcpy(out+16, quants.data()+(first+block)*128, 128);
    }
    hash.update(packed.data(), count*144);
  }
  const auto digest=hash.finish();
  return q8_catalog_answer(12, k, n, digest);
}
void q8_log(const json &record) {
  // Test builds require their configured audit file. Packaged applications can
  // compile an empty path: service verification/accounting remains authoritative
  // and no unbounded per-launch diagnostic file is written in guest RAM.
  if (SHIELD_Q8_CAPTURE_LOG[0] == '\0') return;
  std::lock_guard lock(q8_log_mutex);
  Fd file(open(SHIELD_Q8_CAPTURE_LOG,O_WRONLY|O_CREAT|O_APPEND|O_CLOEXEC|O_NOFOLLOW,0600));
  if(file.value<0) throw std::runtime_error("capture log");
  auto text=record.dump()+'\n';
  write_all(file.value,text.data(),text.size());
}
#ifdef SHIELD_Q8_MASKED
#include "q8_masked.inc"
#include "q8_direct.inc"
#endif
struct Q8CaptureJob;
void q8_prepare_executor();
void CL_CALLBACK q8_ready(cl_event, cl_int, void *);
struct Q8CaptureJob {
  cl_command_queue queue=nullptr;
  cl_kernel kernel=nullptr;
  cl_event ready=nullptr, completion=nullptr;
  std::array<cl_mem,4> buffers{};
  std::array<size_t,3> global{}, local{}, read_bytes{};
  size_t input_offset=0, output_offset=0, output_bytes=0, k=0, n=0;
  bool masked=false;
  uint64_t enqueued_ns=0;
  json metadata;
  std::shared_ptr<Profile> profile;
  ~Q8CaptureJob() {
    for(auto buffer:buffers) if(buffer) REAL(clReleaseMemObject)(buffer);
    if(kernel) REAL(clReleaseKernel)(kernel);
    if(ready) REAL(clReleaseEvent)(ready);
    if(completion) REAL(clReleaseEvent)(completion);
    if(queue) REAL(clReleaseCommandQueue)(queue);
  }
  void execute() {
    cl_int status=CL_EXEC_STATUS_ERROR_FOR_EVENTS_IN_WAIT_LIST;
    try {
      if(REAL(clWaitForEvents)(1,&ready)) throw std::runtime_error("dependency failed");
      q8_timing.add(Q8Timing::Wait,enqueued_ns);
      const bool timed=profile && profile->sample(profile->started);
      auto stage_ns=Q8Timing::now();
      std::array<std::vector<unsigned char>,3> snapshots;
      for(size_t i=0;i<3;++i) {
        snapshots[i].resize(read_bytes[i]);
        if(REAL(clEnqueueReadBuffer)(queue,buffers[i],CL_TRUE,i==2?input_offset:0,
                                     read_bytes[i],snapshots[i].data(),0,nullptr,nullptr))
          throw std::runtime_error("snapshot read failed");
      }
      q8_timing.add(Q8Timing::Snapshot,stage_ns); stage_ns=Q8Timing::now();
      auto weights=q8_authenticate(snapshots[0],snapshots[1],k,n);
      q8_timing.add(Q8Timing::Authenticate,stage_ns);
#ifdef SHIELD_Q8_SNAPSHOT_TEST
      // Compiled only into the dedicated public-fixture test library.
      shield_q8_snapshot_test(k,n,read_bytes[2]/(k*4),snapshots[0].data(),snapshots[1].data(),snapshots[2].data());
#endif
      metadata["weights"]=std::move(weights);
      metadata["capturedBytes"]=read_bytes;
      metadata["phase"]="snapshot-validated";
      metadata["execution"]="original-cpu-kernel-pending";
      metadata["maskedDispatch"]=false;
#ifdef SHIELD_Q8_MASKED
      if(masked) {
        stage_ns=Q8Timing::now();
        auto result=q8_masked(snapshots[2],metadata.at("weights"),k,n,read_bytes[2]/(k*4),metadata);
        q8_timing.add(Q8Timing::Service,stage_ns);
        if(result) {
          stage_ns=Q8Timing::now();
          if(result->size()!=output_bytes) throw std::runtime_error("Q8 output extent");
          // All chunks have verified before any result is written to application
          // memory. Logging failure also leaves application output untouched.
          q8_log(metadata);
          q8_written(buffers[3],output_offset,output_bytes,false);
          if(REAL(clEnqueueWriteBuffer)(queue,buffers[3],CL_TRUE,output_offset,
              result->size(),result->data(),0,nullptr,nullptr)) throw std::runtime_error("Q8 output write");
          q8_timing.add(Q8Timing::Write,stage_ns);
          if(timed && profile->sample(profile->ended) && profile->queued<=profile->submitted &&
             profile->submitted<=profile->started && profile->started<=profile->ended)
            profile->recorded.store(true,std::memory_order_release);
          REAL(clSetUserEventStatus)(completion,CL_COMPLETE);
          q8_pending_jobs.fetch_sub(1);
          return;
        }
      }
#endif
      // An audit-log failure must happen before the CPU kernel writes output.
      // This record attests capture only; the completion event reports execution.
      q8_log(metadata);
      // Future executor must consume these snapshots. After successful capture
      // and logging, this audit runs the original CPU kernel on retained buffers,
      // including unapproved weight candidates. No protected GPU transaction has
      // begun, and there is no retry after the queued command fails.
      stage_ns=Q8Timing::now();
#ifdef SHIELD_Q8_MASKED
      q8_written(buffers[3],output_offset,output_bytes,false);
#endif
      if(REAL(clEnqueueNDRangeKernel)(queue,kernel,3,nullptr,global.data(),local.data(),0,nullptr,nullptr) ||
         REAL(clFinish)(queue)) throw std::runtime_error("CPU execution failed");
      q8_timing.add(Q8Timing::Cpu,stage_ns);
      if(timed && profile->sample(profile->ended) && profile->queued<=profile->submitted &&
         profile->submitted<=profile->started && profile->started<=profile->ended)
        profile->recorded.store(true,std::memory_order_release);
      status=CL_COMPLETE;
    } catch(...) { /* A queued execution failure never retries or selects CPU. */ }
    REAL(clSetUserEventStatus)(completion,status);
    q8_pending_jobs.fetch_sub(1);
  }
};
// Only dependency-ready jobs enter this worker. One worker bounds aggregate
// snapshot/result storage without blocking an application's enqueue thread or
// holding memory reservations for jobs waiting on future user events. Pending
// jobs retain only metadata, events, kernel clones and buffer references.
class Q8Executor {
  std::mutex mutex_;
  std::condition_variable ready_;
  std::deque<std::shared_ptr<Q8CaptureJob>> queue_;
public:
  Q8Executor() { std::thread([this] {
    for(;;) {
      std::shared_ptr<Q8CaptureJob> job;
      {
        std::unique_lock lock(mutex_);
        ready_.wait(lock,[this]{return !queue_.empty();});
        job=std::move(queue_.front()); queue_.pop_front();
      }
      job->execute();
    }
  }).detach(); }
  void push(std::shared_ptr<Q8CaptureJob> job) {
    { std::lock_guard lock(mutex_); queue_.push_back(std::move(job)); }
    ready_.notify_one();
  }
};
Q8Executor &q8_executor() {
  // Process-lifetime worker: no teardown can race a late ICD callback.
  static auto *executor=new Q8Executor;
  return *executor;
}
void q8_prepare_executor() { (void)q8_executor(); }
void CL_CALLBACK q8_ready(cl_event, cl_int, void *opaque) {
  std::unique_ptr<std::shared_ptr<Q8CaptureJob>> holder(
      static_cast<std::shared_ptr<Q8CaptureJob> *>(opaque));
  auto job=*holder;
  try { q8_executor().push(job); }
  catch(...) {
    REAL(clSetUserEventStatus)(job->completion,CL_OUT_OF_RESOURCES);
    q8_pending_jobs.fetch_sub(1);
  }
}
std::pair<bool,cl_int> capture_q8(cl_command_queue q, cl_kernel kernel, const Kernel &state, int kind,
    cl_uint dimensions,const size_t *offset,const size_t *global,const size_t *local,
    cl_uint count,const cl_event *wait,cl_event *event) {
  bool masked=false;
  q8_timing.launch();
  const auto enqueue_ns=Q8Timing::now();
#ifdef SHIELD_Q8_MASKED
  // The explicit arithmetic opt-in is read once; it is a property of the process.
  static const int contract=[] {
    const char *value=std::getenv("SHIELD_OPENCL_ARITHMETIC");
    return !value ? 0 : std::strcmp(value,"shield-fixed-point-v1") ? -1 : 1;
  }();
  if(contract<0) return {true,CL_INVALID_VALUE};
  masked=contract>0;
#endif
  // All refusals before queue insertion retain ordinary CPU execution.
  if(dimensions!=3 || !global || !local || (count&&!wait) || (!count&&wait)) return {false,0};
  for(bool svm:state.svm) if(svm) return {false,0};
  cl_context context=nullptr; cl_device_id device=nullptr;
  cl_command_queue_properties properties=0;
  if(REAL(clGetCommandQueueInfo)(q,CL_QUEUE_CONTEXT,sizeof(context),&context,nullptr) ||
     REAL(clGetCommandQueueInfo)(q,CL_QUEUE_DEVICE,sizeof(device),&device,nullptr) ||
     REAL(clGetCommandQueueInfo)(q,CL_QUEUE_PROPERTIES,sizeof(properties),&properties,nullptr) ||
     !cpu_device(device)) return {false,0};
  cl_context kernel_context=nullptr;
  if(REAL(clGetKernelInfo)(kernel,CL_KERNEL_CONTEXT,sizeof(kernel_context),&kernel_context,nullptr) ||
     kernel_context!=context) return {false,0};
  for(cl_uint i=0;i<count;++i) {
    cl_context c=nullptr;
    if(REAL(clGetEventInfo)(wait[i],CL_EVENT_CONTEXT,sizeof(c),&c,nullptr) || c!=context) return {false,0};
  }
#ifdef SHIELD_Q8_MASKED
  Q8LaunchMemo *memo=nullptr;
  uint64_t memo_epoch=0;
  if(masked && !count && q8_direct_enabled() && q8_memo_enabled() &&
     !(properties&(CL_QUEUE_PROFILING_ENABLE|CL_QUEUE_OUT_OF_ORDER_EXEC_MODE_ENABLE))) {
    static thread_local std::unordered_map<std::string,Q8LaunchMemo> memos;
    static thread_local std::string key;
    key.clear();
    const auto add=[&](const void *bytes,size_t size) { key.append(static_cast<const char *>(bytes),size); };
    add(&kernel,sizeof kernel); add(&q,sizeof q); add(&context,sizeof context); add(&kind,sizeof kind);
    add(global,3*sizeof *global); add(local,3*sizeof *local);
    const size_t none[3]={0,0,0};
    add(offset?offset:none,sizeof none);
    for(unsigned i=0;i<q8_kind_arguments(kind);++i) {
      const unsigned char size=(unsigned char)state.args[i].size();
      add(&size,1); add(state.args[i].data(),state.args[i].size());
    }
    memo_epoch=q8_epoch.load(std::memory_order_acquire);
    if(memos.size()>4096) memos.clear();
    memo=&memos[key];
    if(memo->epoch==memo_epoch) {
      Q8DirectLaunch launch=memo->launch;
      launch.event=event; launch.memo=memo;
      auto result=q8_direct(launch);
      q8_timing.add(Q8Timing::Enqueue,enqueue_ns);
      if(!result) return {false,0};
      return {true,*result};
    }
    memo->epoch=0; memo->identity=nullptr;
  }
#endif
  // Scalars as many places before a K-quant kernel has them as it has extra
  // weight buffers (see q8_admission.inc).
  const bool mv=q8_kind_vector(kind), raw=q8_kind_raw(kind);
  const unsigned weights=q8_kind_weights(kind), shift=raw?0:weights-2;
  std::array<cl_mem,7> buffers{};
  std::array<Storage,7> stored{};
  std::array<unsigned,7> indices{};
  for(unsigned i=0;i<weights;++i) indices[i]=i;
  indices[weights]=2+shift; indices[weights+1]=4+shift;
  std::array<uint64_t,19> scalars{};
  Q8LaunchView view{};
  view.kind=kind;
  for(unsigned i=0;i<3;++i) { view.global[i]=global[i]; view.local[i]=local[i]; view.offset[i]=offset?offset[i]:0; }
  try {
    const bool strided=kind==Q8_MV || kind==Q4K_MV || kind==Q5K_MV;  // byte strides passed as ulong
    if(raw) {
      // A plain kernel: weights offset, input offset, output offset
      // as ulong at 1, 3, 5; ne00, ne01, ne11 at 6, 7, 8. Held with the
      // weights offset at 4.
      for(unsigned i=1;i<9;++i) if(i!=2 && i!=4) {
        const bool wide=i<6;
        const auto &argument=state.args[i];
        if(argument.size()!=(wide?sizeof(cl_ulong):sizeof(cl_int))) return {false,0};
        uint64_t &held=scalars[i==1?4:i];
        if(wide) { cl_ulong value; std::memcpy(&value,argument.data(),sizeof(value)); held=value; }
        else { cl_int value; std::memcpy(&value,argument.data(),sizeof(value)); if(value<0) return {false,0}; held=value; }
      }
    }
    else for(unsigned i=3;i<q8_kind_scalars(kind);++i) if(i!=4) {
      // The Q4_K and Q5_K vector kernels take their two pointer offsets as int.
      bool wide=((i==3 || i==5) && kind!=Q4K_MV && kind!=Q5K_MV) || (strided && (i==8 || i==9 || i==10 || i==12 || i==13 || i==14));
      const auto &argument=state.args[i+shift];
      if(argument.size()!=(wide?sizeof(cl_ulong):sizeof(cl_int))) return {false,0};
      if(wide) { cl_ulong value; std::memcpy(&value,argument.data(),sizeof(value)); scalars[i]=value; }
      else { cl_int value; std::memcpy(&value,argument.data(),sizeof(value)); if(value<0) return {false,0}; scalars[i]=value; }
    }
    view.scalars=scalars;
    for(size_t i=0;i<weights+2;++i) {
      unsigned index=indices[i];
      if(state.args[index].size()!=sizeof(cl_mem)) return {false,0};
      std::memcpy(&buffers[i],state.args[index].data(),sizeof(cl_mem));
      Storage &memory=stored[i];
      if(!storage(buffers[i],memory) || memory.context!=context ||
         (memory.flags&CL_MEM_HOST_NO_ACCESS) || (i<=weights && (memory.flags&CL_MEM_HOST_WRITE_ONLY)) ||
         (masked && i==weights+1 && (memory.flags&CL_MEM_HOST_READ_ONLY))) return {false,0};
      size_t root_bytes=0;
      if(REAL(clGetMemObjectInfo)(memory.root,CL_MEM_SIZE,sizeof(root_bytes),&root_bytes,nullptr)) return {false,0};
      view.buffers[i]={reinterpret_cast<uintptr_t>(memory.root),root_bytes,memory.offset,memory.size};
    }
    const auto layout=q8_validate_launch(view);
    if(!layout) return {false,0};
    const auto bytes=layout->read_bytes;
    const size_t checked_m=layout->m, checked_k=layout->k, checked_n=layout->n, checked_output=layout->output_bytes;
    // These equalities bind the snapshot and write extents to the same checked
    // ABI, in addition to the stride/grid/alias/range validation above.
    if(!checked_k || checked_k!=scalars[6] || !checked_n || checked_n!=scalars[7] ||
       !checked_m || checked_m!=scalars[raw?8:(kind==Q6K_MV || kind==IQ4NL_MV)?12:mv?16:9] ||
       checked_k>(512u<<20)/4/checked_m ||
       checked_n>(512u<<20)/4/checked_m || bytes[weights]!=checked_m*checked_k*4 ||
       checked_output!=checked_m*checked_n*4) return {false,0};
    size_t total=0;
    for(unsigned i=0;i<=weights;++i) {
      if(bytes[i]>size_t(2)<<30) return {false,0};  // one weight buffer or the activation
      total+=bytes[i];
    }
    if(masked) {
      if(checked_output>512u<<20) return {false,0};
      total+=checked_output;
    }
    if(total>size_t(4)<<30) return {false,0};
#ifdef SHIELD_Q8_MASKED
    if(masked && !count && q8_direct_enabled() &&
       !(properties&(CL_QUEUE_PROFILING_ENABLE|CL_QUEUE_OUT_OF_ORDER_EXEC_MODE_ENABLE))) {
      Q8DirectLaunch launch{q,kind,buffers,stored,bytes,raw?size_t(scalars[4]):0,size_t(scalars[3]),size_t(scalars[5]),checked_output,
                            checked_k,checked_n,checked_m,event};
      launch.memo=memo;
      auto result=q8_direct(launch);
      q8_timing.add(Q8Timing::Enqueue,enqueue_ns);
      if(!result) return {false,0};
      if(memo) { memo->launch=launch; memo->launch.event=nullptr; memo->launch.memo=nullptr; memo->epoch=memo_epoch; }
      return {true,*result};
    }
#endif
    // The queued route knows the Q8_0 kernels only, and snapshots the weights
    // for every launch: it keeps the smaller limit it has always had.
    if(kind!=Q8_MM && kind!=Q8_MV) return {false,0};
    if(total>512u<<20) return {false,0};
    if(!queued_offload_reliable()) return {false,0};
    // The queued route keeps a public description of the launch for its log.
    json metadata={{"variant",mv?"mv":"mm"},{"scalars",json::object()},
                   {"buffers",json::object()},{"global",{global[0],global[1],global[2]}},
                   {"local",{local[0],local[1],local[2]}},
                   {"offset",{view.offset[0],view.offset[1],view.offset[2]}}};
    for(unsigned i=3;i<19;++i) if(i!=4) metadata["scalars"][std::to_string(i)]=scalars[i];
    for(size_t i=0;i<4;++i)
      metadata["buffers"][std::to_string(indices[i])]={{"root",view.buffers[i].root},
          {"root_bytes",view.buffers[i].root_bytes},{"offset",view.buffers[i].offset},{"size",view.buffers[i].size}};
    auto job=std::make_shared<Q8CaptureJob>();
    job->masked=masked; job->output_offset=scalars[5]; job->output_bytes=checked_output;
    job->read_bytes={bytes[0],bytes[1],bytes[2]}; job->input_offset=scalars[3]; job->k=scalars[6]; job->n=scalars[7];
    std::copy(global,global+3,job->global.begin()); std::copy(local,local+3,job->local.begin());
    // Allocation addresses stay private: logs contain stable per-record root
    // labels preserving alias relationships, not host/process pointers.
    std::map<uint64_t,unsigned> roots;
    for(auto &[key,buffer]:metadata["buffers"].items()) {
      (void)key;
      uint64_t address=buffer.at("root").get<uint64_t>();
      auto [it,inserted]=roots.emplace(address,unsigned(roots.size()+1)); (void)inserted;
      buffer["root"]=it->second;
    }
    job->metadata=std::move(metadata);
    job->enqueued_ns=enqueue_ns;
    if(properties&CL_QUEUE_PROFILING_ENABLE) {
      job->profile=std::make_shared<Profile>(); job->profile->device=device;
      if(!job->profile->sample(job->profile->queued)) return {false,0};
    }
    cl_int error=0;
    job->kernel=REAL(clCloneKernel)(kernel,&error); if(!job->kernel) return {false,0};
    job->queue=REAL(clCreateCommandQueue)(context,device,0,&error); if(!job->queue) return {false,0};
    job->completion=REAL(clCreateUserEvent)(context,&error); if(!job->completion) return {false,0};
    for(size_t i=0;i<4;++i) {
      if(REAL(clRetainMemObject)(buffers[i])) return {false,0};
      job->buffers[i]=buffers[i];
    }
    q8_prepare_executor();
    if(q8_pending_jobs.fetch_add(1)>=4096) {
      q8_pending_jobs.fetch_sub(1); return {true,CL_OUT_OF_RESOURCES};
    }
    cl_event done=nullptr; bool tracked=false;
    try {
      error=REAL(clEnqueueMarkerWithWaitList)(q,count,wait,&job->ready);
      if(error) { q8_pending_jobs.fetch_sub(1); return {true,error}; }
      error=REAL(clEnqueueMarkerWithWaitList)(q,1,&job->completion,&done);
      if(error) { q8_pending_jobs.fetch_sub(1); return {true,error}; }
      if(REAL(clFlush)(q)) throw std::runtime_error("queue flush");
      if(event) { std::lock_guard lock(mutex); offload_events[done]={1,job->profile}; tracked=true; }
      if(job->profile && !job->profile->sample(job->profile->submitted)) throw std::runtime_error("profile timer");
      auto holder=std::make_unique<std::shared_ptr<Q8CaptureJob>>(job);
      if(REAL(clSetEventCallback)(job->ready,CL_COMPLETE,q8_ready,holder.get()))
        throw std::runtime_error("ready callback");
      // A callback can run synchronously; release ownership without dereferencing.
      holder.release();
      if(event) *event=done; else REAL(clReleaseEvent)(done);
      q8_timing.add(Q8Timing::Enqueue,enqueue_ns);
      return {true,CL_SUCCESS};
    } catch(...) {
      if(tracked) { std::lock_guard lock(mutex); offload_events.erase(done); }
      if(done) { REAL(clSetUserEventStatus)(job->completion,CL_OUT_OF_RESOURCES); REAL(clReleaseEvent)(done); }
      q8_pending_jobs.fetch_sub(1);
      return {true,CL_OUT_OF_RESOURCES};
    }
  } catch(...) { return {false,0}; }
}

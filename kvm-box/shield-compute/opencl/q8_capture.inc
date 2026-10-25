// Experimental only: included inside the frontend namespace with
// SHIELD_Q8_CAPTURE. Captures inputs after queue dependencies, authenticates
// public weights, then executes the original kernel on the CPU ICD.
// Without SHIELD_Q8_MASKED it never invokes Shield or a physical GPU.
// The separate masked build requires explicit arithmetic opt-in for that route.
#ifndef SHIELD_Q8_CAPTURE_LOG
#define SHIELD_Q8_CAPTURE_LOG "/evidence/q8-capture.jsonl"
#endif
// Cache only public layout validation, never content authentication or results.
// Keys include every freshly queried root identity/range, scalar, grid and offset.
// Buffer reuse is harmless when all geometry is equal: bytes are still captured
// and authenticated for every launch after its dependency events complete.
std::pair<json,bool> q8_layout(const json &metadata) {
  static std::mutex cache_mutex;
  static std::map<std::string,json> cache;
  const auto key=metadata.dump();
  {
    std::lock_guard lock(cache_mutex);
    auto it=cache.find(key);
    if(it!=cache.end()) return {it->second,true};
  }
  auto result=invoke_compiler({{"q8Launch",metadata}});
  if(key.size()<=8192 && result.is_object() &&
     result.value("schema","")=="shield-q8-capture-layout/1" &&
     !result.value("dispatchable",true)) {
    std::lock_guard lock(cache_mutex);
    if(cache.size()>=1024) cache.clear();
    cache.emplace(key,result);
  }
  return {result,false};
}
std::atomic<size_t> q8_pending_jobs{0};
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
        if (tensor.at("type") != 8) continue;
        const auto &shape = tensor.at("ne");
        if (!shape.is_array() || shape.size()!=4) throw std::runtime_error("policy shape");
        if (shape[2] != 1 || shape[3] != 1) continue;
        size_t k=shape[0].get<size_t>(), n=shape[1].get<size_t>();
        if (!k || k%32 || !n || k>(512u<<20)/n || tensor.at("nbytes")!=k*n/32*34)
          throw std::runtime_error("policy tensor dimensions");
        result.push_back({k,n,tensor.at("sha256").get<std::string>(),tensor.at("name").get<std::string>(),engine,digest});
      }
    }
    return result;
  }();
  return entries;
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
  for (const auto &tensor:q8_catalog())
    if (tensor.k==k && tensor.n==n && tensor.sha==digest)
      return {{"approved",true},{"engine",tensor.engine},{"tensor",tensor.name},
              {"sha256",digest},{"policySha256",tensor.policy_sha}};
  // Unknown weights may be private. Do not expose their digest or contents.
  return {{"approved",false}};
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
      const bool timed=profile && profile->sample(profile->started);
      std::array<std::vector<unsigned char>,3> snapshots;
      for(size_t i=0;i<3;++i) {
        snapshots[i].resize(read_bytes[i]);
        if(REAL(clEnqueueReadBuffer)(queue,buffers[i],CL_TRUE,i==2?input_offset:0,
                                     read_bytes[i],snapshots[i].data(),0,nullptr,nullptr))
          throw std::runtime_error("snapshot read failed");
      }
      auto weights=q8_authenticate(snapshots[0],snapshots[1],k,n);
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
        auto result=q8_masked(snapshots[2],metadata.at("weights"),k,n,read_bytes[2]/(k*4),metadata);
        if(result) {
          if(result->size()!=output_bytes) throw std::runtime_error("Q8 output extent");
          // All chunks have verified before any result is written to application
          // memory. Logging failure also leaves application output untouched.
          q8_log(metadata);
          if(REAL(clEnqueueWriteBuffer)(queue,buffers[3],CL_TRUE,output_offset,
              result->size(),result->data(),0,nullptr,nullptr)) throw std::runtime_error("Q8 output write");
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
      if(REAL(clEnqueueNDRangeKernel)(queue,kernel,3,nullptr,global.data(),local.data(),0,nullptr,nullptr) ||
         REAL(clFinish)(queue)) throw std::runtime_error("CPU execution failed");
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
std::pair<bool,cl_int> capture_q8(cl_command_queue q, cl_kernel kernel, const Kernel &state,
    cl_uint dimensions,const size_t *offset,const size_t *global,const size_t *local,
    cl_uint count,const cl_event *wait,cl_event *event) {
  bool masked=false;
#ifdef SHIELD_Q8_MASKED
  if(const char *contract=std::getenv("SHIELD_OPENCL_ARITHMETIC")) {
    if(std::strcmp(contract,"shield-fixed-point-v1")) return {true,CL_INVALID_VALUE};
    masked=true;
  }
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
  std::array<cl_mem,4> buffers{};
  std::array<unsigned,4> indices={0,1,2,4};
  json metadata={{"variant",state.q8_analysis.at("variant")},{"scalars",json::object()},
                 {"buffers",json::object()},{"global",{global[0],global[1],global[2]}},
                 {"local",{local[0],local[1],local[2]}},
                 {"offset",{offset?offset[0]:0,offset?offset[1]:0,offset?offset[2]:0}}};
  const bool mv=state.q8_analysis.at("variant")=="mv";
  std::array<uint64_t,19> scalars{};
  try {
    for(unsigned i=3;i<19;++i) if(i!=4) {
      bool wide=i==3 || i==5 || (mv && (i==8 || i==9 || i==10 || i==12 || i==13 || i==14));
      if(state.args[i].size()!=(wide?sizeof(cl_ulong):sizeof(cl_int))) return {false,0};
      if(wide) { cl_ulong value; std::memcpy(&value,state.args[i].data(),sizeof(value)); scalars[i]=value; }
      else { cl_int value; std::memcpy(&value,state.args[i].data(),sizeof(value)); if(value<0) return {false,0}; scalars[i]=value; }
      metadata["scalars"][std::to_string(i)]=scalars[i];
    }
    for(size_t i=0;i<4;++i) {
      unsigned index=indices[i];
      if(state.args[index].size()!=sizeof(cl_mem)) return {false,0};
      std::memcpy(&buffers[i],state.args[index].data(),sizeof(cl_mem));
      Storage memory;
      if(!storage(buffers[i],memory) || memory.context!=context ||
         (memory.flags&CL_MEM_HOST_NO_ACCESS) || (i<3 && (memory.flags&CL_MEM_HOST_WRITE_ONLY)) ||
         (masked && i==3 && (memory.flags&CL_MEM_HOST_READ_ONLY))) return {false,0};
      size_t root_bytes=0;
      if(REAL(clGetMemObjectInfo)(memory.root,CL_MEM_SIZE,sizeof(root_bytes),&root_bytes,nullptr)) return {false,0};
      metadata["buffers"][std::to_string(index)]={{"root",reinterpret_cast<uintptr_t>(memory.root)},
          {"root_bytes",root_bytes},{"offset",memory.offset},{"size",memory.size}};
    }
    auto [layout,layout_hit]=q8_layout(metadata);
    metadata["layoutCacheHit"]=layout_hit;
    if(layout.value("schema","")!="shield-q8-capture-layout/1" || layout.value("dispatchable",true)) return {false,0};
    auto bytes=layout.at("readBytes").get<std::array<size_t,3>>();
    const size_t checked_m=layout.at("m").get<size_t>(), checked_k=layout.at("k").get<size_t>(),
                 checked_n=layout.at("n").get<size_t>(), checked_output=layout.at("outputBytes").get<size_t>();
    // These equalities bind the snapshot and write extents to the same checked
    // ABI, in addition to the helper's stride/grid/alias/range validation.
    if(!checked_k || checked_k!=scalars[6] || !checked_n || checked_n!=scalars[7] ||
       !checked_m || checked_m!=scalars[mv?16:9] || checked_k>(512u<<20)/4/checked_m ||
       checked_n>(512u<<20)/4/checked_m || bytes[2]!=checked_m*checked_k*4 ||
       checked_output!=checked_m*checked_n*4) return {false,0};
    if(bytes[0]>512u<<20 || bytes[1]>512u<<20 || bytes[2]>512u<<20) return {false,0};
    size_t total=bytes[0]+bytes[1]+bytes[2];
    if(masked) {
      size_t output_bytes=layout.at("outputBytes").get<size_t>();
      if(output_bytes>512u<<20) return {false,0};
      total+=output_bytes;
    }
    if(total>512u<<20) return {false,0};
    auto job=std::make_shared<Q8CaptureJob>();
    job->masked=masked; job->output_offset=scalars[5]; job->output_bytes=checked_output;
    job->read_bytes=bytes; job->input_offset=scalars[3]; job->k=scalars[6]; job->n=scalars[7];
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
      return {true,CL_SUCCESS};
    } catch(...) {
      if(tracked) { std::lock_guard lock(mutex); offload_events.erase(done); }
      if(done) { REAL(clSetUserEventStatus)(job->completion,CL_OUT_OF_RESOURCES); REAL(clReleaseEvent)(done); }
      q8_pending_jobs.fetch_sub(1);
      return {true,CL_OUT_OF_RESOURCES};
    }
  } catch(...) { return {false,0}; }
}

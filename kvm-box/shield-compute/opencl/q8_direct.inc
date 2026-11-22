// Direct Q8 route. Included inside the frontend namespace with SHIELD_Q8_MASKED.
//
// The queued route in q8_capture.inc snapshots and re-hashes a weight tensor for
// every launch and sends each activation through the service's JSON framing.
// This route keeps the same decisions and moves the cost off the per-launch path:
//
//  * A weight region is authenticated once and remembered until something could
//    have written to it. Every API that can write a buffer drops the overlapping
//    entries (q8_written and its callers in shield_opencl.cpp); a later launch
//    then hashes the bytes again. The remembered answer only says which public
//    tensor the application means. The service computes with its own
//    authenticated copy of that tensor, so a missed write could give the
//    application a product for the original weights, never disclose anything.
//  * Activations go to the measured executor over one long-lived session socket
//    (see native/q8_executor.cpp), which the service hands over after checking
//    this process's credentials.
//  * The launch runs inside clEnqueueNDRangeKernel: prior commands on the queue
//    are finished first, and the output is in place when the call returns.
//
// It applies to in-order, non-profiling queues and launches without a wait list.
// Everything else keeps the queued route. SHIELD_OPENCL_Q8_DIRECT=0 disables it.

std::mutex q8_cache_mutex;
// Which approved public tensor a weight region holds, if any.
struct Q8Identity {
  bool approved = false;
  std::string engine, tensor, sha256;
  // Slot opened for this tensor in the session with this serial number.
  mutable uint64_t session_serial = 0;
  mutable uint32_t slot = 0;
  // A private copy of the weights for local computation, made the first time
  // a launch with them cannot be offloaded. Used only under q8_direct_mutex.
  mutable std::shared_ptr<const struct Q8Local> local;
  mutable bool local_unavailable = false;
  // Batch widths (bit m) the executor declined for this tensor. Its answer
  // depends only on the tensor and the width, so it is not asked again.
  mutable uint64_t declined = 0;
};
struct Q8Region {
  cl_mem root;
  size_t offset, size;
  bool overlaps(const Q8Region &other) const {
    return root == other.root && offset < other.offset + other.size && other.offset < offset + size;
  }
};
struct Q8Known {
  // The tensor's weight buffers: codes and scales for Q8_0; four for the
  // K-quants (quants, scales, d, dmin for Q4_K; low bits, high bits, scales, d
  // for Q6_K) and five for Q5_K (quants, high bits, scales, d, dmin).
  std::array<Q8Region, 5> weights;
  unsigned count;
  size_t k, n;
  std::shared_ptr<const Q8Identity> identity;  // q8_authenticate's answer for these bytes
};
std::vector<Q8Known> q8_known;                 // each entry holds a reference on its roots
std::map<cl_mem, Q8Region> q8_children;        // sub-buffers and buffer-backed images
std::map<void *, cl_mem> q8_write_maps;        // host pointers mapped for writing
std::atomic<bool> q8_untracked_writers{false}; // an API this file cannot follow is in use
std::atomic<size_t> q8_known_count{0};

bool q8_direct_shared_enabled() {
  static const bool enabled = [] {
    const char *value = std::getenv("SHIELD_OPENCL_Q8_SHARED");
    return !value || std::strcmp(value, "0");
  }();
  return enabled;
}
bool q8_direct_enabled() {
  static const bool enabled = [] {
    const char *value = std::getenv("SHIELD_OPENCL_Q8_DIRECT");
    return !value || std::strcmp(value, "0");
  }();
  return enabled;
}

// Caller holds q8_cache_mutex.
Q8Region q8_resolve(cl_mem handle, size_t offset, size_t size, bool whole) {
  auto it = q8_children.find(handle);
  if (it == q8_children.end()) return {handle, whole ? 0 : offset, whole ? SIZE_MAX / 2 : size};
  if (whole) return it->second;
  return {it->second.root, it->second.offset + offset, std::min(size, it->second.size)};
}
void q8_forget_locked(const Q8Region &written) {
  for (size_t i = 0; i < q8_known.size();) {
    bool overlap = false;
    for (unsigned w = 0; w < q8_known[i].count; ++w) overlap = overlap || q8_known[i].weights[w].overlaps(written);
    if (overlap) {
      for (unsigned w = 0; w < q8_known[i].count; ++w) REAL(clReleaseMemObject)(q8_known[i].weights[w].root);
      q8_known[i] = std::move(q8_known.back());
      q8_known.pop_back();
    } else {
      ++i;
    }
  }
  q8_known_count.store(q8_known.size(), std::memory_order_release);
}
// Something may write [offset, offset+size) of this buffer, sub-buffer or image.
void q8_written(cl_mem handle, size_t offset, size_t size, bool whole) {
  if (!handle || !q8_known_count.load(std::memory_order_acquire)) return;
  std::lock_guard lock(q8_cache_mutex);
  q8_forget_locked(q8_resolve(handle, offset, size, whole));
}
void q8_written_everything() {
  if (!q8_known_count.load(std::memory_order_acquire)) return;
  std::lock_guard lock(q8_cache_mutex);
  for (auto &known : q8_known) {
    for (unsigned w = 0; w < known.count; ++w) REAL(clReleaseMemObject)(known.weights[w].root);
  }
  q8_known.clear();
  q8_known_count.store(0, std::memory_order_release);
}
// An ordinary kernel may write any buffer it was given. `only` restricts that
// to one argument for the Q8 kernels, whose other buffers are read-only inputs.
void q8_kernel_may_write(cl_kernel kernel, int only = -1) {
  if (!q8_known_count.load(std::memory_order_acquire)) return;
  std::vector<std::pair<cl_uint, cl_mem>> mems;
  {
    std::lock_guard lock(mutex);
    auto it = kernels.find(kernel);
    if (it == kernels.end()) return;
    mems = it->second.mems;
  }
  std::lock_guard lock(q8_cache_mutex);
  for (const auto &[index, handle] : mems)
    if (handle && (only < 0 || index == cl_uint(only)))
      q8_forget_locked(q8_resolve(handle, 0, 0, true));
}
void q8_child(cl_mem child, cl_mem parent, size_t offset, size_t size) {
  if (!child || !parent) return;
  std::lock_guard lock(q8_cache_mutex);
  auto it = q8_children.find(parent);
  if (it == q8_children.end()) q8_children[child] = {parent, offset, size};
  else q8_children[child] = {it->second.root, it->second.offset + offset, std::min(size, it->second.size)};
}

// Every float32 is finite: no exponent field is all ones. Written as a
// reduction over fixed blocks so the compiler vectorises it; a large product
// (a vocabulary projection) is otherwise a visible share of a token.
__attribute__((optimize("O3"), target_clones("avx2", "default")))
bool q8_all_finite(const unsigned char *bytes, size_t count) {
  uint32_t largest = 0;
  size_t i = 0;
  for (; i + 64 <= count; i += 64) {
    uint32_t block[64];
    std::memcpy(block, bytes + i * 4, sizeof(block));
    for (unsigned j = 0; j < 64; ++j) largest = std::max(largest, block[j] & 0x7fffffffu);
  }
  for (; i < count; ++i) {
    uint32_t bits;
    std::memcpy(&bits, bytes + i * 4, 4);
    largest = std::max(largest, bits & 0x7fffffffu);
  }
  return largest < 0x7f800000u;
}

// Local computation of a Q8 product that cannot be offloaded: weights this
// process has not shown to be an approved public tensor, or an approved tensor
// the backend has no calibrated site for. The original kernel computes
//   y[r][j] = sum over 32-wide blocks b of scale[j][b] * sum_i code[j][b][i] * x[r][b][i]
// on the CPU runtime's device, where it is slow (one thread under the inline
// driver, emulated half arithmetic). This computes the same sums in float32
// from a private copy of the weights. The order of additions differs from the
// kernel's sub-group reduction, so results can differ in the last bits, as
// they do between any two float implementations. SHIELD_OPENCL_Q8_LOCAL=0
// keeps the original kernel.
#include "q8_kquant.inc"
std::atomic<size_t> q8_local_bytes{0};
struct Q8Local {
  size_t k = 0, n = 0, charged = 0;
  // Weights that share a scale: 32 for Q8_0. A Q6_K tensor is held the same
  // way with 16: its 6-bit quants less 32 as codes, and d times the 8-bit
  // scale of each sixteen as the scale.
  size_t block = 32;
  std::vector<int8_t> codes;   // n rows of k
  std::vector<float> scales;   // n rows of k/block
  // Q4_K and Q5_K weights are scale * quant - minimum for each 32: the quants
  // are the codes, and the minimum of each 32 is taken off the sum of that
  // part of the activation. Empty for the other types.
  std::vector<float> minimums; // n rows of k/32
  ~Q8Local() { q8_local_bytes -= charged; }
};
// Private weight copies for local products, in total: 1 GiB unless
// SHIELD_OPENCL_Q8_LOCAL_MB says otherwise. A model with many tensors the
// executor declines needs more, or the rest run their own kernels.
const size_t Q8_LOCAL_LIMIT = [] {
  const char *value = std::getenv("SHIELD_OPENCL_Q8_LOCAL_MB");
  return size_t(std::min<unsigned long long>(value ? std::strtoull(value, nullptr, 10) : 1024, 1u << 20)) << 20;
}();
bool q8_local_enabled() {
  static const bool enabled = [] {
    const char *value = std::getenv("SHIELD_OPENCL_Q8_LOCAL");
    return !value || std::strcmp(value, "0");
  }();
  return enabled;
}
void q8_local_rows_scalar(const Q8Local &w, const float *x, float *y, size_t first, size_t last) {
  const size_t blocks = w.k / 32;
  for (size_t j = first; j < last; ++j) {
    const int8_t *codes = w.codes.data() + j * w.k;
    const float *scales = w.scales.data() + j * blocks;
    float total = 0;
    for (size_t b = 0; b < blocks; ++b) {
      float sum = 0;
      for (size_t i = 0; i < 32; ++i) sum += float(codes[b * 32 + i]) * x[b * 32 + i];
      total += sum * scales[b];
    }
    y[j] = total;
  }
}
#if defined(__x86_64__)
__attribute__((target("avx2")))
void q8_local_rows_avx2(const Q8Local &w, const float *x, float *y, size_t first, size_t last) {
  const size_t blocks = w.k / 32;
  for (size_t j = first; j < last; ++j) {
    const int8_t *codes = w.codes.data() + j * w.k;
    const float *scales = w.scales.data() + j * blocks;
    __m256 total = _mm256_setzero_ps();
    for (size_t b = 0; b < blocks; ++b) {
      const int8_t *c = codes + b * 32;
      const float *v = x + b * 32;
      __m256 sum = _mm256_mul_ps(_mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_loadl_epi64((const __m128i *)c))),
                                 _mm256_loadu_ps(v));
      for (int part = 1; part < 4; ++part)
        sum = _mm256_add_ps(sum, _mm256_mul_ps(
            _mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_loadl_epi64((const __m128i *)(c + 8 * part)))),
            _mm256_loadu_ps(v + 8 * part)));
      total = _mm256_add_ps(total, _mm256_mul_ps(sum, _mm256_set1_ps(scales[b])));
    }
    __m128 half = _mm_add_ps(_mm256_castps256_ps128(total), _mm256_extractf128_ps(total, 1));
    half = _mm_add_ps(half, _mm_movehl_ps(half, half));
    half = _mm_add_ss(half, _mm_shuffle_ps(half, half, 1));
    y[j] = _mm_cvtss_f32(half);
  }
}
#endif
#if defined(__x86_64__)
// Sixteen codes at a time. Each row is one pass over its codes; a row of a
// feed-forward projection is tens of kilobytes, so the pass is bound by how
// fast the weights arrive from memory and the conversion must not add to it.
__attribute__((target("avx512f,avx512bw,avx512vl,fma")))
void q8_local_rows_avx512(const Q8Local &w, const float *x, float *y, size_t first, size_t last) {
  const size_t blocks = w.k / 32;
  for (size_t j = first; j < last; ++j) {
    const int8_t *codes = w.codes.data() + j * w.k;
    const float *scales = w.scales.data() + j * blocks;
    __m512 total = _mm512_setzero_ps();
    for (size_t b = 0; b < blocks; ++b) {
      const int8_t *c = codes + b * 32;
      const float *v = x + b * 32;
      __m512 sum = _mm512_mul_ps(_mm512_cvtepi32_ps(_mm512_cvtepi8_epi32(_mm_loadu_si128((const __m128i *)c))),
                                 _mm512_loadu_ps(v));
      sum = _mm512_fmadd_ps(_mm512_cvtepi32_ps(_mm512_cvtepi8_epi32(_mm_loadu_si128((const __m128i *)(c + 16)))),
                            _mm512_loadu_ps(v + 16), sum);
      total = _mm512_fmadd_ps(sum, _mm512_set1_ps(scales[b]), total);
    }
    y[j] = _mm512_reduce_add_ps(total);
  }
}
#endif

// The same sums with a scale for every sixteen codes.
void q8_local_rows16_scalar(const Q8Local &w, const float *x, float *y, size_t first, size_t last) {
  const size_t blocks = w.k / 16;
  for (size_t j = first; j < last; ++j) {
    const int8_t *codes = w.codes.data() + j * w.k;
    const float *scales = w.scales.data() + j * blocks;
    float total = 0;
    for (size_t b = 0; b < blocks; ++b) {
      float sum = 0;
      for (size_t i = 0; i < 16; ++i) sum += float(codes[b * 16 + i]) * x[b * 16 + i];
      total += sum * scales[b];
    }
    y[j] = total;
  }
}
#if defined(__x86_64__)
__attribute__((target("avx512f,avx512bw,avx512vl,fma")))
void q8_local_rows16_avx512(const Q8Local &w, const float *x, float *y, size_t first, size_t last) {
  const size_t blocks = w.k / 16;
  for (size_t j = first; j < last; ++j) {
    const int8_t *codes = w.codes.data() + j * w.k;
    const float *scales = w.scales.data() + j * blocks;
    __m512 total = _mm512_setzero_ps();
    for (size_t b = 0; b < blocks; ++b) {
      const __m512 sum = _mm512_mul_ps(
          _mm512_cvtepi32_ps(_mm512_cvtepi8_epi32(_mm_loadu_si128((const __m128i *)(codes + b * 16)))),
          _mm512_loadu_ps(x + b * 16));
      total = _mm512_fmadd_ps(sum, _mm512_set1_ps(scales[b]), total);
    }
    y[j] = _mm512_reduce_add_ps(total);
  }
}
#endif

// Threads that split the rows of a large local product. They are started once
// and wait between products; starting threads for every product cost more than
// the product. A forked child starts its own.
class Q8LocalPool {
public:
  using Rows = void (*)(const Q8Local &, const float *, float *, size_t, size_t);
  void run(Rows rows, const Q8Local &w, const float *x, float *y, size_t parts) {
    std::unique_lock lock(mutex_);
    if (owner_ != getpid()) {
      for (auto &thread : threads_) thread.detach();  // they did not survive the fork
      threads_.clear();
      owner_ = getpid();
      generation_ = pending_ = 0;
    }
    while (threads_.size() + 1 < parts) threads_.emplace_back([this, index = threads_.size() + 1] { work(index); });
    rows_ = rows; w_ = &w; x_ = x; y_ = y; parts_ = parts;
    pending_ = parts - 1;
    ++generation_;
    lock.unlock();
    start_.notify_all();
    const size_t step = (w.n + parts - 1) / parts;
    rows(w, x, y, 0, std::min(w.n, step));
    lock.lock();
    done_.wait(lock, [this] { return pending_ == 0; });
  }

private:
  void work(size_t index) {
    uint64_t seen = 0;
    std::unique_lock lock(mutex_);
    for (;;) {
      start_.wait(lock, [&] { return generation_ != seen; });
      seen = generation_;
      if (index >= parts_) continue;
      const Rows rows = rows_;
      const Q8Local &w = *w_;
      const float *x = x_;
      float *y = y_;
      const size_t step = (w.n + parts_ - 1) / parts_;
      lock.unlock();
      if (index * step < w.n) rows(w, x, y, index * step, std::min(w.n, (index + 1) * step));
      lock.lock();
      if (--pending_ == 0) done_.notify_one();
    }
  }
  std::mutex mutex_;
  std::condition_variable start_, done_;
  std::vector<std::thread> threads_;
  pid_t owner_ = 0;
  uint64_t generation_ = 0;
  size_t pending_ = 0, parts_ = 0;
  Rows rows_ = nullptr;
  const Q8Local *w_ = nullptr;
  const float *x_ = nullptr;
  float *y_ = nullptr;
};

void q8_local_product(const Q8Local &w, const float *x, float *y, size_t m) {
  static const size_t threads = [] {
    const char *value = std::getenv("SHIELD_OPENCL_Q8_LOCAL_THREADS");
    const size_t wanted = value ? std::strtoull(value, nullptr, 10) : 4;
    return std::min<size_t>(std::max<size_t>(wanted, 1), 16);
  }();
  static const Q8LocalPool::Rows rows16 = [] {
    Q8LocalPool::Rows chosen = q8_local_rows16_scalar;
#if defined(__x86_64__)
    if (__builtin_cpu_supports("avx512f") && __builtin_cpu_supports("avx512bw") &&
        __builtin_cpu_supports("avx512vl") && __builtin_cpu_supports("fma"))
      chosen = q8_local_rows16_avx512;
#endif
    return chosen;
  }();
  static const Q8LocalPool::Rows rows32 = [] {
    Q8LocalPool::Rows chosen = q8_local_rows_scalar;
#if defined(__x86_64__)
    if (__builtin_cpu_supports("avx2")) chosen = q8_local_rows_avx2;
    if (__builtin_cpu_supports("avx512f") && __builtin_cpu_supports("avx512bw") &&
        __builtin_cpu_supports("avx512vl") && __builtin_cpu_supports("fma"))
      chosen = q8_local_rows_avx512;
#endif
    return chosen;
  }();
  const Q8LocalPool::Rows rows = w.block == 16 ? rows16 : rows32;
  // The pool lives for the process: no teardown can race a late launch.
  static auto *pool = new Q8LocalPool;
  // Small products are not worth waking threads for.
  const size_t use = w.k * w.n * m < (4u << 20) ? 1 : std::min(threads, w.n);
  std::vector<float> sums;
  for (size_t r = 0; r < m; ++r) {
    if (use == 1) rows(w, x + r * w.k, y + r * w.n, 0, w.n);
    else pool->run(rows, w, x + r * w.k, y + r * w.n, use);
    if (w.minimums.empty()) continue;
    const size_t parts = w.k / 32;
    sums.assign(parts, 0.0f);
    for (size_t b = 0; b < parts; ++b)
      for (size_t i = 0; i < 32; ++i) sums[b] += x[r * w.k + b * 32 + i];
    for (size_t j = 0; j < w.n; ++j) {
      const float *minimum = w.minimums.data() + j * parts;
      float total = 0;
      for (size_t b = 0; b < parts; ++b) total += minimum[b] * sums[b];
      y[r * w.n + j] -= total;
    }
  }
}

// One session with the executor, shared by this process's launches.
constexpr size_t Q8_SHARED_REGION = 64u << 20, Q8_SHARED_REGIONS = 4;
constexpr uint32_t Q8_NO_PAIR = 0xffffffff;
// EXPERIMENT (SHIELD_OPENCL_Q8_MAILBOX=1, and SHIELD_Q8_MAILBOX=1 for the executor): the executor's request
// slot in the last bytes of the shared memory (its Mail). A request and its answer pass through it without a
// system call while the executor is awake. SHIELD_OPENCL_Q8_CPU pins the thread that makes the requests, to
// the other hardware thread of the core the executor's serving thread is on.
struct Q8Mail {
  std::atomic<uint32_t> request, reply, sleeping;
  uint32_t id, m, pair;
  uint32_t status, paired;
  uint64_t exchanges, macs;
  uint32_t third, reserved;  // third: a second partner's slot plus one, 0 for none
};
struct Q8Session {
  int fd = -1;
  uint64_t serial = 0;  // changes with every connection; slots belong to one
  std::string engine;
  std::map<std::string, uint32_t> slots;  // tensor name -> slot opened in this session
  // Payload memory the executor created for this session: activation, product,
  // and the product of a paired tensor. Null when it could not provide one.
  unsigned char *shared = nullptr;
  Q8Mail *mail = nullptr;
  bool pinned = false;
  // Two tensors applied to the same activation one after the other (a
  // feed-forward gate and up projection) can share a round trip: the second
  // product is requested with the first and used only if the next launch asks
  // for exactly that tensor with exactly the same activation bytes.
  std::vector<uint32_t> pair_of;           // slot -> slot seen next with the same activation
  std::vector<unsigned char> last_input;   // activation of the previous single-request launch
  uint32_t last_slot = Q8_NO_PAIR;
  size_t last_m = 0;
  bool last_valid = false, ready_valid = false;
  uint64_t requests = 0, paired = 0, served = 0, learned = 0, missed = 0;  // counts for the timing report
  uint32_t ready_slot = Q8_NO_PAIR;        // paired product waiting in the third region
  // A third tensor on the same activation (attention's q, k and v) can ride with the pair: it is learned
  // when it follows a paired product that was just served, and its product waits in the fourth region.
  std::vector<uint32_t> third_of;          // a pair's first slot -> the slot seen after its partner
  uint32_t first_slot = Q8_NO_PAIR, ready2_slot = Q8_NO_PAIR;
  bool ready2_valid = false, partner_served = false;
  void reset() {
    if (fd >= 0) close(fd);
    fd = -1;
    serial = 0;
    if (shared) munmap(shared, Q8_SHARED_REGIONS * Q8_SHARED_REGION);
    shared = nullptr;
    mail = nullptr;
    slots.clear();
    pair_of.clear();
    third_of.clear();
    last_valid = ready_valid = ready2_valid = partner_served = false;
  }
};
std::mutex q8_direct_mutex;
Q8Session q8_session;
// A forked child must not write frames into its parent's session. It drops its
// copies of the socket and mapping and opens a session of its own if it needs one.
const int q8_fork_guard = pthread_atfork(nullptr, nullptr, [] {
  if (q8_session.fd >= 0) close(q8_session.fd);
  q8_session.fd = -1;
  q8_session.reset();
});
struct Q8Frame {
  uint32_t type, id, a, b;
};
constexpr uint32_t Q8_OPEN = 1, Q8_MATMUL = 2, Q8_SHARE = 3, Q8_MATMUL_SHARED = 4, Q8_MAIL = 5, Q8_WAKE = 6, Q8_BYE = 0xff;
struct Q8Bye {};  // the executor stopped between requests; nothing was computed
struct Q8SendFailed {};
// After a failed send: whether the executor had already said goodbye. It
// closes right after that frame, so a request written into the closing socket
// was never read and may go to a new session.
bool q8_said_goodbye(int fd) {
  struct { uint32_t type, id, a, b; } frame{};
  ssize_t received;
  do received = recv(fd, &frame, sizeof(frame), MSG_DONTWAIT);
  while (received < 0 && errno == EINTR);
  return received == ssize_t(sizeof(frame)) && frame.type == 0xff;
}

void q8_send(int fd, const void *a, size_t an, const void *b = nullptr, size_t bn = 0) {
  iovec parts[2] = {{const_cast<void *>(a), an}, {const_cast<void *>(b), bn}};
  size_t first = 0;
  while (first < 2) {
    msghdr message{};
    message.msg_iov = parts + first;
    message.msg_iovlen = 2 - first;
    ssize_t sent = sendmsg(fd, &message, MSG_NOSIGNAL);
    if (sent < 0 && errno == EINTR) continue;
    if (sent <= 0) throw Q8SendFailed{};
    while (first < 2 && size_t(sent) >= parts[first].iov_len) sent -= parts[first++].iov_len;
    if (first < 2) {
      parts[first].iov_base = static_cast<char *>(parts[first].iov_base) + sent;
      parts[first].iov_len -= sent;
    }
  }
}
// A reply usually arrives within the time a blocked thread takes to wake, so
// look for it briefly first. SHIELD_OPENCL_Q8_SPIN_US bounds that; 0 sleeps.
uint64_t q8_spin_ns() {
  static const uint64_t value = [] {
    const char *text = std::getenv("SHIELD_OPENCL_Q8_SPIN_US");
    return std::min<uint64_t>(text ? std::strtoull(text, nullptr, 10) : 0, 10000) * 1000;
  }();
  return value;
}
void q8_receive(int fd, void *out, size_t n, bool spin = false) {
  auto *p = static_cast<char *>(out);
  if (spin && q8_spin_ns()) {
    const uint64_t until = Q8Timing::now() + q8_spin_ns();
    do {
      ssize_t r = recv(fd, p, n, MSG_DONTWAIT);
      if (r > 0) { p += r; n -= r; break; }
      if (r == 0 || (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
        throw std::runtime_error("Shield session closed");
      __builtin_ia32_pause();
    } while (Q8Timing::now() < until);
  }
  while (n) {
    ssize_t r = recv(fd, p, n, 0);
    if (r < 0 && errno == EINTR) continue;
    if (r <= 0) throw std::runtime_error("Shield session closed");
    p += r;
    n -= r;
  }
}
Q8Frame q8_reply(int fd, uint32_t expected, uint32_t id) {
  Q8Frame frame{};
  q8_receive(fd, &frame, sizeof(frame), true);
  if (frame.type == Q8_BYE) throw Q8Bye{};
  if (frame.type != expected || frame.id != id) throw std::runtime_error("Shield session reply");
  return frame;
}
void q8_connect(const std::string &engine) {
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::minutes(20);
  const auto request = json{{"op", "q8_session"}, {"engine", engine},
                            {"arithmetic", "shield-fixed-point-v1"}}.dump();
  for (;;) {
    Fd socket_fd(socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0));
    if (socket_fd.value < 0) throw std::runtime_error("socket failed");
    timeval timeout{1200, 0};
    setsockopt(socket_fd.value, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    setsockopt(socket_fd.value, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    std::strcpy(address.sun_path, "/run/shield/compute.sock");
    bool busy = false;
    if (connect(socket_fd.value, reinterpret_cast<sockaddr *>(&address), sizeof(address))) {
      if (errno != EAGAIN) throw std::runtime_error("Shield service unavailable");
      busy = true;
    } else {
      uint32_t size = htonl(request.size());
      try {
        q8_send(socket_fd.value, &size, sizeof(size), request.data(), request.size());
      } catch (const Q8SendFailed &) {
        throw std::runtime_error("Shield send failed");
      }
      q8_receive(socket_fd.value, &size, sizeof(size));
      size = ntohl(size);
      if (!size || size > 4096) throw std::runtime_error("response limit");
      std::string response(size, '\0');
      q8_receive(socket_fd.value, response.data(), size);
      auto result = json::parse(response);
      if (result.value("ok", false)) {
        q8_session.reset();
        static uint64_t next_serial = 0;
        q8_session.serial = ++next_serial;
        q8_session.fd = socket_fd.value;
        socket_fd.value = -1;
        q8_session.engine = engine;
        // Ask for shared payload memory. Any refusal leaves payloads on the socket.
        if (q8_direct_shared_enabled()) {
          const Q8Frame share{Q8_SHARE, 0, 0, 0};
          try {
            q8_send(q8_session.fd, &share, sizeof(share));
          } catch (const Q8SendFailed &) {
            throw std::runtime_error("Shield session send failed");
          }
          Q8Frame reply{};
          char control[CMSG_SPACE(sizeof(int))];
          iovec part{&reply, sizeof(reply)};
          msghdr message{};
          message.msg_iov = &part;
          message.msg_iovlen = 1;
          message.msg_control = control;
          message.msg_controllen = sizeof(control);
          ssize_t received;
          do received = recvmsg(q8_session.fd, &message, MSG_CMSG_CLOEXEC | MSG_WAITALL);
          while (received < 0 && errno == EINTR);
          int shared_fd = -1;
          for (cmsghdr *c = CMSG_FIRSTHDR(&message); received > 0 && c; c = CMSG_NXTHDR(&message, c))
            if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS && c->cmsg_len == CMSG_LEN(sizeof(int)))
              std::memcpy(&shared_fd, CMSG_DATA(c), sizeof(int));
          Fd holder(shared_fd);
          if (received != ssize_t(sizeof(reply)) || reply.type != Q8_SHARE)
            throw std::runtime_error("Shield session reply");
          if (reply.a == 0 && reply.b == Q8_SHARED_REGION && shared_fd >= 0) {
            void *base = mmap(nullptr, Q8_SHARED_REGIONS * Q8_SHARED_REGION, PROT_READ | PROT_WRITE, MAP_SHARED, shared_fd, 0);
            if (base != MAP_FAILED) q8_session.shared = static_cast<unsigned char *>(base);
          }
          if (q8_session.shared && std::getenv("SHIELD_OPENCL_Q8_MAILBOX")) {
            const Q8Frame ask{Q8_MAIL, 0, 0, 0};
            try {
              q8_send(q8_session.fd, &ask, sizeof(ask));
            } catch (const Q8SendFailed &) {
              throw std::runtime_error("Shield session send failed");
            }
            Q8Frame answer{};
            q8_receive(q8_session.fd, &answer, sizeof(answer));
            if (answer.type == Q8_MAIL && answer.a == 0)
              q8_session.mail = reinterpret_cast<Q8Mail *>(q8_session.shared + Q8_SHARED_REGIONS * Q8_SHARED_REGION - sizeof(Q8Mail));
          }
        }
        return;
      }
      // Only admission contention is retryable, as for the queued route.
      if (result.value("error", "") != "compute busy") throw std::runtime_error("Shield computation refused");
      busy = true;
    }
    if (busy && std::chrono::steady_clock::now() >= deadline) throw std::runtime_error("Shield computation refused");
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
  }
}
// Tell the service, once, that this process has opted into masked Q8 and is
// still building its programs. The service can start its executor meanwhile.
// A hint only: failures are ignored and nothing waits for it.
void q8_warm_once() {
  static std::once_flag once;
  std::call_once(once, [] {
    const char *contract = std::getenv("SHIELD_OPENCL_ARITHMETIC");
    if (!contract || std::strcmp(contract, "shield-fixed-point-v1") || !q8_direct_enabled()) return;
    try {
      std::thread([] {
        try {
          Fd socket_fd(socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0));
          if (socket_fd.value < 0) return;
          timeval timeout{30, 0};
          setsockopt(socket_fd.value, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
          setsockopt(socket_fd.value, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
          sockaddr_un address{};
          address.sun_family = AF_UNIX;
          std::strcpy(address.sun_path, "/run/shield/compute.sock");
          if (connect(socket_fd.value, reinterpret_cast<sockaddr *>(&address), sizeof(address))) return;
          const std::string request = R"({"op":"q8_warm","arithmetic":"shield-fixed-point-v1"})";
          uint32_t size = htonl(request.size());
          q8_send(socket_fd.value, &size, sizeof(size), request.data(), request.size());
          char reply[256];
          (void)!recv(socket_fd.value, reply, sizeof(reply), 0);
        } catch (...) {
        }
      }).detach();
    } catch (...) {
    }
  });
}
uint32_t q8_slot(const Q8Identity &weights, size_t k, size_t n) {
  // Only launches holding q8_direct_mutex read or write the cached slot.
  if (weights.session_serial == q8_session.serial) return weights.slot;
  const auto remember = [&](uint32_t id) {
    weights.session_serial = q8_session.serial;
    weights.slot = id;
    return id;
  };
  const auto &name = weights.tensor;
  auto it = q8_session.slots.find(name);
  if (it != q8_session.slots.end()) return remember(it->second);
  const auto &digest = weights.sha256;
  if (digest.size() != 64 || name.empty() || name.size() > 127 || q8_session.slots.size() >= 4096)
    throw std::runtime_error("Q8 tensor reference");
  const uint32_t id = q8_session.slots.size();
  struct { Q8Frame frame; uint64_t k, n; char sha[64]; } open{{Q8_OPEN, id, uint32_t(name.size()), 0}, k, n, {}};
  std::memcpy(open.sha, digest.data(), 64);
  q8_send(q8_session.fd, &open, sizeof(open), name.data(), name.size());
  if (q8_reply(q8_session.fd, Q8_OPEN, id).a != 0) throw std::runtime_error("Q8 tensor refused");
  q8_session.slots.emplace(name, id);
  return remember(id);
}
// One chunk. False means the executor declined it before computing anything.
bool q8_exchange(const Q8Identity &weights, size_t k, size_t n, size_t m, const unsigned char *input,
                 unsigned char *output) {
  for (int attempt = 0;; ++attempt) {
    try {
      const auto &engine = weights.engine;
      if (q8_session.fd < 0 || q8_session.engine != engine) q8_connect(engine);
      const uint32_t id = q8_slot(weights, k, n);
      const Q8Frame frame{Q8_MATMUL, id, uint32_t(m), 0};
      q8_send(q8_session.fd, &frame, sizeof(frame), input, m * k * 4);
      const Q8Frame reply = q8_reply(q8_session.fd, Q8_MATMUL, id);
      uint64_t counters[2];
      q8_receive(q8_session.fd, counters, sizeof(counters));
      if (reply.a == 1) return false;
      if (reply.a != 0) throw std::runtime_error("Q8 session status");
      q8_receive(q8_session.fd, output, m * n * 4);
      return true;
    } catch (const Q8Bye &) {
      // A planned stop (another job took the worker, or executor turnover).
      // This request was not read, so it may be sent to a new session.
      q8_session.reset();
      if (attempt >= 8) throw std::runtime_error("Shield session unavailable");
    } catch (const Q8SendFailed &) {
      const bool goodbye = q8_said_goodbye(q8_session.fd);
      q8_session.reset();
      if (!goodbye || attempt >= 8) throw std::runtime_error("Shield session send failed");
    } catch (...) {
      q8_session.reset();
      throw;
    }
  }
}

// A whole launch in one request through shared memory. Returns the product's
// bytes (valid until the next request), or null if the executor declined it
// before computing anything. nullopt: no shared memory, use q8_exchange.
std::optional<const unsigned char *> q8_exchange_shared(const Q8Identity &weights, size_t k, size_t n, size_t m,
                                                        std::vector<unsigned char> &input) {
  Q8Session &session = q8_session;
  for (int attempt = 0;; ++attempt) {
    try {
      const auto &engine = weights.engine;
      if (session.fd < 0 || session.engine != engine) q8_connect(engine);
      if (!session.shared) return std::nullopt;
      const uint32_t id = q8_slot(weights, k, n);
      const bool same_input = session.last_valid && session.last_m == m &&
                              session.last_input.size() == input.size() &&
                              !std::memcmp(session.last_input.data(), input.data(), input.size());
      const bool after_partner = session.partner_served;
      session.partner_served = false;
      if (session.ready_valid) {
        session.ready_valid = false;
        if (session.ready_slot == id && same_input) {
          ++session.served;
          // A third tensor on the same activation (attention's q, k and v)
          // follows this one, not the tensor the pair was requested with:
          // learning it there would replace the pair just served.
          session.last_slot = id;
          session.partner_served = true;
          return session.shared + 2 * Q8_SHARED_REGION;
        }
        ++session.missed;
        session.ready2_valid = false;
        // The expected launch did not follow; stop asking for that pair.
        if (session.last_slot < session.pair_of.size()) session.pair_of[session.last_slot] = Q8_NO_PAIR;
      } else if (session.ready2_valid) {
        session.ready2_valid = false;
        if (session.ready2_slot == id && same_input) {
          ++session.served;
          session.last_slot = id;
          return session.shared + 3 * Q8_SHARED_REGION;
        }
        ++session.missed;
        if (session.first_slot < session.third_of.size()) session.third_of[session.first_slot] = Q8_NO_PAIR;
      } else if (same_input && session.last_slot != id) {
        if (after_partner && session.first_slot != Q8_NO_PAIR && session.first_slot != id &&
            std::getenv("SHIELD_OPENCL_Q8_TRIPLE")) {
          if (session.third_of.size() <= session.first_slot) session.third_of.resize(session.first_slot + 1, Q8_NO_PAIR);
          session.third_of[session.first_slot] = id;
        } else {
          if (session.pair_of.size() <= session.last_slot) session.pair_of.resize(session.last_slot + 1, Q8_NO_PAIR);
          session.pair_of[session.last_slot] = id;
        }
        ++session.learned;
      }
      const uint32_t pair = id < session.pair_of.size() ? session.pair_of[id] : Q8_NO_PAIR;
      const uint32_t third = pair != Q8_NO_PAIR && id < session.third_of.size() ? session.third_of[id] : Q8_NO_PAIR;
      session.first_slot = id;
      std::memcpy(session.shared, input.data(), input.size());
      Q8Frame reply{};
      if (session.mail) {
        Q8Mail *mail = session.mail;
        if (!session.pinned) {
          session.pinned = true;
          if (const char *cpu = std::getenv("SHIELD_OPENCL_Q8_CPU")) {
            cpu_set_t set;
            CPU_ZERO(&set);
            CPU_SET(std::atoi(cpu), &set);
            sched_setaffinity(0, sizeof(set), &set);  // the calling thread
          }
        }
        mail->id = id;
        mail->m = uint32_t(m);
        mail->pair = pair;
        mail->third = third == Q8_NO_PAIR ? 0 : third + 1;
        const uint32_t mine = mail->request.load(std::memory_order_relaxed) + 1;
        mail->request.store(mine, std::memory_order_seq_cst);
        if (mail->sleeping.load(std::memory_order_seq_cst)) {
          const Q8Frame wake{Q8_WAKE, 0, 0, 0};
          q8_send(session.fd, &wake, sizeof(wake));
        }
        // The executor closes the socket when it stops; look now and then so that this does not wait for ever.
        for (uint64_t turn = 1; mail->reply.load(std::memory_order_acquire) != mine; ++turn) {
          _mm_pause();
          if ((turn & 0xfffff) == 0) {
            Q8Frame gone{};
            const ssize_t got = recv(session.fd, &gone, sizeof(gone), MSG_PEEK | MSG_DONTWAIT);
            if (got == 0 || (got == ssize_t(sizeof(gone)) && gone.type == Q8_BYE)) throw std::runtime_error("Shield session closed");
          }
        }
        reply = {Q8_MATMUL_SHARED, id, mail->status, mail->paired};
      } else {
        struct { Q8Frame frame; uint32_t pair, reserved; } request{{Q8_MATMUL_SHARED, id, uint32_t(m), 0}, pair,
                                                                    third == Q8_NO_PAIR ? 0 : third + 1};
        q8_send(session.fd, &request, sizeof(request));
        reply = q8_reply(session.fd, Q8_MATMUL_SHARED, id);
        uint64_t counters[2];
        q8_receive(session.fd, counters, sizeof(counters));
      }
      session.last_valid = false;
      if (reply.a == 1) return nullptr;
      if (reply.a != 0) throw std::runtime_error("Q8 session status");
      session.last_slot = id;
      session.last_m = m;
      session.last_input.swap(input);
      session.last_valid = true;
      ++session.requests;
      session.ready_valid = (reply.b & 1) && pair != Q8_NO_PAIR;
      session.paired += session.ready_valid;
      session.ready_slot = pair;
      session.ready2_valid = session.ready_valid && (reply.b & 2) && third != Q8_NO_PAIR;
      session.ready2_slot = third;
      return session.shared + Q8_SHARED_REGION;
    } catch (const Q8Bye &) {
      session.reset();
      if (attempt >= 8) throw std::runtime_error("Shield session unavailable");
    } catch (const Q8SendFailed &) {
      const bool goodbye = q8_said_goodbye(session.fd);
      session.reset();
      if (!goodbye || attempt >= 8) throw std::runtime_error("Shield session send failed");
    } catch (...) {
      session.reset();
      throw;
    }
  }
}

struct Q8DirectLaunch {
  cl_command_queue queue;
  int kind;                            // Q8Kind
  std::array<cl_mem, 7> buffers;       // the weight buffers, then activation and output
  std::array<Storage, 7> storage;
  std::array<size_t, 6> read_bytes;    // the weight buffers, then the activation
  size_t weight_offset;                // where the weights start in their buffer (the plain IQ4_XS kernel)
  unsigned weights() const { return q8_kind_weights(kind); }
  size_t input_offset, output_offset, output_bytes, k, n, m;
  cl_event *event;
};

// The launch's event, when the application asked for one. A failed launch is
// reported the way the queued route reports it: the enqueue succeeds and its
// event completes with an error. Without an event the error is returned.
cl_int q8_direct_result(const Q8DirectLaunch &launch, cl_int status) {
  if (!launch.event) return status;
  cl_event done = nullptr;
  if (status == CL_SUCCESS) {
    if (REAL(clEnqueueMarkerWithWaitList)(launch.queue, 0, nullptr, &done)) return CL_OUT_OF_RESOURCES;
  } else {
    cl_context context = nullptr;
    cl_int error = 0;
    if (REAL(clGetCommandQueueInfo)(launch.queue, CL_QUEUE_CONTEXT, sizeof(context), &context, nullptr))
      return status;
    done = REAL(clCreateUserEvent)(context, &error);
    if (!done) return status;
    REAL(clSetUserEventStatus)(done, status);
  }
  {
    std::lock_guard lock(mutex);
    offload_events[done] = {1, nullptr};
  }
  *launch.event = done;
  return CL_SUCCESS;
}

// The product computed in this process. False: not available (disabled, the
// weights could not be remembered, or the copy would exceed the limit); the
// caller then runs the original kernel.
// A K-quant tensor's flat buffers, read back and decoded for a local product
// (q8_kquant.inc has the decoding).
bool q8_kquant_local_weights(const Q8DirectLaunch &launch, Q8Local &local) {
  const unsigned count = launch.weights();
  const bool q6 = launch.kind == Q6K_MM || launch.kind == Q6K_MV;
  std::array<std::vector<unsigned char>, 5> raw;
  for (unsigned i = 0; i < count; ++i) {
    raw[i].resize(launch.read_bytes[i]);
    if (REAL(clEnqueueReadBuffer)(launch.queue, launch.buffers[i], CL_TRUE, launch.weight_offset,
                                  launch.read_bytes[i], raw[i].data(), 0, nullptr, nullptr))
      return false;
  }
  const size_t blocks = launch.k * launch.n / 256;
  local.codes.resize(launch.k * launch.n);
  if (launch.kind == IQ4NL_MM || launch.kind == IQ4NL_MV) {
    local.block = 32;
    local.scales.resize(launch.k * launch.n / 32);
    q8_decode_iq4_nl(raw[0].data(), raw[1].data(), launch.k * launch.n / 32, local.codes.data(),
                     local.scales.data());
  } else if (launch.kind == Q3K_RAW) {
    local.block = 16;
    local.scales.resize(blocks * 16);
    q8_decode_q3_k(raw[0].data(), blocks, local.codes.data(), local.scales.data());
  } else if (launch.kind == IQ3S_RAW) {
    local.block = 32;
    local.scales.resize(blocks * 8);
    q8_decode_iq3_s(raw[0].data(), blocks, local.codes.data(), local.scales.data());
  } else if (count == 1) {
    local.block = 32;
    local.scales.resize(blocks * 8);
    q8_decode_iq4_xs(raw[0].data(), blocks, local.codes.data(), local.scales.data());
  } else if (q6) {
    local.block = 16;
    local.scales.resize(blocks * 16);
    q8_decode_q6_k(raw[0].data(), raw[1].data(), raw[2].data(), raw[3].data(), blocks, local.codes.data(),
                   local.scales.data());
  } else {
    const bool q5 = count == 5;
    local.block = 32;
    local.scales.resize(blocks * 8);
    local.minimums.resize(blocks * 8);
    q8_decode_q45_k(raw[0].data(), q5 ? raw[1].data() : nullptr, raw[q5 ? 2 : 1].data(), raw[q5 ? 3 : 2].data(),
                    raw[q5 ? 4 : 3].data(), blocks, local.codes.data(), local.scales.data(), local.minimums.data());
  }
  return true;
}

bool q8_local(const Q8DirectLaunch &launch, const Q8Identity &identity, bool remembered,
              const std::vector<unsigned char> &input, std::vector<unsigned char> &output) {
  // Everything but Q8_0 is decoded from its flat buffers.
  const bool k_quant = launch.kind != Q8_MM && launch.kind != Q8_MV;
  if (!q8_local_enabled() || !remembered || identity.local_unavailable) return false;
  if (!identity.local) {
    const size_t bytes = k_quant ? launch.k * launch.n + launch.k * launch.n / 4
                                 : launch.read_bytes[0] + launch.read_bytes[1] * 2;
    if (q8_local_bytes.load() + bytes > Q8_LOCAL_LIMIT) return false;
    try {
      auto local = std::make_shared<Q8Local>();
      local->k = launch.k;
      local->n = launch.n;
      if (k_quant) {
        if (!q8_kquant_local_weights(launch, *local)) {
          identity.local_unavailable = true;
          return false;
        }
      } else {
        local->codes.resize(launch.read_bytes[0]);
        std::vector<uint16_t> halves(launch.read_bytes[1] / 2);
        if (REAL(clEnqueueReadBuffer)(launch.queue, launch.buffers[0], CL_TRUE, 0, launch.read_bytes[0],
                                      local->codes.data(), 0, nullptr, nullptr) ||
            REAL(clEnqueueReadBuffer)(launch.queue, launch.buffers[1], CL_TRUE, 0, launch.read_bytes[1],
                                      halves.data(), 0, nullptr, nullptr)) {
          identity.local_unavailable = true;
          return false;
        }
        local->scales.resize(halves.size());
        for (size_t i = 0; i < halves.size(); ++i) local->scales[i] = q8_half_to_float(halves[i]);
      }
      q8_local_bytes += bytes;
      local->charged = bytes;
      identity.local = std::move(local);
    } catch (...) {
      identity.local_unavailable = true;
      return false;
    }
  }
  output.resize(launch.output_bytes);
  q8_local_product(*identity.local, reinterpret_cast<const float *>(input.data()),
                   reinterpret_cast<float *>(output.data()), launch.m);
  return true;
}

// Keeps an authenticated tensor's identity for later launches. False when the
// buffers can change without a call this library sees (host-pointer buffers,
// buffers mapped for writing): those are authenticated on every launch.
bool q8_remember(const std::array<Q8Region, 5> &regions, const std::array<cl_mem_flags, 5> &root_flags,
                 unsigned count, size_t k, size_t n, const std::shared_ptr<const Q8Identity> &identity) {
  std::lock_guard lock(q8_cache_mutex);
  bool stable = !q8_untracked_writers.load();
  for (unsigned w = 0; w < count; ++w) {
    if (root_flags[w] & CL_MEM_USE_HOST_PTR) stable = false;
    for (const auto &[pointer, root] : q8_write_maps)
      if (root == regions[w].root) stable = false;
  }
  if (!stable || q8_known.size() >= 8192) return false;
  unsigned retained = 0;
  while (retained < count && !REAL(clRetainMemObject)(regions[retained].root)) ++retained;
  if (retained != count) {
    while (retained) REAL(clReleaseMemObject)(regions[--retained].root);
    return false;
  }
  q8_known.push_back({regions, count, k, n, identity});
  q8_known_count.store(q8_known.size(), std::memory_order_release);
  return true;
}

// Authentication ahead of the first product.
//
// A tensor's first launch waits for its weights to be read back and hashed:
// seconds for a small model and a quarter of a minute for a 27B one, spent
// after the executor is ready. The same bytes are in place as soon as the
// application has loaded them, while the executor is still starting. So when
// the application writes or converts what could be a catalogued tensor, it is
// authenticated then, exactly as a launch would do it, and remembered under
// the same write tracking. A guess that is wrong costs the time only: nothing
// is remembered and the first launch authenticates as before.
struct Q8Expected {
  bool usable = false;
  std::set<std::pair<int, size_t>> flat;  // (type, weights) of tensors kept as several flat buffers
  std::map<size_t, std::vector<int>> raw; // bytes -> types of tensors kept as GGUF bytes
};
const Q8Expected &q8_expected() {
  static const Q8Expected expected = [] {
    Q8Expected result;
    const char *contract = std::getenv("SHIELD_OPENCL_ARITHMETIC");
    const char *enabled = std::getenv("SHIELD_OPENCL_Q8_PREAUTH");
    if (!contract || std::strcmp(contract, "shield-fixed-point-v1") || !q8_direct_enabled() ||
        (enabled && !std::strcmp(enabled, "0")))
      return result;
    try {
      for (const auto &tensor : q8_catalog()) {
        const size_t weights = tensor.k * tensor.n;
        if (tensor.type == 23) result.raw[weights / 256 * 136].push_back(23);
        else if (tensor.type == 11 || tensor.type == 21) result.raw[weights / 256 * 110].push_back(tensor.type);
        else result.flat.insert({tensor.type, weights});
      }
      result.usable = !result.flat.empty() || !result.raw.empty();
    } catch (...) {
      result = {};
    }
    return result;
  }();
  return expected;
}
std::atomic<uint64_t> q8_preauth_tensors{0}, q8_preauth_misses{0}, q8_preauth_ns{0};
// An application whose buffers only look like tensors stops being read for it.
constexpr uint64_t kPreauthMisses = 64;
struct Q8PreauthReport {
  ~Q8PreauthReport() {
    if (q8_timing.enabled)
      std::fprintf(stderr, "[shield-opencl-timing] authenticated ahead: tensors=%llu not catalogued=%llu ms=%.1f\n",
                   (unsigned long long)q8_preauth_tensors.load(), (unsigned long long)q8_preauth_misses.load(),
                   q8_preauth_ns.load() / 1e6);
  }
} q8_preauth_report;
void q8_preauth_answer(const json &answer, const std::array<Q8Region, 5> &regions,
                       const std::array<cl_mem_flags, 5> &root_flags, unsigned count) {
  if (!answer.at("approved").get<bool>()) {
    ++q8_preauth_misses;
    return;
  }
  auto identity = std::make_shared<Q8Identity>();
  identity->approved = true;
  identity->engine = answer.at("engine").get<std::string>();
  identity->tensor = answer.at("tensor").get<std::string>();
  identity->sha256 = answer.at("sha256").get<std::string>();
  if (q8_remember(regions, root_flags, count, answer.at("k").get<size_t>(), answer.at("n").get<size_t>(), identity))
    ++q8_preauth_tensors;
}
// After a kernel that is not a product has run: were some of its buffers,
// taken in argument order, the flat buffers of a catalogued tensor?
void q8_preauthenticate_kernel(cl_command_queue queue, cl_kernel kernel) {
  const Q8Expected &expected = q8_expected();
  if (!expected.usable || expected.flat.empty() || q8_preauth_misses.load() > kPreauthMisses) return;
  // Every pointer-sized argument, in argument order. Only those that are
  // sub-buffers this library saw created are looked at: any other may be a
  // number.
  static thread_local std::vector<std::pair<cl_uint, cl_mem>> arguments;
  {
    std::lock_guard lock(mutex);
    const auto it = kernels.find(kernel);
    if (it == kernels.end() || it->second.mems.size() < 2) return;
    arguments = it->second.mems;
  }
  std::sort(arguments.begin(), arguments.end());
  std::array<cl_mem, 22> mems{};
  std::array<size_t, 22> sizes{};
  unsigned found = 0;
  {
    std::lock_guard lock(q8_cache_mutex);
    if (q8_children.empty()) return;
    for (const auto &[index, handle] : arguments) {
      const auto it = q8_children.find(handle);
      if (it == q8_children.end() || found == mems.size()) continue;
      mems[found] = handle;
      sizes[found++] = it->second.size;
    }
  }
  if (found < 2) return;
  // Type, weights to a unit, then the bytes of each flat buffer for a unit.
  struct Layout { int type; unsigned unit, count; unsigned bytes[5]; };
  static const Layout layouts[] = {{13, 256, 5, {128, 32, 12, 2, 2}}, {12, 256, 4, {128, 12, 2, 2, 0}},
                                   {14, 256, 4, {128, 64, 16, 2, 0}}, {8, 32, 2, {32, 2, 0, 0, 0}},
                                   {20, 32, 2, {16, 2, 0, 0, 0}}};
  for (unsigned first = 0; first + 2 <= found; ++first) for (const Layout &layout : layouts) {
    if (first + layout.count > found || !sizes[first] || sizes[first] % layout.bytes[0]) continue;
    const size_t units = sizes[first] / layout.bytes[0];
    bool fits = expected.flat.count({layout.type, units * layout.unit}) != 0;
    for (unsigned w = 1; fits && w < layout.count; ++w) fits = sizes[first + w] == units * layout.bytes[w];
    if (!fits) continue;
    const uint64_t start = Q8Timing::now();
    std::array<Q8Region, 5> regions{};
    std::array<cl_mem_flags, 5> root_flags{};
    std::array<std::vector<unsigned char>, 5> bytes;
    if (REAL(clFinish)(queue)) return;
    for (unsigned w = 0; w < layout.count; ++w) {
      Storage stored;
      if (!storage(mems[first + w], stored) || stored.size != sizes[first + w]) return;
      regions[w] = {stored.root, stored.offset, stored.size};
      root_flags[w] = stored.root_flags;
      bytes[w].resize(stored.size);
      if (REAL(clEnqueueReadBuffer)(queue, mems[first + w], CL_TRUE, 0, stored.size, bytes[w].data(), 0, nullptr,
                                    nullptr))
        return;
    }
    try {
      const size_t k = layout.unit, n = units;
      q8_any_shape = true;
      const json answer = layout.type == 13 ? q5k_authenticate(bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], k, n)
                          : layout.type == 12 ? q4k_authenticate(bytes[0], bytes[1], bytes[2], bytes[3], k, n)
                          : layout.type == 14 ? q6k_authenticate(bytes[0], bytes[1], bytes[2], bytes[3], k, n)
                          : layout.type == 20 ? iq4nl_authenticate(bytes[0], bytes[1], k, n)
                                              : q8_authenticate(bytes[0], bytes[1], k, n);
      q8_any_shape = false;
      q8_preauth_answer(answer, regions, root_flags, layout.count);
    } catch (...) {
      q8_any_shape = false;
    }
    q8_preauth_ns += Q8Timing::now() - start;
    return;
  }
}
// After the application has written `size` bytes from `pointer`: were they a
// catalogued tensor that is kept as its GGUF bytes?
void q8_preauthenticate_write(cl_command_queue queue, cl_mem buffer, size_t offset, size_t size,
                              const void *pointer) {
  const Q8Expected &expected = q8_expected();
  if (!expected.usable || !pointer || q8_preauth_misses.load() > kPreauthMisses) return;
  const auto types = expected.raw.find(size);
  if (types == expected.raw.end()) return;
  const uint64_t start = Q8Timing::now();
  Storage stored;
  // The write may still be queued; the bytes hashed are the ones written.
  if (REAL(clFinish)(queue) || !storage(buffer, stored) || offset > stored.size || size > stored.size - offset) return;
  try {
    Q8Hash hash;
    hash.update(static_cast<const unsigned char *>(pointer), size);
    const std::string digest = hash.finish();
    json answer = {{"approved", false}};
    q8_any_shape = true;
    for (size_t i = 0; i < types->second.size() && !answer.at("approved").get<bool>(); ++i) {
      const int type = types->second[i];
      answer = q8_catalog_answer(type, 256, size / (type == 23 ? 136 : 110), digest);
    }
    q8_any_shape = false;
    std::array<Q8Region, 5> regions{};
    std::array<cl_mem_flags, 5> root_flags{};
    regions[0] = {stored.root, stored.offset + offset, size};
    root_flags[0] = stored.root_flags;
    q8_preauth_answer(answer, regions, root_flags, 1);
  } catch (...) {
    q8_any_shape = false;
  }
  q8_preauth_ns += Q8Timing::now() - start;
}

// nullopt: not taken, the caller continues with CPU execution of the original
// kernel. Any other value is the launch's result; after a chunk has been sent,
// failures are reported and never retried on another route.
std::optional<cl_int> q8_direct(const Q8DirectLaunch &launch) {
  std::lock_guard direct(q8_direct_mutex);
  auto stage = Q8Timing::now();
  // Results of earlier commands must be in the buffers before they are read.
  if (REAL(clFinish)(launch.queue)) return std::nullopt;
  q8_timing.add(Q8Timing::Wait, stage);

  const unsigned count = launch.weights();
  std::array<Q8Region, 5> regions{};
  for (unsigned w = 0; w < count; ++w)
    regions[w] = {launch.storage[w].root, launch.storage[w].offset + launch.weight_offset, launch.read_bytes[w]};
  std::shared_ptr<const Q8Identity> identity;
  bool known = false;
  {
    std::lock_guard lock(q8_cache_mutex);
    for (const auto &entry : q8_known) {
      bool same = entry.count == count && entry.k == launch.k && entry.n == launch.n;
      for (unsigned w = 0; same && w < count; ++w)
        same = entry.weights[w].root == regions[w].root && entry.weights[w].offset == regions[w].offset &&
               entry.weights[w].size == regions[w].size;
      if (same) {
        identity = entry.identity;
        known = true;
        break;
      }
    }
  }
  if (!known) {
    stage = Q8Timing::now();
    std::array<std::vector<unsigned char>, 5> bytes;
    for (size_t i = 0; i < count; ++i) {
      bytes[i].resize(launch.read_bytes[i]);
      if (REAL(clEnqueueReadBuffer)(launch.queue, launch.buffers[i], CL_TRUE, launch.weight_offset,
                                    launch.read_bytes[i], bytes[i].data(), 0, nullptr, nullptr))
        return std::nullopt;
    }
    q8_timing.add(Q8Timing::Snapshot, stage);
    stage = Q8Timing::now();
    try {
      const bool q6 = launch.kind == Q6K_MM || launch.kind == Q6K_MV;
      const bool nl = launch.kind == IQ4NL_MM || launch.kind == IQ4NL_MV;
      const json answer = nl ? iq4nl_authenticate(bytes[0], bytes[1], launch.k, launch.n)
                          : count == 1 ? raw_authenticate(bytes[0], launch.k, launch.n,
                                                          launch.kind == Q3K_RAW ? 11 : launch.kind == IQ3S_RAW ? 21 : 23,
                                                          q8_raw_block(launch.kind))
                          : count == 5 ? q5k_authenticate(bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], launch.k,
                                                        launch.n)
                          : q6 ? q6k_authenticate(bytes[0], bytes[1], bytes[2], bytes[3], launch.k, launch.n)
                          : count == 4 ? q4k_authenticate(bytes[0], bytes[1], bytes[2], bytes[3], launch.k, launch.n)
                                       : q8_authenticate(bytes[0], bytes[1], launch.k, launch.n);
      auto fresh = std::make_shared<Q8Identity>();
      fresh->approved = answer.at("approved").get<bool>();
      if (fresh->approved) {
        fresh->engine = answer.at("engine").get<std::string>();
        fresh->tensor = answer.at("tensor").get<std::string>();
        fresh->sha256 = answer.at("sha256").get<std::string>();
      }
      identity = std::move(fresh);
    } catch (...) {
      return std::nullopt;
    }
    q8_timing.add(Q8Timing::Authenticate, stage);
    std::array<cl_mem_flags, 5> root_flags{};
    for (unsigned w = 0; w < count; ++w) root_flags[w] = launch.storage[w].root_flags;
    known = q8_remember(regions, root_flags, count, launch.k, launch.n, identity);
  }
  const Q8Identity &weights = *identity;
  constexpr size_t maximum = 2u << 20;
  const size_t rows = std::min({size_t(32), maximum / (launch.k * 4), maximum / (launch.n * 4)});

  static thread_local std::vector<unsigned char> input, output;
  stage = Q8Timing::now();
  input.resize(launch.read_bytes[count]);
  if (const unsigned char *host = host_bytes(launch.queue, launch.buffers[count], launch.input_offset, launch.read_bytes[count]))
    std::memcpy(input.data(), host, launch.read_bytes[count]);
  else if (REAL(clEnqueueReadBuffer)(launch.queue, launch.buffers[count], CL_TRUE, launch.input_offset,
                                     launch.read_bytes[count], input.data(), 0, nullptr, nullptr))
    return std::nullopt;
  q8_timing.add(Q8Timing::Snapshot, stage);
  stage = Q8Timing::now();
  const unsigned char *product = nullptr;
  // Not offloaded: weights that are not an approved tensor, a shape the
  // protocol cannot carry, or a width the executor has already declined.
  const uint64_t width = launch.m < 64 ? uint64_t(1) << launch.m : 0;
  // SHIELD_OPENCL_Q8_LOCAL_ONLY=1 offloads nothing: every product is computed
  // here or by its own kernel. For comparing the two; never set by a package.
  static const bool local_only = [] {
    const char *value = std::getenv("SHIELD_OPENCL_Q8_LOCAL_ONLY");
    return value && !std::strcmp(value, "1");
  }();
  bool local = local_only || !weights.approved || !rows || (weights.declined & width);
  try {
    if (!local && launch.m <= rows) {
      const auto shared = q8_exchange_shared(weights, launch.k, launch.n, launch.m, input);
      if (shared) {
        if (*shared) product = *shared;
        else local = true;
      }
    } else if (!local) {
      q8_session.last_valid = q8_session.ready_valid = false;
    }
    if (!product && !local) {
      output.resize(launch.output_bytes);
      for (size_t first = 0; first < launch.m; first += rows) {
        const size_t count = std::min(rows, launch.m - first);
        if (!q8_exchange(weights, launch.k, launch.n, count, input.data() + first * launch.k * 4,
                         output.data() + first * launch.n * 4)) {
          // Only a decline of the first chunk can select CPU.
          if (first) return q8_direct_result(launch, CL_OUT_OF_RESOURCES);
          local = true;
          break;
        }
      }
      if (!local) product = output.data();
    }
    if (local) {
      // Computed here if possible, otherwise by the original kernel.
      if (weights.approved && rows) weights.declined |= width;
      q8_session.last_valid = q8_session.ready_valid = false;
      if (!q8_local(launch, weights, known, input, output)) return std::nullopt;
      product = output.data();
      q8_timing.add(Q8Timing::Cpu, stage);
      stage = Q8Timing::now();
    }
    // The executor checks every product for non-finite values before it
    // releases it; a product computed here is checked now.
    if (local && !q8_all_finite(product, launch.output_bytes / 4)) throw std::runtime_error("Q8 nonfinite result");
  } catch (const std::exception &error) {
    // A fixed local reason only; never tensor names, shapes or values.
    if (q8_timing.enabled) std::fprintf(stderr, "[shield-opencl] direct route failed: %s\n", error.what());
    return q8_direct_result(launch, CL_OUT_OF_RESOURCES);
  } catch (...) {
    return q8_direct_result(launch, CL_OUT_OF_RESOURCES);
  }
  q8_timing.add(Q8Timing::Service, stage);
  stage = Q8Timing::now();
  // Every chunk has verified before any result reaches application memory.
  q8_written(launch.buffers[count + 1], launch.output_offset, launch.output_bytes, false);
  if (unsigned char *host = host_bytes(launch.queue, launch.buffers[count + 1], launch.output_offset, launch.output_bytes))
    std::memcpy(host, product, launch.output_bytes);
  else if (REAL(clEnqueueWriteBuffer)(launch.queue, launch.buffers[count + 1], CL_TRUE, launch.output_offset,
                                      launch.output_bytes, product, 0, nullptr, nullptr))
    return q8_direct_result(launch, CL_OUT_OF_RESOURCES);
  const cl_int result = q8_direct_result(launch, CL_SUCCESS);
  q8_timing.add(Q8Timing::Write, stage);
  if (q8_timing.enabled && !((q8_session.requests + q8_session.served) % 4096) && q8_session.requests)
    std::fprintf(stderr, "[shield-opencl-timing] shared requests=%llu paired=%llu served=%llu learned=%llu missed=%llu\n",
                 (unsigned long long)q8_session.requests, (unsigned long long)q8_session.paired,
                 (unsigned long long)q8_session.served, (unsigned long long)q8_session.learned,
                 (unsigned long long)q8_session.missed);
  return result;
}

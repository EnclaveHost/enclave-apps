// Persistent broker: original public Q8_0 GGUF tensor x private f32 activations
// through the existing ggml-shielded backend (shield-fixed-point-v1, approximate).
// The preloaded bootstrap authenticates the policy and installs the weight
// verifier and strict offload; this process re-checks the policy independently,
// hashes the mapped model bytes before any backend sees them, and speaks a
// line-delimited JSON + raw f32 protocol on stdin/stdout.
//
// With --control-fd N it additionally serves client sessions: the measured
// service checks a local client's credentials, then hands its connected Unix
// socket to this process over the control socket. A session speaks a small
// binary protocol directly, so an activation makes one guest-local hop instead
// of crossing the service's JSON/base64 framing. The backend, policy and model
// checks are the same ones the stdin protocol uses.
#include "ggml-backend.h"
#include "ggml-shielded.h"
#include "ggml.h"
#include "gguf.h"
#include "nlohmann/json.hpp"

#include <openssl/evp.h>
#include <algorithm>
#include <array>
#include <bit>
#include <cerrno>
#include <chrono>
#include <cmath>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <iterator>
#include <poll.h>
#include <sched.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <map>
#include <memory>
#include <sstream>
#include <stdexcept>
#include <atomic>
#include <condition_variable>
#include <exception>
#include <functional>
#include <mutex>
#include <string>
#include <thread>
#include <tuple>
#include <utility>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <vector>

static_assert(GGML_MAX_NAME == 128, "build with the SDK's GGML_MAX_NAME=128");
static_assert(std::endian::native == std::endian::little, "wire format is little-endian f32");
static_assert(sizeof(float) == 4);

namespace {

constexpr const char * kArithmetic = "shield-fixed-point-v1";
constexpr size_t kMaxPolicy = 16u << 20;
constexpr uint64_t kMaxModel = 64ull << 30;
constexpr size_t kMaxTensor = size_t(2) << 30;
constexpr size_t kMaxHeader = 8192;          // including the terminating '\n'
constexpr size_t kMaxPayload = 2u << 20;     // input and output, each
constexpr uint64_t kMaxRows = 32;
constexpr uint64_t kMaxRequests = 100000;

// Thrown for every refusal; never carries private values.
struct Retire : std::runtime_error {
    using std::runtime_error::runtime_error;
};
[[noreturn]] void retire(const char * why) { throw Retire(why); }

// Static reason only (no tensor values, names or sizes from the client).
void log_reason(const char * why) noexcept {
    std::fprintf(stderr, "[q8-executor] retired: %s\n", why);
}

std::string sha256_hex(const void * bytes, size_t size) {
    unsigned char out[EVP_MAX_MD_SIZE];
    unsigned count = 0;
    if (!EVP_Digest(bytes, size, out, &count, EVP_sha256(), nullptr) || count != 32)
        retire("SHA-256 unavailable");
    static const char hex[] = "0123456789abcdef";
    std::string result(64, '0');
    for (unsigned i = 0; i < 32; ++i) {
        result[2 * i] = hex[out[i] >> 4];
        result[2 * i + 1] = hex[out[i] & 15];
    }
    return result;
}

bool is_sha256_hex(const std::string & s) {
    if (s.size() != 64) return false;
    for (char c : s)
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
    return true;
}

const char * required_env(const char * key) {
    const char * value = std::getenv(key);
    if (!value || !*value) retire("required environment missing");
    return value;
}

// ---------------------------------------------------------------- policy

struct PolicyEntry {
    uint32_t type;
    std::array<int64_t, 4> ne;
    uint64_t nbytes;
    std::string sha256;
};
using Policy = std::map<std::string, PolicyEntry>;

// The weight types a product may be asked for: Q8_0, and Q4_K, Q5_K, Q6_K or
// IQ4_XS whose rows are whole 256-weight super-blocks. Sessions only; the request protocol on stdin
// stays Q8_0, which the service checks before it forwards anything.
bool lowered_weight(uint32_t type, int64_t k) {
    return k > 0 && ((type == GGML_TYPE_Q8_0 && k % 32 == 0) ||
                     ((type == GGML_TYPE_Q4_K || type == GGML_TYPE_Q5_K || type == GGML_TYPE_Q6_K ||
                       type == GGML_TYPE_IQ4_XS) &&
                      k % 256 == 0));
}

std::string read_policy_file(const char * path) {
    const int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) retire("policy open failed");
    std::string bytes;
    struct stat st{};
    bool ok = fstat(fd, &st) == 0 && S_ISREG(st.st_mode) && st.st_size > 0 &&
              (uint64_t)st.st_size <= kMaxPolicy;
    if (ok) {
        bytes.resize((size_t)st.st_size);
        size_t done = 0;
        while (ok && done < bytes.size()) {
            const ssize_t r = read(fd, bytes.data() + done, bytes.size() - done);
            if (r < 0 && errno == EINTR) continue;
            if (r <= 0) ok = false;
            else done += (size_t)r;
        }
    }
    close(fd);
    if (!ok) retire("policy read failed");
    return bytes;
}

Policy load_policy() {
    if (std::strcmp(required_env("ENCLAVE_SHIELD_ARITHMETIC"), kArithmetic))
        retire("explicit shield-fixed-point-v1 arithmetic required");
    const std::string document = read_policy_file(required_env("ENCLAVE_SHIELD_POLICY"));
    if (sha256_hex(document.data(), document.size()) != required_env("ENCLAVE_SHIELD_POLICY_SHA256"))
        retire("policy digest mismatch");

    const auto j = nlohmann::json::parse(document, nullptr, false);
    if (!j.is_object() || !j.contains("schema") || j["schema"] != "shield-public-tensors/1")
        retire("unsupported policy schema");
    if (!j.contains("calibrationSha256") || !j["calibrationSha256"].is_string() ||
            !is_sha256_hex(j["calibrationSha256"].get<std::string>()))
        retire("invalid policy calibration digest");
    if (!j.contains("tensors") || !j["tensors"].is_array() || j["tensors"].empty())
        retire("empty public policy");

    Policy policy;
    for (const auto & e : j["tensors"]) {
        if (!e.is_object()) retire("invalid public tensor record");
        const auto & name = e.at("name");
        const auto & type = e.at("type");
        const auto & ne = e.at("ne");
        const auto & nbytes = e.at("nbytes");
        const auto & sha = e.at("sha256");
        if (!name.is_string() || !type.is_number_unsigned() || !ne.is_array() || ne.size() != 4 ||
                !nbytes.is_number_unsigned() || !sha.is_string())
            retire("invalid public tensor record");
        PolicyEntry entry{};
        if (type.get<uint64_t>() > UINT32_MAX) retire("invalid public tensor record");
        entry.type = type.get<uint32_t>();
        for (size_t i = 0; i < 4; ++i) {
            if (!ne[i].is_number_integer() || ne[i].get<int64_t>() < 1)
                retire("invalid public tensor record");
            entry.ne[i] = ne[i].get<int64_t>();
        }
        entry.nbytes = nbytes.get<uint64_t>();
        entry.sha256 = sha.get<std::string>();
        auto key = name.get<std::string>();
        if (key.empty() || key.size() >= GGML_MAX_NAME || key.find('\0') != std::string::npos ||
                entry.nbytes == 0 || !is_sha256_hex(entry.sha256) ||
                !policy.emplace(std::move(key), std::move(entry)).second)
            retire("invalid public tensor record");
    }
    return policy;
}

// ---------------------------------------------------------------- model

// Read-only mapping of the trusted, immutable GGUF plus its no_alloc metadata.
// Metadata tensors point into the mapping once verified; both must outlive
// the backend, which is why Backend is declared after Model.
struct Mapping {
    void * base = MAP_FAILED;
    size_t size = 0;
    Mapping() = default;
    Mapping(const Mapping &) = delete;
    Mapping & operator=(const Mapping &) = delete;
    ~Mapping() {
        if (base != MAP_FAILED) munmap(base, size);
    }
};

struct Model {
    // Member order is teardown order in reverse: gguf, meta, then mapping.
    Mapping map;
    std::unique_ptr<ggml_context, decltype(&ggml_free)> meta{nullptr, ggml_free};
    std::unique_ptr<gguf_context, decltype(&gguf_free)> gguf{nullptr, gguf_free};
    std::map<std::string, ggml_tensor *> verified;
    // Tensors whose mapped bytes verify_all found to match the policy digest,
    // with the exact range it hashed.
    std::map<std::string, std::pair<const unsigned char *, size_t>> prehashed;

    // Check the policy digest of each named tensor's mapped bytes on several
    // threads. A tensor that is missing, malformed or does not match is simply
    // left out; weight() then performs every check itself and retires.
    void verify_all(const Policy & policy, const std::vector<std::string> & names) {
        struct Job { std::string name; const unsigned char * bytes; size_t size; std::string want; bool ok = false; };
        std::vector<Job> jobs;
        const size_t data_off = gguf_get_data_offset(gguf.get());
        const size_t align = gguf_get_alignment(gguf.get());
        for (const std::string & name : names) {
            const auto pit = policy.find(name);
            const int64_t id = gguf_find_tensor(gguf.get(), name.c_str());
            if (pit == policy.end() || id < 0 || verified.count(name) || prehashed.count(name)) continue;
            const size_t bytes = gguf_get_tensor_size(gguf.get(), id);
            const size_t tensor_off = gguf_get_tensor_offset(gguf.get(), id);
            if (bytes == 0 || bytes > kMaxTensor || bytes != pit->second.nbytes || data_off > map.size ||
                    tensor_off > map.size - data_off || bytes > map.size - data_off - tensor_off ||
                    align == 0 || (data_off + tensor_off) % align != 0)
                continue;
            jobs.push_back({name, static_cast<const unsigned char *>(map.base) + data_off + tensor_off, bytes,
                            pit->second.sha256});
        }
        const unsigned threads = std::max(1u, std::min(8u, std::thread::hardware_concurrency()));
        std::atomic<size_t> next{0};
        const auto work = [&] {
            for (size_t i; (i = next.fetch_add(1)) < jobs.size();) {
                unsigned char out[EVP_MAX_MD_SIZE];
                unsigned count = 0;
                static const char hex[] = "0123456789abcdef";
                std::string digest(64, '0');
                if (!EVP_Digest(jobs[i].bytes, jobs[i].size, out, &count, EVP_sha256(), nullptr) || count != 32) continue;
                for (unsigned b = 0; b < 32; ++b) { digest[2 * b] = hex[out[b] >> 4]; digest[2 * b + 1] = hex[out[b] & 15]; }
                jobs[i].ok = digest == jobs[i].want;
            }
        };
        std::vector<std::thread> pool;
        for (unsigned t = 1; t < threads; ++t) pool.emplace_back(work);
        work();
        for (auto & thread : pool) thread.join();
        for (const Job & job : jobs)
            if (job.ok) prehashed.emplace(job.name, std::make_pair(job.bytes, job.size));
    }

    explicit Model(const char * path) {
        const int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) retire("model open failed");
        struct stat st{};
        if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode) || st.st_size <= 0 ||
                (uint64_t)st.st_size > kMaxModel) {
            close(fd);
            retire("invalid model file");
        }
        map.size = (size_t)st.st_size;
        map.base = mmap(nullptr, map.size, PROT_READ, MAP_PRIVATE, fd, 0);
        // Parse metadata from the same open file, not by reopening the path.
        const int dup_fd = fcntl(fd, F_DUPFD_CLOEXEC, 0);
        close(fd);
        if (map.base == MAP_FAILED) {
            if (dup_fd >= 0) close(dup_fd);
            retire("model mmap failed");
        }
        FILE * file = dup_fd >= 0 ? fdopen(dup_fd, "rb") : nullptr;
        if (!file) {
            if (dup_fd >= 0) close(dup_fd);
            retire("model metadata open failed");
        }
        ggml_context * meta_ctx = nullptr;
        gguf.reset(gguf_init_from_file_ptr(file, gguf_init_params{true, &meta_ctx}));
        meta.reset(meta_ctx);
        std::fclose(file);
        if (!gguf || !meta) retire("model metadata parse failed");
    }
    Model(const Model &) = delete;
    Model & operator=(const Model &) = delete;

    // Exact-name lookup. On first use: validate metadata against the policy,
    // hash the mapped bytes and only then point the tensor at the mapping.
    // Whether a client's reference names an approved public tensor. A session
    // client that gets this wrong is refused; it has not broken anything.
    static bool approved(const Policy & policy, const std::string & name,
                         const std::string & digest, int64_t k, int64_t n) {
        const auto pit = policy.find(name);
        return pit != policy.end() && pit->second.sha256 == digest &&
               lowered_weight(pit->second.type, k) &&
               pit->second.ne == std::array<int64_t, 4>{k, n, 1, 1};
    }

    ggml_tensor * weight(const Policy & policy, const std::string & name,
                         const std::string & digest, int64_t k, int64_t n) {
        const auto pit = policy.find(name);
        if (pit == policy.end()) retire("tensor not in public policy");
        const PolicyEntry & entry = pit->second;
        if (entry.sha256 != digest) retire("request digest does not match policy");
        if (!lowered_weight(entry.type, k) || entry.ne != std::array<int64_t, 4>{k, n, 1, 1})
            retire("request geometry does not match policy");
        const ggml_type type = (ggml_type)entry.type;

        if (const auto it = verified.find(name); it != verified.end()) return it->second;

        const int64_t id = gguf_find_tensor(gguf.get(), name.c_str());
        ggml_tensor * t = ggml_get_tensor(meta.get(), name.c_str());
        if (id < 0 || !t || std::strcmp(ggml_get_name(t), name.c_str()))
            retire("tensor not in model");
        const int64_t * gne = gguf_get_tensor_ne(gguf.get(), id);
        if (gguf_get_tensor_type(gguf.get(), id) != type || t->type != type)
            retire("tensor type does not match policy");
        for (int i = 0; i < 4; ++i)
            if (gne[i] != entry.ne[i] || t->ne[i] != entry.ne[i])
                retire("tensor geometry mismatch");
        const size_t bytes = gguf_get_tensor_size(gguf.get(), id);
        if (bytes == 0 || bytes > kMaxTensor || bytes != entry.nbytes || bytes != ggml_nbytes(t) ||
                bytes != ggml_row_size(type, k) * (size_t)n || !ggml_is_contiguous(t))
            retire("tensor size mismatch");
        const size_t size = map.size;
        const size_t data_off = gguf_get_data_offset(gguf.get());
        const size_t tensor_off = gguf_get_tensor_offset(gguf.get(), id);
        const size_t align = gguf_get_alignment(gguf.get());
        if (data_off > size || tensor_off > size - data_off || bytes > size - data_off - tensor_off ||
                align == 0 || (data_off + tensor_off) % align != 0)
            retire("tensor offset outside model file");
        if (t->data || t->buffer || t->view_src) retire("unexpected metadata tensor state");

        const auto * bytes_ptr = static_cast<const unsigned char *>(map.base) + data_off + tensor_off;
        if (const auto it = prehashed.find(name); it != prehashed.end()) {
            // verify_all hashed exactly these bytes: same mapping, offset and length.
            if (it->second.first != bytes_ptr || it->second.second != bytes) retire("model tensor digest mismatch");
        } else if (sha256_hex(bytes_ptr, bytes) != entry.sha256) {
            retire("model tensor digest mismatch");
        }

        // Read-only public mapping; the backend copies it privately and the
        // bootstrap verifier re-authenticates that copy at registration.
        t->data = const_cast<unsigned char *>(bytes_ptr);
        verified.emplace(name, t);
        return t;
    }
};

// ---------------------------------------------------------------- backend

struct Backend {
    ggml_backend_t backend = nullptr;
    Backend() {
        // The module the bootstrap loaded (and installed the verifier into)
        // must be the very instance this binary links against.
        ggml_backend_reg_t reg = ggml_backend_reg_by_name("Shielded");
        if (!reg || reg != ggml_backend_shielded_reg()) retire("Shield module mismatch");
        for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
            ggml_backend_dev_t dev = ggml_backend_dev_get(i);
            const auto type = ggml_backend_dev_type(dev);
            if (type != GGML_BACKEND_DEVICE_TYPE_CPU &&
                    (type != GGML_BACKEND_DEVICE_TYPE_ACCEL || std::strcmp(ggml_backend_dev_name(dev), "Shielded")))
                retire("unprotected compute backend present");
        }
        // Idempotent before registration: also proves a verifier is installed.
        // The pinned backend rejects this after any weights/pending work exist.
        if (ggml_backend_shielded_require_offload() != 0)
            retire("strict verified offload unavailable");
        backend = ggml_backend_shielded_init();
        if (!backend || !ggml_backend_is_shielded(backend)) retire("Shield backend init failed");
    }
    ~Backend() {
        if (backend) ggml_backend_free(backend);
    }
    Backend(const Backend &) = delete;
    Backend & operator=(const Backend &) = delete;
};

struct Counters {
    uint64_t offloaded = 0, local = 0, graph_macs = 0, failures = 0, exchanges = 0, macs = 0;
    static Counters read();
};

// ---------------------------------------------------------------- I/O

class Input {
public:
    enum class Line { Ok, Eof };

    // Clean EOF is only accepted before the first byte of a header.
    Line read_line(std::string & line) {
        line.clear();
        for (;;) {
            if (pos_ == len_ && !fill()) {
                if (line.empty()) return Line::Eof;
                retire("truncated header");
            }
            const char c = buf_[pos_++];
            if (line.size() + 1 > kMaxHeader) retire("header too long");
            if (c == '\n') return Line::Ok;
            line.push_back(c);
        }
    }
    void read_exact(void * dst, size_t n) {
        auto * out = static_cast<unsigned char *>(dst);
        while (n) {
            if (pos_ == len_ && !fill()) retire("truncated payload");
            const size_t take = std::min(n, len_ - pos_);
            std::memcpy(out, buf_ + pos_, take);
            pos_ += take; out += take; n -= take;
        }
    }

private:
    bool fill() {
        for (;;) {
            const ssize_t r = read(STDIN_FILENO, buf_, sizeof buf_);
            if (r < 0 && errno == EINTR) continue;
            if (r < 0) retire("input read failed");
            pos_ = 0; len_ = (size_t)r;
            return r > 0;
        }
    }
public:
    bool buffered() const { return pos_ != len_; }

private:
    unsigned char buf_[64 * 1024];
    size_t pos_ = 0, len_ = 0;
};

bool write_all(int fd, const void * data, size_t n) noexcept {
    const auto * p = static_cast<const unsigned char *>(data);
    while (n) {
        const ssize_t w = write(fd, p, n);
        if (w < 0 && errno == EINTR) continue;
        if (w <= 0) return false;
        p += w; n -= (size_t)w;
    }
    return true;
}

void reply(int fd, const nlohmann::json & header, const void * payload = nullptr, size_t n = 0) {
    const std::string line = header.dump() + "\n";
    if (!write_all(fd, line.data(), line.size()) || (n && !write_all(fd, payload, n)))
        retire("reply write failed");
}

void reply_failure(int fd) noexcept {
    static const char line[] =
        "{\"ok\":false,\"outputBytes\":0,\"error\":\"Q8 execution failed; executor retired\"}\n";
    write_all(fd, line, sizeof line - 1);
}

// ---------------------------------------------------------------- requests

struct Request {
    std::string tensor, sha256;
    uint64_t k = 0, n = 0, m = 0, input_bytes = 0;
};

uint64_t positive_int(const nlohmann::json & v) {
    // is_number_integer() is false for bool and for floating point literals.
    if (!v.is_number_integer()) retire("invalid integer field");
    if (v.is_number_unsigned()) {
        const uint64_t x = v.get<uint64_t>();
        if (x == 0) retire("invalid integer field");
        return x;
    }
    const int64_t x = v.get<int64_t>();
    if (x <= 0) retire("invalid integer field");
    return (uint64_t)x;
}

Request parse_header(const std::string & line) {
    size_t top_keys = 0;
    const nlohmann::json::parser_callback_t count_keys =
        [&top_keys](int depth, nlohmann::json::parse_event_t event, nlohmann::json &) {
            if (event == nlohmann::json::parse_event_t::key && depth == 1) ++top_keys;
            return true;
        };
    const auto j = nlohmann::json::parse(line, count_keys, false);
    static const char * const keys[] = {"schema", "tensor", "sha256", "k", "n", "m", "arithmetic", "inputBytes"};
    // Counting raw keys also rejects duplicates, which json would collapse.
    if (j.is_discarded() || !j.is_object() || top_keys != std::size(keys) || j.size() != std::size(keys))
        retire("malformed header");
    for (const char * key : keys)
        if (!j.contains(key)) retire("malformed header");
    if (!j["schema"].is_string() || j["schema"] != "shield-q8-executor/1" ||
            !j["arithmetic"].is_string() || j["arithmetic"] != kArithmetic ||
            !j["tensor"].is_string() || !j["sha256"].is_string())
        retire("malformed header");

    Request r;
    r.tensor = j["tensor"].get<std::string>();
    r.sha256 = j["sha256"].get<std::string>();
    if (r.tensor.empty() || r.tensor.size() > GGML_MAX_NAME - 1) retire("invalid tensor name");
    for (unsigned char c : r.tensor)
        if (c < 0x20 || c > 0x7e) retire("invalid tensor name");
    if (!is_sha256_hex(r.sha256)) retire("invalid digest");

    r.k = positive_int(j["k"]);
    r.n = positive_int(j["n"]);
    r.m = positive_int(j["m"]);
    r.input_bytes = positive_int(j["inputBytes"]);
    // Bound each factor first so the products below cannot overflow.
    if (r.m > kMaxRows || r.k > kMaxPayload || r.n > kMaxPayload || r.k % 32 != 0)
        retire("unsupported geometry");
    if (r.input_bytes != r.m * r.k * 4 || r.input_bytes > kMaxPayload || r.m * r.n * 4 > kMaxPayload)
        retire("unsupported geometry");
    return r;
}

// Every value is finite: no exponent field is all ones. A reduction over
// fixed blocks, so the compiler vectorises it; a vocabulary-sized product is
// otherwise a visible share of a token.
__attribute__((optimize("O3"), target_clones("avx2", "default")))
// Fixed blocks so the compiler vectorises the reduction; the products of a
// feed-forward pair are large enough for a scalar scan to show in a token.
__attribute__((optimize("O3"), target_clones("avx2", "default")))
bool all_finite(const float * v, size_t n) {
    uint32_t largest = 0;
    size_t i = 0;
    for (; i + 64 <= n; i += 64) {
        uint32_t block[64];
        std::memcpy(block, v + i, sizeof block);
        for (unsigned j = 0; j < 64; ++j) largest = std::max(largest, block[j] & 0x7fffffffu);
    }
    for (; i < n; ++i) {
        uint32_t bits;
        std::memcpy(&bits, v + i, 4);
        largest = std::max(largest, bits & 0x7fffffffu);
    }
    return largest < 0x7f800000u;
}

// all_finite(from, n) and a copy to `to` in one pass. `from` is private
// memory; the answer describes exactly the values that were there.
__attribute__((optimize("O3"), target_clones("avx2", "default")))
bool copy_finite(unsigned char * to, const float * from, size_t n) {
    uint32_t largest = 0;
    size_t i = 0;
    for (; i + 64 <= n; i += 64) {
        uint32_t block[64];
        std::memcpy(block, from + i, sizeof block);
        std::memcpy(to + i * 4, block, sizeof block);
        for (unsigned j = 0; j < 64; ++j) largest = std::max(largest, block[j] & 0x7fffffffu);
    }
    for (; i < n; ++i) {
        uint32_t bits;
        std::memcpy(&bits, from + i, 4);
        std::memcpy(to + i * 4, &bits, 4);
        largest = std::max(largest, bits & 0x7fffffffu);
    }
    return largest < 0x7f800000u;
}

using GraphContext = std::unique_ptr<ggml_context, decltype(&ggml_free)>;

// One weight x batch-width product, built once and reused. The engine does
// the same with its own graphs, so the backend sees stable nodes across tokens.
struct Product {
    GraphContext ctx{nullptr, ggml_free};
    ggml_tensor * x = nullptr, * y = nullptr;
    ggml_cgraph * graph = nullptr;
    size_t input_bytes = 0, output_bytes = 0;
    bool supported = false;
    uint64_t stamp = 0;  // changes whenever x or y may have changed
};

// Optional aggregate timing (SHIELD_Q8_EXECUTOR_TIMING=1): counts and
// durations only, printed once when the executor stops.
struct Timing {
    const bool enabled = std::getenv("SHIELD_Q8_EXECUTOR_TIMING") != nullptr;
    // SHIELD_Q8_EXECUTOR_TIMING=N leaves the first N products (load) out.
    const uint64_t skip = enabled ? std::strtoull(std::getenv("SHIELD_Q8_EXECUTOR_TIMING"), nullptr, 10) : 0;
    uint64_t seen = 0;
    uint64_t compute_ns = 0, backend_ns = 0, request_ns = 0, counters_ns = 0, calls = 0, exchanged = 0;
    uint64_t gap_ns[2] = {0, 0}, gaps[2] = {0, 0}, replied = 0, ahead = 0; bool was_pair = false;
    static uint64_t now() {
        return (uint64_t)std::chrono::duration_cast<std::chrono::nanoseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count();
    }
    ~Timing() {
        if (enabled && calls)
            std::fprintf(stderr, "[q8-executor-timing] client time after a single product: n=%llu us=%.1f; after a pair: n=%llu us=%.1f; served ahead=%llu\n",
                         (unsigned long long)gaps[0], gaps[0] ? gap_ns[0] / 1e3 / gaps[0] : 0.0,
                         (unsigned long long)gaps[1], gaps[1] ? gap_ns[1] / 1e3 / gaps[1] : 0.0,
                         (unsigned long long)ahead);
        if (enabled && calls)
            std::fprintf(stderr, "[q8-executor-timing] calls=%llu exchanged=%llu request_us=%.1f compute_us=%.1f backend_us=%.1f counters_us=%.1f\n",
                         (unsigned long long)calls, (unsigned long long)exchanged, request_ns / 1e3 / calls,
                         compute_ns / 1e3 / calls, backend_ns / 1e3 / calls, counters_ns / 1e3 / calls);
    }
};
Timing timing;

Counters Counters::read() {
    const uint64_t t0 = timing.enabled ? Timing::now() : 0;
    Counters c;
    ggml_backend_shielded_stats(&c.offloaded, &c.local, &c.graph_macs, &c.failures);
    ggml_backend_shielded_verified_stats(&c.exchanges, &c.macs);
    if (timing.enabled && timing.seen >= timing.skip) timing.counters_ns += Timing::now() - t0;
    return c;
}

struct Outcome {
    bool supported = false;
    uint64_t exchanges = 0, macs = 0;
    Counters delta;
};

class Engine {
public:
    Engine(Model & model, const Policy & policy, Backend & backend)
        : model_(model), policy_(policy), backend_(backend) {}

    const Policy & policy() const { return policy_; }
    Model & model() { return model_; }

    Product & product(ggml_tensor * w, uint64_t k, uint64_t n, uint64_t m) {
        const auto key = std::make_pair(w, m);
        if (const auto it = products_.find(key); it != products_.end()) return it->second;
        constexpr size_t kGraphNodes = 8;
        Product p;
        p.input_bytes = m * k * 4;
        p.output_bytes = m * n * 4;
        // Keep the retained scratch bounded; a full cache is simply rebuilt.
        if (retained_ + p.input_bytes + p.output_bytes > kMaxRetained) {
            products_.clear();
            pairs_.clear();
            retained_ = 0;
            ++generation_;
        }
        const size_t mem = 4 * ggml_tensor_overhead() + ggml_graph_overhead_custom(kGraphNodes, false) +
                           p.input_bytes + p.output_bytes + (64u << 10);
        p.ctx.reset(ggml_init(ggml_init_params{mem, nullptr, false}));
        if (!p.ctx) retire("graph context allocation failed");
        p.x = ggml_new_tensor_2d(p.ctx.get(), GGML_TYPE_F32, (int64_t)k, (int64_t)m);
        if (!p.x || !p.x->data || ggml_nbytes(p.x) != p.input_bytes) retire("input tensor allocation failed");
        ggml_set_input(p.x);
        p.y = ggml_mul_mat(p.ctx.get(), w, p.x);
        if (!p.y || !p.y->data || p.y->type != GGML_TYPE_F32 || p.y->ne[0] != (int64_t)n ||
                p.y->ne[1] != (int64_t)m || p.y->ne[2] != 1 || p.y->ne[3] != 1 ||
                ggml_nbytes(p.y) != p.output_bytes || !ggml_is_contiguous(p.y))
            retire("output tensor allocation failed");
        ggml_set_output(p.y);
        p.graph = ggml_new_graph_custom(p.ctx.get(), kGraphNodes, false);
        if (!p.graph) retire("graph allocation failed");
        ggml_build_forward_expand(p.graph, p.y);
        p.supported = ggml_backend_supports_op(backend_.backend, p.y);
        retained_ += p.input_bytes + p.output_bytes;
        return products_.emplace(key, std::move(p)).first->second;
    }

    // The input is already in p.x->data and has been checked finite. With
    // `to`, the product is also copied there, after the accounting holds and
    // in the same pass that checks it; a product that then turns out not to be
    // finite retires the executor with part of it copied and no reply sent.
    Outcome compute(Product & p, uint64_t k, uint64_t n, uint64_t m, unsigned char * to = nullptr) {
        Outcome out;
        if (!p.supported) return out;
        ++p.stamp;
        const uint64_t t0 = timing.enabled ? Timing::now() : 0;
        // NaN sentinel: an output the backend never wrote fails the finite check.
        std::fill_n(static_cast<float *>(p.y->data), m * n, std::nanf(""));
        const Counters before = Counters::read();
        if (before.failures != 0) retire("backend has verification failures");
        const uint64_t t1 = timing.enabled ? Timing::now() : 0;
        if (ggml_backend_graph_compute(backend_.backend, p.graph) != GGML_STATUS_SUCCESS)
            retire("backend compute failed");

        if (timing.enabled && timing.seen >= timing.skip) timing.backend_ns += Timing::now() - t1;
        const Counters after = Counters::read();
        if (after.offloaded < before.offloaded || after.local < before.local ||
                after.graph_macs < before.graph_macs || after.exchanges < before.exchanges ||
                after.macs < before.macs || after.failures != 0)
            retire("backend counters inconsistent");
        Counters & d = out.delta;
        d.offloaded = after.offloaded - before.offloaded;
        d.local = after.local - before.local;
        d.graph_macs = after.graph_macs - before.graph_macs;
        d.exchanges = after.exchanges - before.exchanges;
        d.macs = after.macs - before.macs;
        // The accounting every caller relies on: exactly this product ran,
        // once, and verified exchanges and their work appear together.
        if (d.graph_macs != m * k * n || d.offloaded + d.local != 1 ||
                (d.exchanges != 0) != (d.macs != 0) || (d.local == 1 && d.exchanges != 0))
            retire("invalid verified accounting");
        const auto * y = static_cast<const float *>(p.y->data);
        if (!(to ? copy_finite(to, y, m * n) : all_finite(y, m * n))) retire("non-finite output");
        out.supported = true;
        out.exchanges = d.exchanges;
        out.macs = d.macs;
        if (timing.enabled && timing.seen++ >= timing.skip) {
            timing.compute_ns += Timing::now() - t0;
            ++timing.calls;
            timing.exchanged += d.exchanges != 0;
        }
        ++completed_;
        return out;
    }

    // Two weights applied to one activation in a single graph, so the backend
    // sees the pair the way an engine's own graph presents it: one call, one
    // exchange for the group. Null when either product is unsupported.
    struct Pair {
        GraphContext ctx{nullptr, ggml_free};
        ggml_tensor * x = nullptr, * y[2] = {nullptr, nullptr};
        ggml_cgraph * graph = nullptr;
        size_t input_bytes = 0, output_bytes[2] = {0, 0};
        // The two products gated together the way a feed-forward block does
        // it, swiglu(y[i], y[1 - i]), for products of equal width. Computed
        // only when gated() is asked for one.
        ggml_tensor * gated[2] = {nullptr, nullptr};
        ggml_cgraph * glue[2] = {nullptr, nullptr};
        uint64_t stamp = 0;  // changes with every compute
    };

    Pair * pair(ggml_tensor * w1, ggml_tensor * w2, uint64_t k, uint64_t n1, uint64_t n2, uint64_t m) {
        const auto key = std::make_tuple(w1, w2, m);
        if (const auto it = pairs_.find(key); it != pairs_.end()) return it->second.graph ? &it->second : nullptr;
        constexpr size_t kGraphNodes = 8;
        Pair p;
        p.input_bytes = m * k * 4;
        p.output_bytes[0] = m * n1 * 4;
        p.output_bytes[1] = m * n2 * 4;
        const size_t bytes = p.input_bytes + p.output_bytes[0] + p.output_bytes[1];
        if (retained_ + bytes > kMaxRetained) {
            products_.clear();
            pairs_.clear();
            retained_ = 0;
            ++generation_;
        }
        const bool glue = n1 == n2;
        const size_t mem = 10 * ggml_tensor_overhead() + 3 * ggml_graph_overhead_custom(kGraphNodes, false) + bytes +
                           (glue ? 4 * p.output_bytes[0] : 0) + (64u << 10);
        p.ctx.reset(ggml_init(ggml_init_params{mem, nullptr, false}));
        if (!p.ctx) retire("graph context allocation failed");
        p.x = ggml_new_tensor_2d(p.ctx.get(), GGML_TYPE_F32, (int64_t)k, (int64_t)m);
        if (!p.x || !p.x->data || ggml_nbytes(p.x) != p.input_bytes) retire("input tensor allocation failed");
        ggml_set_input(p.x);
        ggml_tensor * const weights[2] = {w1, w2};
        const uint64_t widths[2] = {n1, n2};
        bool supported = true;
        for (int i = 0; i < 2; ++i) {
            ggml_tensor * y = ggml_mul_mat(p.ctx.get(), weights[i], p.x);
            if (!y || !y->data || y->type != GGML_TYPE_F32 || y->ne[0] != (int64_t)widths[i] ||
                    y->ne[1] != (int64_t)m || y->ne[2] != 1 || y->ne[3] != 1 ||
                    ggml_nbytes(y) != p.output_bytes[i] || !ggml_is_contiguous(y))
                retire("output tensor allocation failed");
            ggml_set_output(y);
            supported = supported && ggml_backend_supports_op(backend_.backend, y);
            p.y[i] = y;
        }
        if (supported) {
            p.graph = ggml_new_graph_custom(p.ctx.get(), kGraphNodes, false);
            if (!p.graph) retire("graph allocation failed");
            ggml_build_forward_expand(p.graph, p.y[0]);
            ggml_build_forward_expand(p.graph, p.y[1]);
        }
        // The gating graphs read the products through plain input tensors
        // that point at the same bytes. Built on the products themselves, a
        // graph would contain the two multiplications and the CPU backend
        // would compute them again, unprotected and over the verified values.
        ggml_tensor * read[2] = {nullptr, nullptr};
        for (int i = 0; supported && glue && i < 2; ++i) {
            read[i] = ggml_new_tensor_2d(p.ctx.get(), GGML_TYPE_F32, (int64_t)n1, (int64_t)m);
            if (!read[i]) retire("graph context allocation failed");
            read[i]->data = p.y[i]->data;
            ggml_set_input(read[i]);
        }
        for (int i = 0; supported && glue && i < 2; ++i) {
            ggml_tensor * g = ggml_swiglu_split(p.ctx.get(), read[i], read[1 - i]);
            ggml_cgraph * graph = g ? ggml_new_graph_custom(p.ctx.get(), kGraphNodes, false) : nullptr;
            if (!graph || !g->data || g->type != GGML_TYPE_F32 || ggml_nbytes(g) != p.output_bytes[0]) break;
            ggml_build_forward_expand(graph, g);
            p.gated[i] = g;
            p.glue[i] = graph;
        }
        retained_ += bytes + (glue ? 4 * p.output_bytes[0] : 0);
        Pair & stored = pairs_.emplace(key, std::move(p)).first->second;
        return stored.graph ? &stored : nullptr;
    }

    // Both products, or a retirement. The input is in p.x->data, checked finite.
    // `to` receives them as in the single-product compute.
    Outcome compute(Pair & p, uint64_t k, uint64_t n1, uint64_t n2, uint64_t m, unsigned char * const to[2]) {
        Outcome out;
        ++p.stamp;
        const uint64_t t0 = timing.enabled ? Timing::now() : 0;
        std::fill_n(static_cast<float *>(p.y[0]->data), m * n1, std::nanf(""));
        std::fill_n(static_cast<float *>(p.y[1]->data), m * n2, std::nanf(""));
        const Counters before = Counters::read();
        if (before.failures != 0) retire("backend has verification failures");
        const uint64_t t1 = timing.enabled ? Timing::now() : 0;
        if (ggml_backend_graph_compute(backend_.backend, p.graph) != GGML_STATUS_SUCCESS)
            retire("backend compute failed");
        if (timing.enabled && timing.seen >= timing.skip) timing.backend_ns += Timing::now() - t1;
        const Counters after = Counters::read();
        if (after.offloaded < before.offloaded || after.local < before.local ||
                after.graph_macs < before.graph_macs || after.exchanges < before.exchanges ||
                after.macs < before.macs || after.failures != 0)
            retire("backend counters inconsistent");
        Counters & d = out.delta;
        d.offloaded = after.offloaded - before.offloaded;
        d.local = after.local - before.local;
        d.graph_macs = after.graph_macs - before.graph_macs;
        d.exchanges = after.exchanges - before.exchanges;
        d.macs = after.macs - before.macs;
        // Exactly these two products ran, once each, and verified exchanges
        // and their work appear together.
        if (d.graph_macs != m * k * (n1 + n2) || d.offloaded + d.local != 2 ||
                (d.exchanges != 0) != (d.macs != 0) || (d.local == 2 && d.exchanges != 0))
            retire("invalid verified accounting");
        if (!copy_finite(to[0], static_cast<const float *>(p.y[0]->data), m * n1) ||
                !copy_finite(to[1], static_cast<const float *>(p.y[1]->data), m * n2))
            retire("non-finite output");
        out.supported = true;
        out.exchanges = d.exchanges;
        out.macs = d.macs;
        if (timing.enabled && timing.seen++ >= timing.skip) {
            timing.compute_ns += Timing::now() - t0;
            ++timing.calls;
            timing.exchanged += d.exchanges != 0;
        }
        completed_ += 2;
        return out;
    }

    uint64_t completed() const { return completed_; }

    // Changes when cached graphs were dropped: Product and Pair references
    // taken before the change are gone.
    uint64_t generation() const { return generation_; }

    // swiglu(y[order], y[1 - order]) of the pair's current products, computed
    // on the CPU backend in this process, or null when that is not available.
    // Ordinary arithmetic on values this process already holds; nothing is
    // sent anywhere.
    const float * gated(Pair & p, int order) {
        if (!p.glue[order]) return nullptr;
        if (!cpu_tried_) {
            cpu_tried_ = true;
            cpu_ = ggml_backend_init_by_type(GGML_BACKEND_DEVICE_TYPE_CPU, nullptr);
            if (cpu_) {
                ggml_backend_reg_t reg = ggml_backend_dev_backend_reg(ggml_backend_get_device(cpu_));
                const auto set = reg ? (ggml_backend_set_n_threads_t)ggml_backend_reg_get_proc_address(
                                           reg, "ggml_backend_set_n_threads") : nullptr;
                if (set) set(cpu_, 1);
            }
        }
        if (!cpu_ || ggml_backend_graph_compute(cpu_, p.glue[order]) != GGML_STATUS_SUCCESS) return nullptr;
        return static_cast<const float *>(p.gated[order]->data);
    }

    // Offer every approved tensor to the backend before the first product, so
    // it registers them with the worker in one start. Offered one at a time,
    // each new tensor restarts the link with everything registered so far.
    //
    // Only when the whole approved set fits this executor's share of the
    // card with room to spare: the backend refuses a placement it cannot
    // complete, and a client that uses a few tensors of a larger model must
    // keep working as it does when tensors are offered as they are used.
    void prepare() {
        if (prepared_) return;
        prepared_ = true;
        const auto eligible = [](const PolicyEntry & entry) {
            return lowered_weight(entry.type, entry.ne[0]) && entry.ne[2] == 1 && entry.ne[3] == 1 &&
                   (uint64_t)entry.ne[0] <= kMaxPayload / 4 && (uint64_t)entry.ne[1] <= kMaxPayload / 4;
        };
        const char * reserve = std::getenv("SHIELDED_RESERVE_BYTES");
        uint64_t cap = reserve ? std::strtoull(reserve, nullptr, 10) : 0;
        // With a pool of workers (host|port|vsock|bytes, one a line) the
        // backend spreads the weights over all of them: their shares add up.
        if (const char * pool = std::getenv("SHIELDED_WORKERS")) {
            uint64_t total = 0;
            std::istringstream lines(pool);
            for (std::string line; std::getline(lines, line);) {
                std::istringstream fields(line);
                std::string field;
                for (int i = 0; i < 4 && std::getline(fields, field, '|'); ++i)
                    if (i == 3) total += std::strtoull(field.c_str(), nullptr, 10);
            }
            if (total) cap = total;
        }
        if (cap) {
            // The backend charges a weight its int8 matrix plus activation
            // buffers for the widest batch; keep a fifth of the share spare.
            const char * width = std::getenv("SHIELDED_MAX_M");
            const uint64_t rows = width ? std::min<uint64_t>(std::strtoull(width, nullptr, 10), 4096) : 8;
            uint64_t bytes = 0;
            for (const auto & [name, entry] : policy_) {
                if (!eligible(entry)) continue;
                const uint64_t k = (uint64_t)entry.ne[0], n = (uint64_t)entry.ne[1];
                bytes += k * n + rows * (3 * k + 4 * n);
            }
            if (bytes > cap / 5 * 4) return;
        }
        // Hashing a multi-gigabyte model on one thread is most of the wait.
        // The digests are independent, so check them on a few threads first;
        // weight() then finds each tensor already verified or retires.
        std::vector<std::string> names;
        for (const auto & [name, entry] : policy_)
            if (eligible(entry)) names.push_back(name);
        model_.verify_all(policy_, names);
        for (const std::string & name : names) {
            const PolicyEntry & entry = policy_.at(name);
            ggml_tensor * w = model_.weight(policy_, name, entry.sha256, entry.ne[0], entry.ne[1]);
            product(w, (uint64_t)entry.ne[0], (uint64_t)entry.ne[1], 1);
        }
        // An empty graph makes the backend place and encode what was offered
        // now, rather than inside the first product.
        GraphContext ctx(ggml_init(ggml_init_params{ggml_graph_overhead_custom(8, false) + (64u << 10), nullptr, false}),
                         ggml_free);
        ggml_cgraph * empty = ctx ? ggml_new_graph_custom(ctx.get(), 8, false) : nullptr;
        if (!empty || ggml_backend_graph_compute(backend_.backend, empty) != GGML_STATUS_SUCCESS)
            retire("backend placement failed");
    }

private:
    static constexpr size_t kMaxRetained = 256u << 20;
    Model & model_;
    const Policy & policy_;
    Backend & backend_;
    std::map<std::pair<ggml_tensor *, uint64_t>, Product> products_;
    std::map<std::tuple<ggml_tensor *, ggml_tensor *, uint64_t>, Pair> pairs_;
    uint64_t generation_ = 0;
    ggml_backend_t cpu_ = nullptr;
    bool cpu_tried_ = false;
    size_t retained_ = 0;
    std::atomic<uint64_t> completed_{0};  // read by the serving loop while a job may run
    bool prepared_ = false;
};

// One request of the stdin protocol. Its peer is the measured service, which
// validated the request, so anything malformed here retires the executor.
void serve_stdin(Input & in, const std::string & line, int out_fd, Engine & engine) {
    const Request req = parse_header(line);
    ggml_tensor * w = nullptr;
    // Complete, validated private payload before any backend query.
    std::vector<float> input(req.m * req.k);
    in.read_exact(input.data(), req.input_bytes);
    if (!all_finite(input.data(), input.size())) retire("non-finite input");
    w = engine.model().weight(engine.policy(), req.tensor, req.sha256, (int64_t)req.k, (int64_t)req.n);
    Product & p = engine.product(w, req.k, req.n, req.m);
    std::memcpy(p.x->data, input.data(), req.input_bytes);
    const Outcome out = engine.compute(p, req.k, req.n, req.m);
    if (!out.supported) {
        reply(out_fd, {{"ok", true}, {"supported", false}, {"outputBytes", 0}});
        return;
    }
    reply(out_fd,
          {{"ok", true},
           {"supported", true},
           {"outputBytes", p.output_bytes},
           {"arithmetic", kArithmetic},
           {"stats",
            {{"offloaded", out.delta.offloaded},
             {"local", out.delta.local},
             {"graph_macs", out.delta.graph_macs},
             {"exchanges", out.delta.exchanges},
             {"macs", out.delta.macs},
             {"verification_failures", 0}}}},
          p.y->data, p.output_bytes);
}

// ---------------------------------------------------------------- sessions
//
// Frames are little-endian. A client mistake (bad frame, unknown slot, a
// reference that is not an approved tensor, non-finite input, a stall) ends or
// refuses that session only: nothing protected has started. Backend, model and
// accounting failures still retire the whole process.

constexpr uint32_t kOpen = 1, kMatmul = 2, kShare = 3, kMatmulShared = 4, kBye = 0xff;
// Shared regions, kMaxPayload bytes each: activation, product, paired product.
constexpr size_t kRegions = 3;
constexpr uint32_t kNoPair = 0xffffffff;
constexpr uint32_t kOk = 0, kUnsupported = 1, kRefused = 2;
constexpr size_t kMaxSessions = 16, kMaxSlots = 4096;
constexpr uint64_t kMaxSessionRequests = 1ull << 40;

struct Frame {
    uint32_t type, id, a, b;
};
static_assert(sizeof(Frame) == 16);

struct Slot {
    ggml_tensor * w = nullptr;
    uint64_t k = 0, n = 0;
};

// Optional payload memory shared with one client: input, output and the
// output of a paired product, kMaxPayload bytes each. This process creates and
// seals it, so the client cannot resize it under a mapping. The client can
// still write to it at any time; inputs are copied out before they are
// checked, and outputs are copied in only after they have been checked.
struct Shared {
    unsigned char * base = nullptr;
    Shared() = default;
    Shared(Shared && other) noexcept : base(other.base) { other.base = nullptr; }
    Shared & operator=(Shared && other) noexcept {
        std::swap(base, other.base);
        return *this;
    }
    ~Shared() {
        if (base) munmap(base, kRegions * kMaxPayload);
    }
};

// What a client was seen to do with a pair of products: gate them together
// and ask for a third tensor's product of the result (a feed-forward block's
// down projection). Indexed by the pair's first slot.
struct Chain {
    uint32_t second = kNoPair, next = kNoPair;
    uint8_t order = 0, strikes = 0;
};
constexpr uint8_t kChainStrikes = 3;

struct Session {
    int fd = -1;
    std::vector<Slot> slots;
    Shared shared;
    std::vector<Chain> chains;
    // The pair served last, while its products are still the pair's current ones.
    struct {
        Engine::Pair * pair = nullptr;
        uint64_t stamp = 0, generation = 0, m = 0;
        uint32_t first = kNoPair, second = kNoPair;
        bool valid = false;
    } last;
    // A product computed ahead of the request expected next. It is released
    // only to a request for the same tensor whose activation is byte for byte
    // the one it was computed from; otherwise it is dropped.
    struct {
        Product * product = nullptr;
        ggml_tensor * w = nullptr;
        uint64_t stamp = 0, generation = 0, m = 0;
        uint32_t first = kNoPair;
        Outcome out;
        bool valid = false;
    } held;
};

// This session only. Static reason, as for Retire: no client values.
struct Drop {
    const char * why;
};

void session_read(int fd, void * dst, size_t n) {
    auto * p = static_cast<unsigned char *>(dst);
    while (n) {
        const ssize_t r = recv(fd, p, n, 0);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) throw Drop{r == 0 ? "client closed" : "read stalled"};
        p += r; n -= (size_t)r;
    }
}

void session_write(int fd, const void * src, size_t n) {
    const auto * p = static_cast<const unsigned char *>(src);
    while (n) {
        const ssize_t w = send(fd, p, n, MSG_NOSIGNAL);
        if (w < 0 && errno == EINTR) continue;
        if (w <= 0) throw Drop{"write stalled"};
        p += w; n -= (size_t)w;
    }
}

// One job at a time on a second thread. The thread that answers a client goes
// straight back to waiting for it: a reply on the session socket wakes the
// client onto the replying thread's processor, and a thread that kept
// computing there would hold the client up for as long as it computed.
// Nothing else uses the engine or the sessions while a job runs; the serving
// loop calls wait() before it handles anything.
class Ahead {
public:
    Ahead() : thread_([this] { run(); }) {}
    ~Ahead() {
        {
            std::lock_guard lock(mutex_);
            stop_ = true;
        }
        wake_.notify_one();
        thread_.join();
    }
    Ahead(const Ahead &) = delete;
    Ahead & operator=(const Ahead &) = delete;

    void post(std::function<void()> job) {
        {
            std::lock_guard lock(mutex_);
            job_ = std::move(job);
            busy_.store(true, std::memory_order_release);
        }
        wake_.notify_one();
    }

    // Returns once no job is running; rethrows what the last one threw.
    void wait() {
        // Asleep, not spinning: a job is mostly waiting for the worker, and on
        // a guest with two processors a spinning thread here takes one of them
        // from the thread that makes the pads.
        std::unique_lock lock(mutex_);
        idle_.wait(lock, [this] { return !busy_.load(std::memory_order_acquire); });
        if (error_) std::rethrow_exception(std::exchange(error_, nullptr));
    }

private:
    void run() {
        std::unique_lock lock(mutex_);
        for (;;) {
            wake_.wait(lock, [this] { return stop_ || job_; });
            if (!job_) return;
            const std::function<void()> job = std::exchange(job_, nullptr);
            lock.unlock();
            std::exception_ptr error;
            try {
                job();
            } catch (...) {
                error = std::current_exception();
            }
            lock.lock();
            error_ = error;
            busy_.store(false, std::memory_order_release);
            idle_.notify_all();
        }
    }

    std::mutex mutex_;
    std::condition_variable wake_, idle_;
    std::function<void()> job_;
    std::exception_ptr error_;
    std::atomic<bool> busy_{false};
    bool stop_ = false;
    std::thread thread_;
};

// After a pair, see whether the request that followed asked for a product of
// the pair gated together. The activation is already in x, private and finite.
void chain_learn(Session & s, Engine & engine, uint32_t id, uint64_t m, const void * x, size_t bytes) {
    auto & last = s.last;
    if (!last.valid) return;
    last.valid = false;
    if (last.generation != engine.generation() || last.pair->stamp != last.stamp || last.m != m ||
            last.pair->output_bytes[0] != bytes || id == last.first || id == last.second)
        return;
    if (s.chains.size() <= last.first) s.chains.resize(last.first + 1);
    Chain & chain = s.chains[last.first];
    if (chain.next != kNoPair || chain.strikes >= kChainStrikes) return;
    for (int order = 0; order < 2; ++order) {
        const float * gated = engine.gated(*last.pair, order);
        if (gated && !std::memcmp(gated, x, bytes)) {
            chain.second = last.second;
            chain.next = id;
            chain.order = (uint8_t)order;
            return;
        }
    }
    ++chain.strikes;
}

// After a pair whose continuation is known, compute it before it is asked for.
void chain_ahead(Session & s, Engine & engine, uint32_t first, uint32_t second, uint64_t m, Engine::Pair & pair) {
    if (first >= s.chains.size()) return;
    Chain & chain = s.chains[first];
    if (chain.next == kNoPair || chain.second != second || chain.next >= s.slots.size()) return;
    const Slot & next = s.slots[chain.next];
    const size_t bytes = pair.output_bytes[0];
    if (!next.w || m * next.k * 4 != bytes || m * next.n * 4 > kMaxPayload) return;
    const uint64_t generation = engine.generation();
    Product & p = engine.product(next.w, next.k, next.n, m);
    if (engine.generation() != generation) return;  // the pair went with the old graphs
    const float * gated = engine.gated(pair, chain.order);
    if (!gated || !all_finite(gated, bytes / 4)) return;
    std::memcpy(p.x->data, gated, bytes);
    const Outcome out = engine.compute(p, next.k, next.n, m);
    if (!out.supported) {
        chain.next = kNoPair;
        chain.strikes = kChainStrikes;
        return;
    }
    s.held.product = &p;
    s.held.w = next.w;
    s.held.stamp = p.stamp;
    s.held.generation = engine.generation();
    s.held.m = m;
    s.held.first = first;
    s.held.out = out;
    s.held.valid = true;
}

void session_request(Session & s, Engine & engine, Ahead & ahead_thread) {
    Frame f{};
    session_read(s.fd, &f, sizeof f);
    const uint64_t t0 = timing.enabled ? Timing::now() : 0;
    if (f.type == kOpen) {
        struct { uint64_t k, n; char sha[64]; } body{};
        char name[GGML_MAX_NAME] = {};
        if (f.id >= kMaxSlots || f.a == 0 || f.a >= GGML_MAX_NAME || f.b != 0) throw Drop{"invalid open frame"};
        session_read(s.fd, &body, sizeof body);
        session_read(s.fd, name, f.a);
        const std::string tensor(name, f.a), digest(body.sha, sizeof body.sha);
        for (unsigned char c : tensor)
            if (c < 0x20 || c > 0x7e) throw Drop{"invalid tensor name"};
        engine.prepare();
        Frame r{kOpen, f.id, kRefused, 0};
        if (is_sha256_hex(digest) && body.k && body.n && body.k <= kMaxPayload && body.n <= kMaxPayload &&
                body.k % 32 == 0 &&
                Model::approved(engine.policy(), tensor, digest, (int64_t)body.k, (int64_t)body.n)) {
            // Approved by policy; a model that then disagrees is an integrity failure.
            if (s.slots.size() <= f.id) s.slots.resize(f.id + 1);
            s.slots[f.id] = {engine.model().weight(engine.policy(), tensor, digest, (int64_t)body.k, (int64_t)body.n),
                             body.k, body.n};
            r.a = kOk;
        }
        session_write(s.fd, &r, sizeof r);
        return;
    }
    if (f.type == kShare) {
        // Refused (a = 1) when this kernel or sandbox has no sealed memfd; the
        // client then keeps sending payloads on the socket.
        Frame r{kShare, 0, 1, (uint32_t)kMaxPayload};
        int fd = -1;
        if (!s.shared.base && f.id == 0 && f.a == 0 && f.b == 0) {
#ifdef MFD_NOEXEC_SEAL
            fd = memfd_create("shield-q8", MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL);
            if (fd < 0)  // a kernel without the flag
#endif
            fd = memfd_create("shield-q8", MFD_CLOEXEC | MFD_ALLOW_SEALING);
            void * base = MAP_FAILED;
            if (fd >= 0 && ftruncate(fd, (off_t)(kRegions * kMaxPayload)) == 0 &&
                    fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL) == 0)
                base = mmap(nullptr, kRegions * kMaxPayload, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            if (base != MAP_FAILED) {
                s.shared.base = static_cast<unsigned char *>(base);
                r.a = kOk;
            } else if (fd >= 0) {
                close(fd);
                fd = -1;
            }
        }
        char cbuf[CMSG_SPACE(sizeof(int))] = {};
        iovec iov{&r, sizeof r};
        msghdr msg{};
        msg.msg_iov = &iov; msg.msg_iovlen = 1;
        if (fd >= 0) {
            msg.msg_control = cbuf; msg.msg_controllen = sizeof cbuf;
            cmsghdr * c = CMSG_FIRSTHDR(&msg);
            c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
            std::memcpy(CMSG_DATA(c), &fd, sizeof(int));
        }
        ssize_t sent;
        do sent = sendmsg(s.fd, &msg, MSG_NOSIGNAL); while (sent < 0 && errno == EINTR);
        if (fd >= 0) close(fd);
        if (sent != (ssize_t)sizeof r) throw Drop{"write stalled"};
        return;
    }
    if (f.type == kMatmulShared) {
        if (timing.enabled && timing.seen > timing.skip && timing.replied) {
            timing.gap_ns[timing.was_pair] += t0 - timing.replied;
            ++timing.gaps[timing.was_pair];
        }
        struct { uint32_t pair, reserved; } extra{};
        session_read(s.fd, &extra, sizeof extra);
        if (!s.shared.base || f.id >= s.slots.size() || !s.slots[f.id].w || f.b != 0 || extra.reserved != 0)
            throw Drop{"invalid frame"};
        const Slot & slot = s.slots[f.id];
        const uint64_t m = f.a;
        if (m == 0 || m > kMaxRows || m * slot.k * 4 > kMaxPayload || m * slot.n * 4 > kMaxPayload)
            throw Drop{"unsupported geometry"};
        struct { Frame f; uint64_t exchanges, macs; } r{{kMatmulShared, f.id, kUnsupported, 0}, 0, 0};
        if (s.held.valid) {
            s.held.valid = false;
            Product * ahead = s.held.product;
            if (s.held.generation == engine.generation() && ahead->stamp == s.held.stamp && s.held.w == slot.w &&
                    s.held.m == m && extra.pair == kNoPair && ahead->input_bytes == m * slot.k * 4 &&
                    !std::memcmp(s.shared.base, ahead->x->data, ahead->input_bytes)) {
                std::memcpy(s.shared.base + kMaxPayload, ahead->y->data, ahead->output_bytes);
                r.f.a = kOk;
                r.exchanges = s.held.out.exchanges;
                r.macs = s.held.out.macs;
                s.last.valid = false;
                session_write(s.fd, &r, sizeof r);
                if (timing.enabled) { timing.replied = Timing::now(); timing.was_pair = false; ++timing.ahead; }
                return;
            }
            // Not what followed this time: stop expecting it.
            if (s.held.first < s.chains.size()) {
                Chain & chain = s.chains[s.held.first];
                chain.next = kNoPair;
                if (chain.strikes < kChainStrikes) ++chain.strikes;
            }
        }
        // A second approved tensor applied to the same activation, when the
        // client expects to ask for it next. Declined silently otherwise.
        Engine::Pair * both = nullptr;
        if (extra.pair != kNoPair && extra.pair < s.slots.size() && s.slots[extra.pair].w &&
                extra.pair != f.id && s.slots[extra.pair].k == slot.k && m * s.slots[extra.pair].n * 4 <= kMaxPayload)
            both = engine.pair(slot.w, s.slots[extra.pair].w, slot.k, slot.n, s.slots[extra.pair].n, m);
        if (both) {
            const Slot & second = s.slots[extra.pair];
            std::memcpy(both->x->data, s.shared.base, both->input_bytes);
            if (!all_finite(static_cast<const float *>(both->x->data), m * slot.k)) throw Drop{"non-finite input"};
            unsigned char * const to[2] = {s.shared.base + kMaxPayload, s.shared.base + 2 * kMaxPayload};
            s.last.valid = false;
            const Outcome out = engine.compute(*both, slot.k, slot.n, second.n, m, to);
            r.f.a = kOk;
            r.f.b = 1;
            r.exchanges = out.exchanges;
            r.macs = out.macs;
        } else {
            Product & p = engine.product(slot.w, slot.k, slot.n, m);
            std::memcpy(p.x->data, s.shared.base, p.input_bytes);
            if (!all_finite(static_cast<const float *>(p.x->data), m * slot.k)) throw Drop{"non-finite input"};
            chain_learn(s, engine, f.id, m, p.x->data, p.input_bytes);
            const Outcome out = engine.compute(p, slot.k, slot.n, m, s.shared.base + kMaxPayload);
            if (out.supported) {
                r.f.a = kOk;
                r.exchanges = out.exchanges;
                r.macs = out.macs;
            }
        }
        if (both) {
            s.last.pair = both;
            s.last.stamp = both->stamp;
            s.last.generation = engine.generation();
            s.last.m = m;
            s.last.first = f.id;
            s.last.second = extra.pair;
            s.last.valid = true;
            // The continuation needs a processor of its own to be a gain: on by
            // default with four or more, SHIELD_Q8_AHEAD=0 or 1 decides otherwise.
            static const bool ahead = [] {
                if (const char * value = std::getenv("SHIELD_Q8_AHEAD")) return std::strcmp(value, "0") != 0;
                cpu_set_t set;
                return sched_getaffinity(0, sizeof set, &set) == 0 && CPU_COUNT(&set) >= 4;
            }();
            // Started before the reply so it overlaps the client's own work.
            // The products are complete and are only read from here on.
            if (ahead && f.id < s.chains.size() && s.chains[f.id].next != kNoPair) {
                const uint32_t first = f.id, second = extra.pair;
                ahead_thread.post([&s, &engine, first, second, m, both] { chain_ahead(s, engine, first, second, m, *both); });
            }
        }
        session_write(s.fd, &r, sizeof r);
        if (timing.enabled) { timing.replied = Timing::now(); timing.was_pair = both != nullptr; }
        if (timing.enabled && timing.seen > timing.skip) timing.request_ns += Timing::now() - t0;
        return;
    }
    if (f.type != kMatmul || f.id >= s.slots.size() || !s.slots[f.id].w || f.b != 0) throw Drop{"invalid frame"};
    const Slot & slot = s.slots[f.id];
    const uint64_t m = f.a;
    if (m == 0 || m > kMaxRows || m * slot.k * 4 > kMaxPayload || m * slot.n * 4 > kMaxPayload)
        throw Drop{"unsupported geometry"};
    Product & p = engine.product(slot.w, slot.k, slot.n, m);
    // Complete, validated private payload before any backend query.
    session_read(s.fd, p.x->data, p.input_bytes);
    if (!all_finite(static_cast<const float *>(p.x->data), m * slot.k)) throw Drop{"non-finite input"};
    const Outcome out = engine.compute(p, slot.k, slot.n, m);
    struct { Frame f; uint64_t exchanges, macs; } r{{kMatmul, f.id, out.supported ? kOk : kUnsupported, 0},
                                                    out.exchanges, out.macs};
    session_write(s.fd, &r, sizeof r);
    if (out.supported) session_write(s.fd, p.y->data, p.output_bytes);
    if (timing.enabled && timing.seen > timing.skip) timing.request_ns += Timing::now() - t0;
}

struct Totals {
    uint64_t v[7];
};

Totals totals(const Counters & base, const Engine & engine) {
    const Counters now = Counters::read();
    return {{now.offloaded - base.offloaded, now.local - base.local, now.graph_macs - base.graph_macs,
             now.failures, now.exchanges - base.exchanges, now.macs - base.macs, engine.completed()}};
}

void close_sessions(std::vector<Session> & sessions, bool bye) {
    for (Session & s : sessions) {
        if (bye) {
            const Frame f{kBye, 0, 0, 0};
            (void)!send(s.fd, &f, sizeof f, MSG_NOSIGNAL);
        }
        close(s.fd);
    }
    sessions.clear();
}

// Receive one control datagram and an optional passed descriptor.
ssize_t control_recv(int control, char & command, int & passed) {
    passed = -1;
    char cbuf[CMSG_SPACE(sizeof(int))];
    iovec iov{&command, 1};
    msghdr msg{};
    msg.msg_iov = &iov; msg.msg_iovlen = 1;
    msg.msg_control = cbuf; msg.msg_controllen = sizeof cbuf;
    ssize_t r;
    do r = recvmsg(control, &msg, MSG_CMSG_CLOEXEC); while (r < 0 && errno == EINTR);
    for (cmsghdr * c = CMSG_FIRSTHDR(&msg); r > 0 && c; c = CMSG_NXTHDR(&msg, c))
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS && c->cmsg_len == CMSG_LEN(sizeof(int)))
            std::memcpy(&passed, CMSG_DATA(c), sizeof(int));
    return r;
}

void control_send(int control, char tag, const Totals * t) {
    unsigned char buf[1 + sizeof(Totals)];
    buf[0] = (unsigned char)tag;
    if (t) std::memcpy(buf + 1, t, sizeof *t);
    (void)!send(control, buf, t ? sizeof buf : 1, MSG_NOSIGNAL);
}

int serve(int out_fd, int control, Model & model, const Policy & policy, Backend & backend) {
    Engine engine(model, policy, backend);
    const Counters base = Counters::read();
    Input in;
    std::string line;
    std::vector<Session> sessions;
    Ahead ahead;  // after the engine and the sessions its jobs use
    bool stdin_open = true;
    uint64_t served = 0;
    // SHIELD_Q8_SPIN_US bounds the wait described below; 0 always sleeps.
    const char * spin_env = std::getenv("SHIELD_Q8_SPIN_US");
    const uint64_t spin_ns = std::min<uint64_t>(spin_env ? std::strtoull(spin_env, nullptr, 10) : 0, 10000) * 1000;
    for (;;) {
        std::vector<pollfd> fds;
        fds.push_back({stdin_open ? STDIN_FILENO : -1, POLLIN, 0});
        fds.push_back({control, POLLIN, 0});
        for (const Session & s : sessions) fds.push_back({s.fd, POLLIN, 0});
        if (!stdin_open && control < 0) return 0;
        // A session's next request usually follows its last reply within
        // tens of microseconds. Waking from a blocking poll costs about as
        // much as the product itself, so look again briefly before sleeping.
        int ready = 0;
        if (!sessions.empty() && spin_ns) {
            const uint64_t until = Timing::now() + spin_ns;
            do {
                ready = poll(fds.data(), fds.size(), 0);
                if (ready != 0) break;
                __builtin_ia32_pause();
            } while (Timing::now() < until);
        }
        if (ready == 0)
            do ready = poll(fds.data(), fds.size(), -1); while (ready < 0 && errno == EINTR);
        if (ready < 0 && errno == EINTR) continue;
        if (ready < 0) retire("poll failed");
        // Whatever follows may use the engine or change the sessions.
        ahead.wait();

        if (fds[1].revents) {
            char command = 0;
            int passed = -1;
            const ssize_t r = control_recv(control, command, passed);
            if (r <= 0) {  // the service is gone: nobody is left to serve
                close_sessions(sessions, false);
                return 0;
            }
            if (command == 'A') {
                bool ok = passed >= 0 && sessions.size() < kMaxSessions;
                if (ok) {
                    // A stalled client delays the others by at most this long,
                    // then loses its session.
                    // The service's own timeouts leave the shared socket
                    // non-blocking; frames here are read whole.
                    const int flags = fcntl(passed, F_GETFL);
                    if (flags >= 0) fcntl(passed, F_SETFL, flags & ~O_NONBLOCK);
                    const timeval timeout{10, 0};
                    setsockopt(passed, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout);
                    setsockopt(passed, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof timeout);
                    sessions.emplace_back();
                    sessions.back().fd = passed;
                } else if (passed >= 0) {
                    close(passed);
                }
                control_send(control, ok ? 'a' : 'n', nullptr);
            } else if (command == 'P') {
                // Get ready before the first session; there is no reply.
                engine.prepare();
            } else if (command == 'S') {
                const Totals t = totals(base, engine);
                control_send(control, 's', &t);
            } else if (command == 'D') {
                close_sessions(sessions, true);
                const Totals t = totals(base, engine);
                control_send(control, 'F', &t);
                return 0;
            } else {
                if (passed >= 0) close(passed);
                retire("unknown control command");
            }
            continue;  // the session list may have changed
        }

        if (fds[0].revents) {
            // Drain every buffered request: poll cannot see Input's own buffer.
            do {
                if (in.read_line(line) == Input::Line::Eof) {
                    stdin_open = false;
                    if (control < 0) return 0;
                    break;
                }
                if (served++ >= kMaxRequests) retire("request limit reached");
                serve_stdin(in, line, out_fd, engine);
            } while (in.buffered());
        }

        for (size_t i = 0; i < sessions.size();) {
            if (!fds[2 + i].revents) { ++i; continue; }
            try {
                // Another session's request in this same pass may have started one.
                ahead.wait();
                session_request(sessions[i], engine, ahead);
                ++i;
            } catch (const Drop & drop) {
                if (std::strcmp(drop.why, "client closed"))
                    std::fprintf(stderr, "[q8-executor] session dropped: %s\n", drop.why);
                // A reply that could not be written may follow a job that
                // still uses this session.
                ahead.wait();
                close(sessions[i].fd);
                sessions.erase(sessions.begin() + (ptrdiff_t)i);
                fds.erase(fds.begin() + 2 + (ptrdiff_t)i);
            }
            if (engine.completed() >= kMaxSessionRequests) {
                ahead.wait();  // the totals below read the engine
                close_sessions(sessions, true);
                const Totals t = totals(base, engine);
                control_send(control, 'F', &t);
                return 0;
            }
        }
    }
}

int run(int out_fd, int control, const char * model_path) {
    const Policy policy = load_policy();
    Model model(model_path);
    Backend backend;  // destroyed before model: backend first, then metadata/mapping
    try {
        return serve(out_fd, control, model, policy, backend);
    } catch (const std::exception & e) {
        log_reason(dynamic_cast<const Retire *>(&e) ? e.what() : "internal error");
    } catch (...) {
        log_reason("internal error");
    }
    reply_failure(out_fd);
    return 2;
}

}  // namespace

int main(int argc, char ** argv) {
    // Protocol replies go to a private duplicate of stdout; anything native
    // code prints to stdout from here on lands on stderr instead.
    std::fflush(stdout);
    const int out_fd = fcntl(STDOUT_FILENO, F_DUPFD_CLOEXEC, 3);
    if (out_fd < 0 || dup2(STDERR_FILENO, STDOUT_FILENO) < 0) return 1;
    std::signal(SIGPIPE, SIG_IGN);

    try {
        int control = -1;
        if (argc == 4 && !std::strcmp(argv[2], "--control-fd")) {
            char * end = nullptr;
            const long fd = std::strtol(argv[3], &end, 10);
            if (!*argv[3] || *end || fd < 3 || fd > 1 << 20) retire("invalid control descriptor");
            control = (int)fd;
        } else if (argc != 2) {
            retire("usage: q8_executor MODEL_PATH [--control-fd N]");
        }
        if (!argv[1] || !*argv[1]) retire("usage: q8_executor MODEL_PATH [--control-fd N]");
        return run(out_fd, control, argv[1]);
    } catch (const std::exception & e) {
        log_reason(dynamic_cast<const Retire *>(&e) ? e.what() : "internal startup error");
    } catch (...) {
        log_reason("internal startup error");
    }
    reply_failure(out_fd);
    return 1;
}

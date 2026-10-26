// Persistent broker: original public Q8_0 GGUF tensor x private f32 activations
// through the existing ggml-shielded backend (shield-fixed-point-v1, approximate).
// The preloaded bootstrap authenticates the policy and installs the weight
// verifier and strict offload; this process re-checks the policy independently,
// hashes the mapped model bytes before any backend sees them, and speaks a
// line-delimited JSON + raw f32 protocol on stdin/stdout.
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
#include <cmath>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <iterator>
#include <map>
#include <memory>
#include <stdexcept>
#include <string>
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
constexpr uint64_t kMaxModel = 16ull << 30;
constexpr size_t kMaxTensor = 512u << 20;
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
    ggml_tensor * weight(const Policy & policy, const std::string & name,
                         const std::string & digest, int64_t k, int64_t n) {
        const auto pit = policy.find(name);
        if (pit == policy.end()) retire("tensor not in public policy");
        const PolicyEntry & entry = pit->second;
        if (entry.sha256 != digest) retire("request digest does not match policy");
        if (entry.type != GGML_TYPE_Q8_0 || entry.ne != std::array<int64_t, 4>{k, n, 1, 1})
            retire("request geometry does not match policy");

        if (const auto it = verified.find(name); it != verified.end()) return it->second;

        const int64_t id = gguf_find_tensor(gguf.get(), name.c_str());
        ggml_tensor * t = ggml_get_tensor(meta.get(), name.c_str());
        if (id < 0 || !t || std::strcmp(ggml_get_name(t), name.c_str()))
            retire("tensor not in model");
        const int64_t * gne = gguf_get_tensor_ne(gguf.get(), id);
        if (gguf_get_tensor_type(gguf.get(), id) != GGML_TYPE_Q8_0 || t->type != GGML_TYPE_Q8_0)
            retire("tensor is not Q8_0");
        for (int i = 0; i < 4; ++i)
            if (gne[i] != entry.ne[i] || t->ne[i] != entry.ne[i])
                retire("tensor geometry mismatch");
        const size_t bytes = gguf_get_tensor_size(gguf.get(), id);
        if (bytes == 0 || bytes > kMaxTensor || bytes != entry.nbytes || bytes != ggml_nbytes(t) ||
                bytes != ggml_row_size(GGML_TYPE_Q8_0, k) * (size_t)n || !ggml_is_contiguous(t))
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
        if (sha256_hex(bytes_ptr, bytes) != entry.sha256) retire("model tensor digest mismatch");

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
    static Counters read() {
        Counters c;
        ggml_backend_shielded_stats(&c.offloaded, &c.local, &c.graph_macs, &c.failures);
        ggml_backend_shielded_verified_stats(&c.exchanges, &c.macs);
        return c;
    }
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

using GraphContext = std::unique_ptr<ggml_context, decltype(&ggml_free)>;

int serve(int out_fd, Model & model, const Policy & policy, Backend & backend) {
    Input in;
    std::string line;
    for (uint64_t served = 0;; ++served) {
        if (in.read_line(line) == Input::Line::Eof) return 0;
        if (served >= kMaxRequests) retire("request limit reached");
        const Request req = parse_header(line);

        // Complete, validated private payload before any backend query.
        std::vector<float> input(req.m * req.k);
        in.read_exact(input.data(), req.input_bytes);
        for (float v : input)
            if (!std::isfinite(v)) retire("non-finite input");

        ggml_tensor * w = model.weight(policy, req.tensor, req.sha256, (int64_t)req.k, (int64_t)req.n);

        constexpr size_t kGraphNodes = 8;
        const size_t output_bytes = req.m * req.n * 4;
        const size_t mem = 4 * ggml_tensor_overhead() + ggml_graph_overhead_custom(kGraphNodes, false) +
                           req.input_bytes + output_bytes + (64u << 10);
        GraphContext ctx(ggml_init(ggml_init_params{mem, nullptr, false}), ggml_free);
        if (!ctx) retire("graph context allocation failed");
        ggml_tensor * x = ggml_new_tensor_2d(ctx.get(), GGML_TYPE_F32, (int64_t)req.k, (int64_t)req.m);
        if (!x || !x->data || ggml_nbytes(x) != req.input_bytes) retire("input tensor allocation failed");
        ggml_set_input(x);
        std::memcpy(x->data, input.data(), req.input_bytes);

        ggml_tensor * y = ggml_mul_mat(ctx.get(), w, x);
        if (!y || !y->data || y->type != GGML_TYPE_F32 || y->ne[0] != (int64_t)req.n ||
                y->ne[1] != (int64_t)req.m || y->ne[2] != 1 || y->ne[3] != 1 ||
                ggml_nbytes(y) != output_bytes || !ggml_is_contiguous(y))
            retire("output tensor allocation failed");
        ggml_set_output(y);
        // NaN sentinel: an output the backend never wrote fails the finite check.
        std::fill_n(static_cast<float *>(y->data), req.m * req.n, std::nanf(""));
        ggml_cgraph * graph = ggml_new_graph_custom(ctx.get(), kGraphNodes, false);
        if (!graph) retire("graph allocation failed");
        ggml_build_forward_expand(graph, y);

        if (!ggml_backend_supports_op(backend.backend, y)) {
            reply(out_fd, {{"ok", true}, {"supported", false}, {"outputBytes", 0}});
            continue;
        }

        const Counters before = Counters::read();
        if (before.failures != 0) retire("backend has verification failures");
        if (ggml_backend_graph_compute(backend.backend, graph) != GGML_STATUS_SUCCESS)
            retire("backend compute failed");
        const Counters after = Counters::read();
        if (after.offloaded < before.offloaded || after.local < before.local ||
                after.graph_macs < before.graph_macs || after.exchanges < before.exchanges ||
                after.macs < before.macs || after.failures != 0)
            retire("backend counters inconsistent");

        std::vector<float> output(req.m * req.n);
        std::memcpy(output.data(), y->data, output_bytes);
        ctx.reset();  // only after compute has returned
        for (float v : output)
            if (!std::isfinite(v)) retire("non-finite output");

        reply(out_fd,
              {{"ok", true},
               {"supported", true},
               {"outputBytes", output_bytes},
               {"arithmetic", kArithmetic},
               {"stats",
                {{"offloaded", after.offloaded - before.offloaded},
                 {"local", after.local - before.local},
                 {"graph_macs", after.graph_macs - before.graph_macs},
                 {"exchanges", after.exchanges - before.exchanges},
                 {"macs", after.macs - before.macs},
                 {"verification_failures", after.failures - before.failures}}}},
              output.data(), output_bytes);
    }
}

int run(int out_fd, const char * model_path) {
    const Policy policy = load_policy();
    Model model(model_path);
    Backend backend;  // destroyed before model: backend first, then metadata/mapping
    try {
        return serve(out_fd, model, policy, backend);
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
        if (argc != 2 || !argv[1] || !*argv[1]) retire("usage: q8_executor MODEL_PATH");
        return run(out_fd, argv[1]);
    } catch (const std::exception & e) {
        log_reason(dynamic_cast<const Retire *>(&e) ? e.what() : "internal startup error");
    } catch (...) {
        log_reason("internal startup error");
    }
    reply_failure(out_fd);
    return 1;
}

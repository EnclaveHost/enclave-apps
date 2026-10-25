// Loaded before the native engine: authenticate offload policy and install
// the verifier before any model graph can register public weights.
#include "ggml-backend.h"
#include "ggml-shielded.h"
#include "nlohmann/json.hpp"

#include <openssl/evp.h>
#include <array>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <dlfcn.h>
#include <fstream>
#include <map>
#include <string>
#include <vector>
#include <unistd.h>

struct PublicTensor {
    uint32_t type;
    std::array<int64_t, 4> ne;
    size_t bytes;
    std::string digest;
};
struct Policy {
    std::map<std::string, PublicTensor> tensors;
    void (*stats)(uint64_t *, uint64_t *, uint64_t *, uint64_t *) = nullptr;
    void (*verified_stats)(uint64_t *, uint64_t *) = nullptr;
};
static Policy & policy() { static Policy p; return p; }

[[noreturn]] static void fail(const char * reason) {
    std::fprintf(stderr, "[enclave-shield] startup refused: %s\n", reason);
    _exit(78);
}
static const char * required(const char * key) {
    const char * value = std::getenv(key);
    if (!value || !*value) fail(key);
    return value;
}
static std::string digest(const void * bytes, size_t size) {
    unsigned char out[EVP_MAX_MD_SIZE]; unsigned count = 0;
    if (!EVP_Digest(bytes, size, out, &count, EVP_sha256(), nullptr) || count != 32)
        fail("SHA-256 unavailable");
    static const char hex[] = "0123456789abcdef";
    std::string result(64, '0');
    for (unsigned i = 0; i < 32; ++i) {
        result[2*i] = hex[out[i] >> 4]; result[2*i+1] = hex[out[i] & 15];
    }
    return result;
}
static std::string read_bounded(const char * path, size_t limit) {
    std::ifstream file(path, std::ios::binary | std::ios::ate);
    if (!file || file.tellg() < 1 || (uint64_t)file.tellg() > limit) fail("invalid policy file size");
    std::string bytes((size_t)file.tellg(), '\0');
    file.seekg(0);
    if (!file.read(bytes.data(), bytes.size())) fail("policy read failed");
    return bytes;
}
static int verify_weight(void *, const char * name, uint32_t type, const int64_t ne[4],
                         const void * bytes, size_t size) noexcept {
    try {
        const auto it = policy().tensors.find(name ? name : "");
        if (it == policy().tensors.end() || !bytes || !ne) return -1;
        const auto & t = it->second;
        if (t.type != type || t.bytes != size || std::memcmp(t.ne.data(), ne, sizeof(int64_t)*4)) return -1;
        return digest(bytes, size) == t.digest ? 0 : -1;
    } catch (...) { return -1; }
}
static void report() {
    if (!policy().stats) return;
    uint64_t off=0, local=0, graph_macs=0, failures=0, exchanges=0, macs=0;
    policy().stats(&off, &local, &graph_macs, &failures);
    policy().verified_stats(&exchanges, &macs);
    std::fprintf(stderr, "[enclave-shield] offloaded=%llu local=%llu graph_macs=%llu exchanges=%llu macs=%llu verification_failures=%llu\n",
        (unsigned long long)off, (unsigned long long)local,
        (unsigned long long)graph_macs, (unsigned long long)exchanges,
        (unsigned long long)macs, (unsigned long long)failures);
}

__attribute__((constructor)) static void bootstrap() {
    try {
        if (std::strcmp(required("ENCLAVE_SHIELD_ARITHMETIC"), "shield-fixed-point-v1"))
            fail("explicit shield-fixed-point-v1 arithmetic required");
        const auto document = read_bounded(required("ENCLAVE_SHIELD_POLICY"), 16 << 20);
        if (digest(document.data(), document.size()) != required("ENCLAVE_SHIELD_POLICY_SHA256"))
            fail("policy digest mismatch");
        const auto j = nlohmann::json::parse(document);
        if (j.at("schema") != "shield-public-tensors/1") fail("unsupported policy");
        const auto calibration = read_bounded(required("SHIELDED_CALIB"), 4 << 20);
        if (digest(calibration.data(), calibration.size()) != j.at("calibrationSha256").get<std::string>())
            fail("calibration digest mismatch");
        for (const auto & entry : j.at("tensors")) {
            const auto name = entry.at("name").get<std::string>();
            PublicTensor t{entry.at("type").get<uint32_t>(), entry.at("ne").get<std::array<int64_t,4>>(),
                           entry.at("nbytes").get<size_t>(), entry.at("sha256").get<std::string>()};
            if (name.empty() || name.size() >= GGML_MAX_NAME || !t.bytes || t.digest.size() != 64 ||
                    !policy().tensors.emplace(name, t).second) fail("invalid public tensor record");
        }
        if (policy().tensors.empty()) fail("empty public policy");
        if (!ggml_backend_load(required("ENCLAVE_SHIELD_CPU"))) fail("CPU backend unavailable");
        const char * backend_path = required("ENCLAVE_SHIELD_BACKEND");
        auto reg = ggml_backend_load(backend_path);
        if (!reg || std::strcmp(ggml_backend_reg_name(reg), "Shielded")) fail("Shield backend unavailable");
        void * handle = dlopen(backend_path, RTLD_NOW | RTLD_NOLOAD);
        if (!handle) fail("Shield module handle unavailable");
        auto install = (int (*)(ggml_shielded_weight_verifier, void *))
            dlsym(handle, "ggml_backend_shielded_set_weight_verifier");
        policy().stats = (decltype(policy().stats))dlsym(handle, "ggml_backend_shielded_stats");
        policy().verified_stats = (decltype(policy().verified_stats))dlsym(handle, "ggml_backend_shielded_verified_stats");
        auto require_offload = (int (*)(void))dlsym(handle, "ggml_backend_shielded_require_offload");
        if (!install || !policy().stats || !policy().verified_stats || !require_offload ||
                install(verify_weight, nullptr) || require_offload())
            fail("weight verifier, verified accounting or strict offload policy unavailable");
        for (size_t i=0; i<ggml_backend_dev_count(); ++i) {
            auto dev = ggml_backend_dev_get(i);
            const auto type = ggml_backend_dev_type(dev);
            if (type != GGML_BACKEND_DEVICE_TYPE_CPU &&
                    (type != GGML_BACKEND_DEVICE_TYPE_ACCEL || std::strcmp(ggml_backend_dev_name(dev), "Shielded")))
                fail("unprotected compute backend present");
        }
        std::atexit(report);
        std::fprintf(stderr, "[enclave-shield] public policy authenticated; CPU and masked Shield available\n");
    } catch (...) { fail("invalid policy or backend configuration"); }
}

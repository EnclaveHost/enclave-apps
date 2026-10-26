// Test-only evaluator linked to the exact packaged llama/GGML libraries.
// Writes all teacher-forced logits for a bounded public text, never ships in VM.
// CPU and OpenCL use identical batch, attention and CPU KV settings. The native
// masked comparison injects the existing measured SDK bootstrap via environment.
#include "llama.h"
#include "ggml-backend.h"
#include <cassert>
#include <cstdio>
#include <fstream>
#include <iterator>
#include <string>
#include <vector>

int main(int argc, char **argv) {
    assert(argc == 5);
    const bool opencl = std::string(argv[4]) == "opencl";
    assert(opencl || std::string(argv[4]) == "cpu");
    std::ifstream input(argv[2]);
    std::string text((std::istreambuf_iterator<char>(input)), {});
    assert(!text.empty() && text.size() <= 8192);
    assert(ggml_backend_load("/opt/shield/lib/backends/libggml-cpu.so"));
    if (opencl) assert(ggml_backend_load("/opt/shield/apps/bin/libggml-opencl.so"));
    llama_backend_init();
    auto mp = llama_model_default_params();
    mp.n_gpu_layers = opencl ? 99 : 0;
    mp.use_extra_bufts = false;
    auto *model = llama_model_load_from_file(argv[1], mp);
    assert(model);
    auto *vocab = llama_model_get_vocab(model);
    std::vector<llama_token> tokens(1024);
    int n = llama_tokenize(vocab, text.data(), text.size(), tokens.data(), tokens.size(), true, true);
    assert(n > 1 && n <= 384);
    tokens.resize(n);
    auto cp = llama_context_default_params();
    cp.n_ctx = 1024;
    cp.offload_kqv = false;
    cp.flash_attn_type = LLAMA_FLASH_ATTN_TYPE_DISABLED;
    cp.n_batch = cp.n_ubatch = 32;
    cp.n_threads = cp.n_threads_batch = 2;
    auto *ctx = llama_init_from_model(model, cp);
    assert(ctx);
    const int32_t width = llama_vocab_n_tokens(vocab);
    auto *file = std::fopen(argv[3], "wb");
    assert(file);
    int32_t header[2] = {n, width};
    assert(std::fwrite(header, sizeof(int32_t), 2, file) == 2);
    assert(std::fwrite(tokens.data(), sizeof(llama_token), n, file) == (size_t)n);
    auto batch = llama_batch_init(32, 0, 1);
    for (int offset = 0; offset < n; offset += 32) {
        batch.n_tokens = std::min(32, n - offset);
        for (int i = 0; i < batch.n_tokens; ++i) {
            batch.token[i] = tokens[offset + i];
            batch.pos[i] = offset + i;
            batch.n_seq_id[i] = 1;
            batch.seq_id[i][0] = 0;
            batch.logits[i] = 1;
        }
        assert(llama_decode(ctx, batch) == 0);
        for (int i = 0; i < batch.n_tokens; ++i) {
            auto *logits = llama_get_logits_ith(ctx, i);
            assert(logits && std::fwrite(logits, sizeof(float), width, file) == (size_t)width);
        }
    }
    assert(std::fclose(file) == 0);
    llama_batch_free(batch);
    llama_free(ctx);
    llama_model_free(model);
    llama_backend_free();
    std::printf("{\"tokens\":%d,\"vocabulary\":%d,\"batch\":32}\n", n, width);
}

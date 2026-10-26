// Deterministic graph: CPU Q8 gather -> OpenCL Q8 matmul -> CPU ReLU.
#include "ggml.h"
#include "ggml-backend.h"
#include "ggml-alloc.h"
#include "core/ggml_graph_operations.h"
#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <vector>

static void require(bool value, const char * message) {
    if (!value) throw std::runtime_error(message);
}
int main(int argc, char ** argv) {
    try {
        require(argc == 3, "usage: test-opencl-cpu-fallback OPENCL_PLUGIN CPU_PLUGIN");
        setenv("GGML_SHIELD_NATIVE_CPU_FALLBACK", "1", 1);
        require(ggml_backend_load(argv[1]) != nullptr, "load OpenCL");
        require(ggml_backend_load(argv[2]) != nullptr, "load CPU");
        ggml_backend_t gpu = ggml_backend_init_by_type(GGML_BACKEND_DEVICE_TYPE_GPU, nullptr);
        ggml_backend_t cpu = ggml_backend_init_by_type(GGML_BACKEND_DEVICE_TYPE_CPU, nullptr);
        require(gpu && cpu, "init both backends");
        constexpr int k = 1024, n = 2048, m = 2;
        ggml_context * weights_ctx = ggml_init({ggml_tensor_overhead()*4, nullptr, true});
        ggml_tensor * weights = ggml_new_tensor_2d(weights_ctx, GGML_TYPE_Q8_0, k, n);
        ggml_backend_buffer_t weights_buffer = ggml_backend_alloc_ctx_tensors(weights_ctx, gpu);
        require(weights_buffer != nullptr, "allocate weights");
        ggml_backend_buffer_set_usage(weights_buffer, GGML_BACKEND_BUFFER_USAGE_WEIGHTS);
        constexpr int row_bytes = k/32*34;
        std::vector<uint8_t> raw(n*row_bytes);
        auto code = [](int row, int col) { return (row*3+col)%17-8; };
        const ggml_fp16_t scale = ggml_fp32_to_fp16(0.125f);
        for (int row=0; row<n; ++row) {
            for (int block=0; block<k/32; ++block) {
                std::memcpy(raw.data()+row*row_bytes+block*34, &scale, 2);
                for (int col=0; col<32; ++col)
                    raw[row*row_bytes+block*34+2+col] = uint8_t(int8_t(code(row,block*32+col)));
            }
        }
        ggml_backend_tensor_set(weights, raw.data(), 0, raw.size());
        ggml_context * ctx = ggml_init({1<<20, nullptr, true});
        ggml_tensor * ids = ggml_new_tensor_1d(ctx, GGML_TYPE_I32, m);
        ggml_set_input(ids);
        ggml_tensor * gathered = ggml_get_rows(ctx, weights, ids);
        ggml_tensor * product = ggml_mul_mat(ctx, weights, gathered);
        ggml_tensor * bias = ggml_new_tensor_1d(ctx, GGML_TYPE_F32, 1);
        ggml_set_input(bias);
        require(sd_graph_add_inplace(ctx, product, bias)->view_src == product,
                "default graph must preserve in-place request");
        ggml_tensor * shifted = nullptr;
        ggml_tensor * output = nullptr;
        {
            SDGraphOutOfPlaceScope scope(ctx, true, cpu);
            ggml_tensor * local_weights = ggml_new_tensor_2d(ctx, GGML_TYPE_F16, k, n);
            ggml_tensor * local_product = ggml_mul_mat(ctx, local_weights, gathered);
            require(!ggml_backend_supports_op(gpu, local_product) &&
                    sd_graph_supports_op(ctx, gpu, local_product),
                    "graph capabilities must include the configured CPU fallback");
            shifted = sd_graph_add_inplace(ctx, product, bias);
            require(shifted->view_src == nullptr, "mixed-backend temporary must get its own output");
            ggml_tensor * reshaped = ggml_reshape_2d(ctx, shifted, n, m);
            output = sd_graph_relu_inplace(ctx, reshaped);
            require(output->view_src == nullptr, "view of temporary must get its own output");
            require(sd_graph_add_inplace(ctx, bias, bias)->view_src == bias,
                    "persistent input mutation must retain its semantics");
        }
        require(sd_graph_add_inplace(ctx, product, bias)->view_src == product,
                "scope must restore the original construction policy");
        ggml_set_output(output);
        ggml_tensor * small_w = ggml_new_tensor_2d(ctx, GGML_TYPE_Q8_0, 32, 128);
        ggml_tensor * small_x = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, 32, 2);
        require(!ggml_backend_supports_op(gpu, ggml_mul_mat(ctx, small_w, small_x)),
                "small Q8 product must remain on native CPU");
        ggml_tensor * f16 = ggml_new_tensor_2d(ctx, GGML_TYPE_F16, k, n);
        ggml_tensor * f16_product = ggml_mul_mat(ctx, f16, gathered);
        require(!ggml_backend_supports_op(gpu, gathered), "Q8 gather must decline OpenCL");
        require(ggml_backend_supports_op(gpu, product), "Q8 matmul must remain supported");
        require(!ggml_backend_supports_op(gpu, f16_product), "F16 matmul must use native CPU");
        require(!sd_graph_supports_op(ctx, gpu, f16_product), "fallback capability must not escape its graph scope");
        require(!ggml_backend_supports_op(gpu, output), "ReLU must use native CPU");
        ggml_cgraph * graph = ggml_new_graph_custom(ctx, 128, false);
        ggml_build_forward_expand(graph, output);
        ggml_backend_t backends[] = {gpu, cpu};
        ggml_backend_buffer_type_t bufts[] = {
            ggml_backend_get_default_buffer_type(gpu), ggml_backend_get_default_buffer_type(cpu)};
        ggml_backend_sched_t sched = ggml_backend_sched_new(backends, bufts, 2, 128, false, false);
        require(sched && ggml_backend_sched_alloc_graph(sched, graph), "allocate graph");
        require(ggml_backend_sched_get_tensor_backend(sched, gathered) == cpu, "gather placement");
        require(ggml_backend_sched_get_tensor_backend(sched, product) == gpu, "matmul placement");
        require(ggml_backend_sched_get_tensor_backend(sched, shifted) == cpu, "add placement");
        require(ggml_backend_sched_get_tensor_backend(sched, output) == cpu, "ReLU placement");
        const float bias_value = 0.25f;
        ggml_backend_tensor_set(bias, &bias_value, 0, sizeof(bias_value));
        int32_t selected[m] = {1, 5};
        ggml_backend_tensor_set(ids, selected, 0, sizeof(selected));
        require(ggml_backend_sched_graph_compute(sched, graph) == GGML_STATUS_SUCCESS, "compute graph");
        ggml_backend_sched_synchronize(sched);
        std::vector<float> actual(n*m);
        ggml_backend_tensor_get(output, actual.data(), 0, actual.size()*sizeof(float));
        for (int batch=0; batch<m; ++batch) for (int row=0; row<n; ++row) {
            float expected = 0;
            for (int col=0; col<k; ++col) expected += code(row,col)*code(selected[batch],col)*0.015625f;
            require(actual[batch*n+row] == std::max(0.0f,expected+bias_value), "exact graph output mismatch");
        }
        std::puts("{\"passed\":true,\"outputs\":4096,\"gather\":\"cpu\",\"q8Matmul\":\"opencl\",\"relu\":\"cpu\",\"f16MatmulDeclined\":true}");
        ggml_backend_sched_free(sched); ggml_free(ctx);
        ggml_backend_buffer_free(weights_buffer); ggml_free(weights_ctx);
        ggml_backend_free(gpu); ggml_backend_free(cpu);
        return 0;
    } catch (const std::exception & e) { std::fprintf(stderr,"FAIL: %s\n",e.what()); return 1; }
}

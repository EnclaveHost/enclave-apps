// Execute the plain IQ4_XS, Q3_K and IQ3_S kernels (q8-reference/, added to
// llama's OpenCL backend by native/llama-shield-iq4-xs.patch) on the CPU
// runtime and compare each output with the product of the weights
// q8_kquant.inc decodes from the same bytes. test_q8_kquant.py checks that
// decoding against gguf-py, so the two together check the kernels.
//
//   c++ -std=c++20 -O2 test_q8_raw_kernels.cpp -I. -l:libOpenCL-shield-cpu.so.1 -o test-q8-raw-kernels
//   test-q8-raw-kernels q8-reference
#define CL_TARGET_OPENCL_VERSION 300
#include <CL/cl.h>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <random>
#include <string>
#include <vector>

#include "q8_kquant.inc"

static void check(cl_int code, const char *phase) {
    if (code) { std::fprintf(stderr, "%s: %d\n", phase, code); std::exit(1); }
}
struct Kind { const char *file, *name; size_t block, d_at, part; };

int main(int argc, char **argv) {
    if (argc != 2) { std::fprintf(stderr, "usage: test-q8-raw-kernels Q8_REFERENCE_DIRECTORY\n"); return 2; }
    cl_platform_id platform; cl_device_id device; cl_int error;
    check(clGetPlatformIDs(1, &platform, nullptr), "platform");
    check(clGetDeviceIDs(platform, CL_DEVICE_TYPE_ALL, 1, &device, nullptr), "device");
    auto context = clCreateContext(nullptr, 1, &device, nullptr, nullptr, &error); check(error, "context");
    auto queue = clCreateCommandQueueWithProperties(context, device, nullptr, &error); check(error, "queue");
    const Kind kinds[] = {{"mul_mat_iq4_xs_f32_raw.cl", "kernel_mul_mat_iq4_xs_f32_raw", 136, 0, 32},
                          {"mul_mat_q3_k_f32_raw.cl", "kernel_mul_mat_q3_k_f32_raw", 110, 108, 16},
                          {"mul_mat_iq3_s_f32_raw.cl", "kernel_mul_mat_iq3_s_f32_raw", 110, 0, 32}};
    std::mt19937_64 rng(20261120);
    unsigned checked = 0;
    for (const Kind &kind : kinds) for (bool fast : {false, true}) {
        std::ifstream stream(std::string(argv[1]) + "/" + kind.file);
        if (!stream) return 2;
        std::string source((std::istreambuf_iterator<char>(stream)), {});
        const char *text = source.c_str(); size_t length = source.size();
        auto program = clCreateProgramWithSource(context, 1, &text, &length, &error); check(error, "program");
        const char *options = fast ? "-cl-std=CL3.0 -cl-mad-enable -cl-unsafe-math-optimizations -cl-finite-math-only -cl-fast-relaxed-math" : "-cl-std=CL3.0";
        error = clBuildProgram(program, 1, &device, options, nullptr, nullptr);
        if (error) {
            size_t bytes = 0; clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &bytes);
            std::vector<char> log(bytes + 1, 0);
            clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, bytes, log.data(), nullptr);
            std::fprintf(stderr, "%s\n", log.data());
            return 1;
        }
        auto kernel = clCreateKernel(program, kind.name, &error); check(error, "kernel");
        // k, n, m and the three byte offsets; one batch wider than a block row.
        const size_t shapes[][6] = {{256, 1, 1, 0, 0, 0}, {512, 37, 3, 64, 32, 16}, {1024, 5, 9, 2, 4, 8}};
        for (const auto &shape : shapes) {
            const size_t k = shape[0], n = shape[1], m = shape[2], blocks = k * n / 256;
            std::vector<unsigned char> weights(shape[3] + blocks * kind.block);
            for (auto &byte : weights) byte = (unsigned char)rng();
            for (size_t ib = 0; ib < blocks; ++ib) {
                // An ordinary finite half scale, either sign: sign, exponent 8..15, any fraction.
                const uint16_t half = uint16_t(((rng() & 1) << 15) | ((8 + rng() % 8) << 10) | (rng() & 1023));
                std::memcpy(weights.data() + shape[3] + ib * kind.block + kind.d_at, &half, 2);
            }
            std::vector<float> input(shape[4] / 4 + m * k), output(shape[5] / 4 + m * n, -7.0f);
            std::uniform_real_distribution<float> uniform(-1.0f, 1.0f);
            for (auto &value : input) value = uniform(rng);
            auto w = clCreateBuffer(context, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR, weights.size(), weights.data(), &error); check(error, "weights");
            auto x = clCreateBuffer(context, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR, input.size() * 4, input.data(), &error); check(error, "input");
            auto y = clCreateBuffer(context, CL_MEM_READ_WRITE | CL_MEM_COPY_HOST_PTR, output.size() * 4, output.data(), &error); check(error, "output");
            const cl_ulong o0 = shape[3], o1 = shape[4], od = shape[5];
            const cl_int ne00 = cl_int(k), ne01 = cl_int(n), ne11 = cl_int(m);
            check(clSetKernelArg(kernel, 0, sizeof(w), &w), "arg");
            check(clSetKernelArg(kernel, 1, sizeof(o0), &o0), "arg");
            check(clSetKernelArg(kernel, 2, sizeof(x), &x), "arg");
            check(clSetKernelArg(kernel, 3, sizeof(o1), &o1), "arg");
            check(clSetKernelArg(kernel, 4, sizeof(y), &y), "arg");
            check(clSetKernelArg(kernel, 5, sizeof(od), &od), "arg");
            check(clSetKernelArg(kernel, 6, sizeof(ne00), &ne00), "arg");
            check(clSetKernelArg(kernel, 7, sizeof(ne01), &ne01), "arg");
            check(clSetKernelArg(kernel, 8, sizeof(ne11), &ne11), "arg");
            const size_t global[3] = {n, m, 1}, local[3] = {1, 1, 1};
            check(clEnqueueNDRangeKernel(queue, kernel, 3, nullptr, global, local, 0, nullptr, nullptr), "enqueue");
            check(clEnqueueReadBuffer(queue, y, CL_TRUE, 0, output.size() * 4, output.data(), 0, nullptr, nullptr), "read");

            std::vector<int8_t> codes(k * n);
            std::vector<float> scales(k * n / kind.part);
            const unsigned char *bytes = weights.data() + shape[3];
            if (kind.block == 136) q8_decode_iq4_xs(bytes, blocks, codes.data(), scales.data());
            else if (kind.d_at) q8_decode_q3_k(bytes, blocks, codes.data(), scales.data());
            else q8_decode_iq3_s(bytes, blocks, codes.data(), scales.data());
            for (size_t i = 0; i < shape[5] / 4; ++i)
                if (output[i] != -7.0f) { std::fprintf(stderr, "%s wrote before its output\n", kind.name); return 1; }
            for (size_t r = 0; r < m; ++r) for (size_t j = 0; j < n; ++j) {
                double expected = 0, magnitude = 0;
                for (size_t i = 0; i < k; ++i) {
                    const double term = double(scales[(j * k + i) / kind.part]) * codes[j * k + i] *
                                        input[shape[4] / 4 + r * k + i];
                    expected += term;
                    magnitude += std::fabs(term);
                }
                const double actual = output[shape[5] / 4 + r * n + j];
                // Float sums in another order: a few parts in 1e5 of the terms' size.
                if (!(std::fabs(actual - expected) <= 2e-5 * magnitude + 1e-30)) {
                    std::fprintf(stderr, "%s%s k=%zu n=%zu m=%zu row %zu output %zu: %.9g, expected %.9g\n", kind.name,
                                 fast ? " (fast)" : "", k, n, m, r, j, actual, expected);
                    return 1;
                }
                ++checked;
            }
            clReleaseMemObject(w); clReleaseMemObject(x); clReleaseMemObject(y);
        }
        clReleaseKernel(kernel); clReleaseProgram(program);
    }
    std::printf("plain kernels agree with the decoded weights: %u outputs\n", checked);
    return 0;
}

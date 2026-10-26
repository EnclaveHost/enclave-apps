// Regression test: raw Q8_0 bytes read back from an OpenCL weights buffer must
// match what was uploaded, for the whole tensor, for views at nonzero offsets
// (contiguous, nested and strided), for partial byte-range reads, and for a
// scheduler-style copy of a view into a CPU buffer.
//
// The OpenCL backend stores uploaded Q8_0 weights as SoA (scales and quants in
// separate sub-buffers) and has to restore the AoS block layout on readback.
// Before the fix, views ignored view_offs and returned the parent's leading
// bytes.
//
// usage:   test_q8_views <path/to/libggml-opencl.so> <path/to/libggml-cpu.so>
// needs:   a GGML_BACKEND_DL build (backends as loadable modules)
// exit:    0 pass, 1 fail, 2 usage, 77 skip (no OpenCL device)

#include "ggml.h"
#include "ggml-alloc.h"
#include "ggml-backend.h"

#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>

namespace {

constexpr int64_t NE0   = 256;     // 8 Q8_0 blocks per row
constexpr int64_t NE1   = 16;      // 128 blocks: a multiple of the SoA convert kernel's local size (64)
constexpr size_t  QK8_0 = 32;
constexpr size_t  BLK   = 34;      // sizeof(block_q8_0): fp16 d + 32 x int8 qs

int g_failures = 0;

#define REQUIRE(cond, ...) do {                                         \
        if (!(cond)) {                                                  \
            fprintf(stderr, "FATAL %s:%d: ", __FILE__, __LINE__);       \
            fprintf(stderr, __VA_ARGS__);                               \
            fputc('\n', stderr);                                        \
            return 1;                                                   \
        }                                                               \
    } while (0)

// Deterministic, public fixture. Every block has a distinct quant pattern
// (37 is odd, so b*37 mod 256 is unique for b < 256) and a finite, nonzero,
// exactly representable fp16 scale, stored little-endian as in block_q8_0.
std::vector<uint8_t> make_fixture(size_t n_blocks) {
    std::vector<uint8_t> out(n_blocks*BLK);
    for (size_t b = 0; b < n_blocks; ++b) {
        uint8_t * blk = out.data() + b*BLK;
        const ggml_fp16_t d = ggml_fp32_to_fp16(0.0078125f * (float) (1 + b % 13));
        blk[0] = (uint8_t) (d & 0xFF);
        blk[1] = (uint8_t) (d >> 8);
        for (size_t i = 0; i < QK8_0; ++i) {
            blk[2 + i] = (uint8_t) ((b*37 + i*11 + 5) & 0xFF);
        }
    }
    return out;
}

bool fixture_scales_finite(const std::vector<uint8_t> & fx) {
    for (size_t off = 0; off < fx.size(); off += BLK) {
        const ggml_fp16_t d = (ggml_fp16_t) (fx[off] | (fx[off + 1] << 8));
        const float f = ggml_fp16_to_fp32(d);
        if (!std::isfinite(f) || f == 0.0f) {
            return false;
        }
    }
    return true;
}

// Reads [offset, offset + size) of `t` via ggml_backend_tensor_get into a
// poisoned buffer with a trailing guard and compares against fixture bytes
// starting at `want_off` (absolute offset in the parent).
void check_get(const char * what, const ggml_tensor * t, size_t offset, size_t size,
               const std::vector<uint8_t> & fx, size_t want_off) {
    constexpr size_t GUARD = 16;
    std::vector<uint8_t> got(size + GUARD, 0xA5);
    ggml_backend_tensor_get(t, got.data(), offset, size);

    for (size_t i = 0; i < GUARD; ++i) {
        if (got[size + i] != 0xA5) {
            fprintf(stderr, "FAIL %s: wrote past requested size (guard byte %zu)\n", what, i);
            ++g_failures;
            return;
        }
    }
    if (want_off + size > fx.size()) {
        fprintf(stderr, "FAIL %s: test bug, expected range out of fixture\n", what);
        ++g_failures;
        return;
    }
    if (memcmp(got.data(), fx.data() + want_off, size) == 0) {
        printf("ok   %s (%zu bytes @ parent+%zu)\n", what, size, want_off);
        return;
    }

    size_t i = 0;
    while (got[i] == fx[want_off + i]) {
        ++i;
    }
    fprintf(stderr, "FAIL %s: first mismatch at byte %zu (parent byte %zu, block %zu): got 0x%02x want 0x%02x\n",
            what, i, want_off + i, (want_off + i)/BLK, got[i], fx[want_off + i]);
    // Signature of the original bug: the bytes are the parent's, read at
    // `offset` without the view's view_offs.
    if (t->view_offs != 0 && offset + size <= fx.size() &&
        memcmp(got.data(), fx.data() + offset, size) == 0) {
        fprintf(stderr, "     %s: data equals parent bytes at +%zu, i.e. view_offs (%zu) was ignored\n",
                what, offset, t->view_offs);
    }
    ++g_failures;
}

ggml_tensor * make_view_2d(ggml_context * ctx, ggml_tensor * src, int64_t ne0, int64_t ne1,
                           size_t nb1, size_t offs, const char * name) {
    ggml_tensor * v = ggml_view_2d(ctx, src, ne0, ne1, nb1, offs);
    ggml_set_name(v, name);
    return v;
}

} // namespace

int main(int argc, char ** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <libggml-opencl path> <libggml-cpu path>\n", argv[0]);
        return 2;
    }

    REQUIRE(ggml_type_size(GGML_TYPE_Q8_0) == BLK && ggml_blck_size(GGML_TYPE_Q8_0) == (int64_t) QK8_0,
            "unexpected Q8_0 block layout");

    const size_t n_blocks = (size_t) (NE0*NE1) / QK8_0;
    const std::vector<uint8_t> fx = make_fixture(n_blocks);
    REQUIRE(fixture_scales_finite(fx), "fixture has non-finite or zero scales");
    REQUIRE(ggml_validate_row_data(GGML_TYPE_Q8_0, fx.data(), fx.size()), "fixture rejected by ggml_validate_row_data");

    ggml_backend_reg_t reg_cl = ggml_backend_load(argv[1]);
    REQUIRE(reg_cl != nullptr, "failed to load OpenCL backend from %s", argv[1]);
    ggml_backend_reg_t reg_cpu = ggml_backend_load(argv[2]);
    REQUIRE(reg_cpu != nullptr, "failed to load CPU backend from %s", argv[2]);
    printf("loaded %s and %s\n", ggml_backend_reg_name(reg_cl), ggml_backend_reg_name(reg_cpu));

    if (ggml_backend_reg_dev_count(reg_cl) == 0) {
        printf("SKIP: no OpenCL device\n");
        ggml_backend_unload(reg_cpu);
        ggml_backend_unload(reg_cl);
        return 77;
    }

    // The OpenCL buffer type reads per-device state that is created when the
    // backend is initialized, and readback synchronizes with every OpenCL
    // device, so initialize all of them before allocating.
    std::vector<ggml_backend_t> cl_backends;
    for (size_t i = 0; i < ggml_backend_reg_dev_count(reg_cl); ++i) {
        ggml_backend_t be = ggml_backend_dev_init(ggml_backend_reg_dev_get(reg_cl, i), nullptr);
        REQUIRE(be != nullptr, "failed to init OpenCL device %zu", i);
        cl_backends.push_back(be);
    }
    ggml_backend_dev_t dev_cl = ggml_backend_reg_dev_get(reg_cl, 0);
    ggml_backend_t     be_cl  = cl_backends[0];
    printf("OpenCL device: %s (%s)\n", ggml_backend_dev_name(dev_cl), ggml_backend_dev_description(dev_cl));

    REQUIRE(ggml_backend_reg_dev_count(reg_cpu) > 0, "CPU backend has no device");
    ggml_backend_dev_t dev_cpu = ggml_backend_reg_dev_get(reg_cpu, 0);
    REQUIRE(ggml_backend_dev_type(dev_cpu) == GGML_BACKEND_DEVICE_TYPE_CPU, "second module is not a CPU backend");
    ggml_backend_t be_cpu = ggml_backend_dev_init(dev_cpu, nullptr);
    REQUIRE(be_cpu != nullptr, "failed to init CPU backend");

    // Weight tensor in an OpenCL buffer marked as weights.
    ggml_init_params ip_w = { ggml_tensor_overhead()*2, nullptr, true };
    ggml_context * ctx_w = ggml_init(ip_w);
    REQUIRE(ctx_w != nullptr, "ggml_init failed");
    ggml_tensor * w = ggml_new_tensor_2d(ctx_w, GGML_TYPE_Q8_0, NE0, NE1);
    ggml_set_name(w, "q8_fixture.weight");
    REQUIRE(ggml_nbytes(w) == fx.size(), "unexpected tensor size %zu", ggml_nbytes(w));

    ggml_backend_buffer_t buf_w = ggml_backend_alloc_ctx_tensors_from_buft(ctx_w, ggml_backend_dev_buffer_type(dev_cl));
    REQUIRE(buf_w != nullptr, "failed to allocate OpenCL weights buffer");
    ggml_backend_buffer_set_usage(buf_w, GGML_BACKEND_BUFFER_USAGE_WEIGHTS);
    REQUIRE(!ggml_backend_buffer_is_host(buf_w), "OpenCL buffer is host memory; test would not exercise device readback");

    // Upload in a single full-size write: the backend converts Q8_0 to SoA on
    // set_tensor using the whole tensor, so partial uploads are not supported.
    ggml_backend_tensor_set(w, fx.data(), 0, fx.size());

    // Views over the uploaded weight, initialized the way the graph allocator
    // initializes views of pre-allocated tensors.
    ggml_init_params ip_v = { ggml_tensor_overhead()*8, nullptr, true };
    ggml_context * ctx_v = ggml_init(ip_v);
    REQUIRE(ctx_v != nullptr, "ggml_init failed");

    const size_t nb1 = w->nb[1];
    ggml_tensor * v_rows    = make_view_2d(ctx_v, w, NE0, 4, nb1, 5*nb1, "rows5_8");
    ggml_tensor * v_nested  = make_view_2d(ctx_v, v_rows, NE0, 2, v_rows->nb[1], 1*v_rows->nb[1], "rows6_7_nested");
    ggml_tensor * v_tail    = make_view_2d(ctx_v, w, NE0, 1, nb1, (size_t) (NE1 - 1)*nb1, "last_row");
    ggml_tensor * v_strided = make_view_2d(ctx_v, w, 4*QK8_0, 4, 2*nb1, 3*nb1 + 2*BLK, "strided");
    ggml_tensor * v_1d      = ggml_view_1d(ctx_v, w, 3*QK8_0, 13*BLK);
    ggml_set_name(v_1d, "1d_blocks13_15");

    ggml_tensor * views[] = { v_rows, v_nested, v_tail, v_strided, v_1d };
    for (ggml_tensor * v : views) {
        REQUIRE(ggml_backend_view_init(v) == GGML_STATUS_SUCCESS, "view_init failed for %s", v->name);
        REQUIRE(v->buffer == buf_w && v->view_src == w, "view %s not rooted at the weight", v->name);
        REQUIRE(v->view_offs + ggml_nbytes(v) <= ggml_nbytes(w), "view %s exceeds parent", v->name);
    }
    // ggml roots nested views at the owner and accumulates the offset.
    REQUIRE(v_nested->view_offs == 6*nb1, "nested view offset %zu", v_nested->view_offs);
    REQUIRE(ggml_is_contiguous(v_rows) && ggml_is_contiguous(v_nested) && ggml_is_contiguous(v_1d),
            "contiguous views are not contiguous");
    REQUIRE(!ggml_is_contiguous(v_strided), "strided view is contiguous");

    // 1. Whole tensor: full and byte-unaligned partial reads.
    check_get("whole",            w, 0, fx.size(), fx, 0);
    check_get("whole partial",    w, 7, 100, fx, 7);

    // 2. Views at nonzero offsets, full reads. For a non-contiguous view the
    //    readback is the raw byte span starting at tensor->data, as for CPU
    //    and AoS buffers.
    for (ggml_tensor * v : views) {
        check_get(v->name, v, 0, ggml_nbytes(v), fx, v->view_offs);
    }

    // 3. Partial reads inside views: unaligned starts, block-straddling
    //    ranges, a read ending exactly at the parent's end.
    check_get("rows5_8 partial mid-block",    v_rows, 7, 3*BLK + 5, fx, v_rows->view_offs + 7);
    check_get("rows5_8 partial scale bytes",  v_rows, BLK - 1, 2, fx, v_rows->view_offs + BLK - 1);
    check_get("rows6_7_nested partial",       v_nested, nb1 + 1, nb1 - 1, fx, v_nested->view_offs + nb1 + 1);
    check_get("last_row partial to end",      v_tail, nb1 - 3, 3, fx, fx.size() - 3);
    check_get("strided partial",              v_strided, 2*nb1 + 3, 50, fx, v_strided->view_offs + 2*nb1 + 3);

    // 4. Scheduler-style copies into a CPU buffer. ggml_backend_tensor_copy_async
    //    is what the scheduler issues for split inputs; with no async copy it
    //    syncs both backends and falls back to get_tensor on the source.
    ggml_init_params ip_c = { ggml_tensor_overhead()*4, nullptr, true };
    ggml_context * ctx_c = ggml_init(ip_c);
    REQUIRE(ctx_c != nullptr, "ggml_init failed");
    ggml_tensor * c_w      = ggml_new_tensor_2d(ctx_c, GGML_TYPE_Q8_0, NE0, NE1);
    ggml_tensor * c_rows   = ggml_new_tensor_2d(ctx_c, GGML_TYPE_Q8_0, NE0, 4);
    ggml_tensor * c_nested = ggml_new_tensor_2d(ctx_c, GGML_TYPE_Q8_0, NE0, 2);
    ggml_tensor * c_1d     = ggml_new_tensor_1d(ctx_c, GGML_TYPE_Q8_0, 3*QK8_0);
    ggml_backend_buffer_t buf_c = ggml_backend_alloc_ctx_tensors_from_buft(ctx_c, ggml_backend_dev_buffer_type(dev_cpu));
    REQUIRE(buf_c != nullptr, "failed to allocate CPU buffer");
    ggml_backend_buffer_clear(buf_c, 0);

    ggml_backend_tensor_copy(w, c_w);
    ggml_backend_tensor_copy_async(be_cl, be_cpu, v_rows,   c_rows);
    ggml_backend_tensor_copy_async(be_cl, be_cpu, v_nested, c_nested);
    ggml_backend_tensor_copy_async(be_cl, be_cpu, v_1d,     c_1d);
    ggml_backend_synchronize(be_cl);
    ggml_backend_synchronize(be_cpu);

    check_get("cpu copy whole",          c_w,      0, ggml_nbytes(c_w),      fx, 0);
    check_get("cpu copy rows5_8",        c_rows,   0, ggml_nbytes(c_rows),   fx, v_rows->view_offs);
    check_get("cpu copy rows6_7_nested", c_nested, 0, ggml_nbytes(c_nested), fx, v_nested->view_offs);
    check_get("cpu copy 1d",             c_1d,     0, ggml_nbytes(c_1d),     fx, v_1d->view_offs);

    // 5. Readback must not disturb the device copy.
    check_get("whole after views", w, 0, fx.size(), fx, 0);

    // Free buffers before the contexts that describe them and before
    // unloading the modules whose code implements the buffers.
    ggml_backend_buffer_free(buf_c);
    ggml_free(ctx_c);
    ggml_free(ctx_v);
    ggml_backend_buffer_free(buf_w);
    ggml_free(ctx_w);
    ggml_backend_free(be_cpu);
    for (ggml_backend_t be : cl_backends) {
        ggml_backend_free(be);
    }
    ggml_backend_unload(reg_cpu);
    ggml_backend_unload(reg_cl);

    if (g_failures != 0) {
        fprintf(stderr, "%d check(s) failed\n", g_failures);
        return 1;
    }
    printf("all checks passed\n");
    return 0;
}

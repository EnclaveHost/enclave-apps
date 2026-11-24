// EXPERIMENT (SHIELD_OPENCL_GLUE=1). The arithmetic an application does between two products of a
// residual stream (an addition, a multiplication by a row of weights, a root-mean-square norm, the gate
// of a feed-forward block) computed here, on the bytes of the device buffers, when the application
// launches its own kernel for it. With these on the device the stream does not have to go back to the
// application's processor backend between the products: no read and write of every activation, and no
// separate little graph for the processor each time.
//
// Kernels are known by name, which is enough for an experiment and not for a release: a release would
// bind the kernel's source as the product kernels' is bound. Each one below does what the kernel of that
// name in llama.cpp's OpenCL backend does for 32-bit floats; a launch with any other geometry than the
// ones that backend uses, or that the bytes of its buffers do not cover, is left to the runtime. Nothing
// here is protected work: these are sums and products of values the application already holds.
enum GlueKind { GLUE_NONE = 0, GLUE_ADD, GLUE_ADD_ROW, GLUE_MUL, GLUE_MUL_ROW, GLUE_RMS_NORM, GLUE_SWIGLU };
bool glue_enabled() {
  static const bool enabled = [] {
    const char *value = std::getenv("SHIELD_OPENCL_GLUE");
    return value && std::strcmp(value, "0") != 0;
  }();
  return enabled;
}
int glue_kind_of(cl_kernel kernel) {
  char name[64] = {};
  if (REAL(clGetKernelInfo)(kernel, CL_KERNEL_FUNCTION_NAME, sizeof(name) - 1, name, nullptr)) return GLUE_NONE;
  static const std::pair<const char *, int> known[] = {
      {"kernel_add", GLUE_ADD},         {"kernel_add_row", GLUE_ADD_ROW},   {"kernel_mul", GLUE_MUL},
      {"kernel_mul_row", GLUE_MUL_ROW}, {"kernel_rms_norm", GLUE_RMS_NORM}, {"kernel_swiglu", GLUE_SWIGLU}};
  for (const auto &[text, kind] : known)
    if (!std::strcmp(name, text)) return kind;
  return GLUE_NONE;
}
struct GlueArgs {
  std::array<uint64_t, 32> value{};
  std::array<uint8_t, 32> size{};
  bool mem(unsigned i, cl_mem &out) const {
    if (size[i] != sizeof(cl_mem)) return false;
    out = reinterpret_cast<cl_mem>(uintptr_t(value[i]));
    return out != nullptr;
  }
  bool wide(unsigned i, uint64_t &out) const {
    if (size[i] != sizeof(cl_ulong) || value[i] > (uint64_t(1) << 40)) return false;
    out = value[i];
    return true;
  }
  bool count(unsigned i, uint64_t &out) const {  // a non-negative int
    if (size[i] != sizeof(cl_int)) return false;
    const int32_t v = int32_t(uint32_t(value[i]));
    if (v < 0) return false;
    out = uint64_t(v);
    return true;
  }
  bool real(unsigned i, float &out) const {
    if (size[i] != sizeof(float)) return false;
    const uint32_t bits = uint32_t(value[i]);
    std::memcpy(&out, &bits, sizeof(out));
    return true;
  }
};
__attribute__((target("avx512f,avx512dq"))) inline __m512 glue_expf(__m512 x) {
  // The exponential of the application's processor backend (ggml's, from Arm's optimized routines), so
  // that a gate computed here has the bits the same gate has there.
  const __m512 r = _mm512_set1_ps(0x1.8p23f);
  const __m512 z = _mm512_fmadd_ps(x, _mm512_set1_ps(0x1.715476p+0f), r);
  const __m512 n = _mm512_sub_ps(z, r);
  const __m512 b = _mm512_fnmadd_ps(n, _mm512_set1_ps(0x1.7f7d1cp-20f), _mm512_fnmadd_ps(n, _mm512_set1_ps(0x1.62e4p-1f), x));
  const __mmask16 d = _mm512_cmp_ps_mask(_mm512_abs_ps(n), _mm512_set1_ps(192), _CMP_GT_OQ);
  const __m512 u = _mm512_mul_ps(b, b);
  const __m512 j = _mm512_fmadd_ps(
      _mm512_fmadd_ps(_mm512_fmadd_ps(_mm512_set1_ps(0x1.0e4020p-7f), b, _mm512_set1_ps(0x1.573e2ep-5f)), u,
                      _mm512_fmadd_ps(_mm512_set1_ps(0x1.555e66p-3f), b, _mm512_set1_ps(0x1.fffdb6p-2f))),
      u, _mm512_fmadd_ps(_mm512_set1_ps(0x1.ffffecp-1f), b, _mm512_set1_ps(1.0F)));
  const __m512 res = _mm512_scalef_ps(j, n);
  if (_mm512_kortestz(d, d)) return res;
  const __m512 zero = _mm512_setzero_ps();
  const __m512 alt = _mm512_mask_blend_ps(_mm512_cmp_ps_mask(n, zero, _CMP_LE_OQ), _mm512_set1_ps(INFINITY), zero);
  return _mm512_mask_blend_ps(d, res, alt);
}
__attribute__((target("avx512f,avx512dq"))) void glue_swiglu_row(float *y, const float *x, const float *g, size_t n) {
  size_t i = 0;
  const __m512 one = _mm512_set1_ps(1), zero = _mm512_setzero_ps();
  for (; i + 15 < n; i += 16) {
    const __m512 v = _mm512_loadu_ps(x + i);
    const __m512 silu = _mm512_div_ps(v, _mm512_add_ps(one, glue_expf(_mm512_sub_ps(zero, v))));
    _mm512_storeu_ps(y + i, _mm512_mul_ps(silu, _mm512_loadu_ps(g + i)));
  }
  for (; i < n; ++i) y[i] = x[i] / (1.0f + expf(-x[i])) * g[i];
}
// The same for a build of that backend with 256-bit vectors, which is what the packaged one is: the two
// do not round alike, and the executor learns a feed-forward block's continuation only from a gate with
// the bits its own copy of that backend gives.
__attribute__((target("avx2,fma"))) inline __m256 glue_expf8(__m256 x) {
  const __m256 r = _mm256_set1_ps(0x1.8p23f);
  const __m256 z = _mm256_fmadd_ps(x, _mm256_set1_ps(0x1.715476p+0f), r);
  const __m256 n = _mm256_sub_ps(z, r);
  const __m256 b = _mm256_fnmadd_ps(n, _mm256_set1_ps(0x1.7f7d1cp-20f), _mm256_fnmadd_ps(n, _mm256_set1_ps(0x1.62e4p-1f), x));
  const __m256i e = _mm256_slli_epi32(_mm256_castps_si256(z), 23);
  const __m256 k = _mm256_castsi256_ps(_mm256_add_epi32(e, _mm256_castps_si256(_mm256_set1_ps(1))));
  const __m256i c = _mm256_castps_si256(_mm256_cmp_ps(_mm256_andnot_ps(_mm256_set1_ps(-0.f), n), _mm256_set1_ps(126), _CMP_GT_OQ));
  const __m256 u = _mm256_mul_ps(b, b);
  const __m256 j = _mm256_fmadd_ps(
      _mm256_fmadd_ps(_mm256_fmadd_ps(_mm256_set1_ps(0x1.0e4020p-7f), b, _mm256_set1_ps(0x1.573e2ep-5f)), u,
                      _mm256_fmadd_ps(_mm256_set1_ps(0x1.555e66p-3f), b, _mm256_set1_ps(0x1.fffdb6p-2f))),
      u, _mm256_mul_ps(_mm256_set1_ps(0x1.ffffecp-1f), b));
  if (!_mm256_movemask_ps(_mm256_castsi256_ps(c))) return _mm256_fmadd_ps(j, k, k);
  const __m256i g = _mm256_and_si256(_mm256_castps_si256(_mm256_cmp_ps(n, _mm256_setzero_ps(), _CMP_LE_OQ)),
                                     _mm256_set1_epi32(int(0x82000000u)));
  const __m256 s1 = _mm256_castsi256_ps(_mm256_add_epi32(g, _mm256_set1_epi32(0x7f000000)));
  const __m256 s2 = _mm256_castsi256_ps(_mm256_sub_epi32(e, g));
  const __m256i d = _mm256_castps_si256(_mm256_cmp_ps(_mm256_andnot_ps(_mm256_set1_ps(-0.f), n), _mm256_set1_ps(192), _CMP_GT_OQ));
  return _mm256_or_ps(
      _mm256_and_ps(_mm256_castsi256_ps(d), _mm256_mul_ps(s1, s1)),
      _mm256_andnot_ps(_mm256_castsi256_ps(d),
                       _mm256_or_ps(_mm256_and_ps(_mm256_castsi256_ps(c), _mm256_mul_ps(_mm256_fmadd_ps(s2, j, s2), s1)),
                                    _mm256_andnot_ps(_mm256_castsi256_ps(c), _mm256_fmadd_ps(k, j, k)))));
}
__attribute__((target("avx2,fma"))) void glue_swiglu_row8(float *y, const float *x, const float *g, size_t n) {
  size_t i = 0;
  const __m256 one = _mm256_set1_ps(1), zero = _mm256_setzero_ps();
  for (; i + 7 < n; i += 8) {
    const __m256 v = _mm256_loadu_ps(x + i);
    const __m256 silu = _mm256_div_ps(v, _mm256_add_ps(one, glue_expf8(_mm256_sub_ps(zero, v))));
    _mm256_storeu_ps(y + i, _mm256_mul_ps(silu, _mm256_loadu_ps(g + i)));
  }
  for (; i < n; ++i) y[i] = x[i] / (1.0f + expf(-x[i])) * g[i];
}
__attribute__((optimize("O3"), target_clones("avx512f", "avx2", "default")))
void glue_combine(float *d, const float *a, const float *b, size_t n, size_t period, bool multiply) {
  if (period == n) {
    if (multiply) for (size_t i = 0; i < n; ++i) d[i] = a[i] * b[i];
    else for (size_t i = 0; i < n; ++i) d[i] = a[i] + b[i];
    return;
  }
  for (size_t i = 0; i < n; ++i) d[i] = multiply ? a[i] * b[i % period] : a[i] + b[i % period];
}
__attribute__((optimize("O3"), target_clones("avx512f", "avx2", "default")))
void glue_rms_row(float *y, const float *x, size_t n, float eps) {
  double sum[8] = {};
  size_t i = 0;
  for (; i + 7 < n; i += 8)
    for (unsigned j = 0; j < 8; ++j) sum[j] += double(x[i + j]) * double(x[i + j]);
  double total = 0;
  for (; i < n; ++i) total += double(x[i]) * double(x[i]);
  for (unsigned j = 0; j < 8; ++j) total += sum[j];
  const float scale = 1.0f / sqrtf(float(total / double(n)) + eps);
  for (size_t k = 0; k < n; ++k) y[k] = x[k] * scale;
}
// True when the launch was computed here.
bool glue_run(cl_command_queue q, int kind, const GlueArgs &a, cl_uint dimensions, const size_t *offset,
              const size_t *global, const size_t *local) {
  if (!global || (offset && (offset[0] || (dimensions > 1 && offset[1]) || (dimensions > 2 && offset[2])))) return false;
  StartupTiming::Scope scope(startup_timing, 3);
  const auto bytes = [&](cl_mem buffer, uint64_t at, uint64_t size) -> unsigned char * {
    return size && size <= (uint64_t(1) << 32) ? host_bytes(q, buffer, size_t(at), size_t(size)) : nullptr;
  };
  const uint64_t limit = uint64_t(1) << 24;
  if (kind == GLUE_ADD || kind == GLUE_MUL) {
    cl_mem m0, m1, md;
    uint64_t o0, o1, od, ne0s[4], nb0s[4], ne1s[4], nb1s[4], ned[4], nbd[4];
    if (dimensions != 3 || !local || !a.mem(0, m0) || !a.wide(1, o0) || !a.mem(2, m1) || !a.wide(3, o1) || !a.mem(4, md) ||
        !a.wide(5, od))
      return false;
    for (unsigned i = 0; i < 4; ++i)
      if (!a.count(6 + i, ne0s[i]) || !a.wide(10 + i, nb0s[i]) || !a.count(14 + i, ne1s[i]) || !a.wide(18 + i, nb1s[i]) ||
          !a.count(22 + i, ned[i]) || !a.wide(26 + i, nbd[i]) || !ne0s[i] || !ne1s[i] || !ned[i] || ne0s[i] > limit ||
          ne1s[i] > limit || ned[i] > limit || ne0s[i] != ned[i])
        return false;
    if (nb0s[0] != 4 || nb1s[0] != 4 || nbd[0] != 4 || !local[0] || global[0] != ned[1] * local[0] || global[1] != ned[2] ||
        global[2] != ned[3])
      return false;
    const uint64_t row = ned[0] * 4, period = std::min(ne1s[0], ned[0]);
    const uint64_t ext0 = (ned[3] - 1) * nb0s[3] + (ned[2] - 1) * nb0s[2] + (ned[1] - 1) * nb0s[1] + row;
    const uint64_t ext1 = (ne1s[3] - 1) * nb1s[3] + (ne1s[2] - 1) * nb1s[2] + (ne1s[1] - 1) * nb1s[1] + period * 4;
    const uint64_t extd = (ned[3] - 1) * nbd[3] + (ned[2] - 1) * nbd[2] + (ned[1] - 1) * nbd[1] + row;
    const unsigned char *p0 = bytes(m0, o0, ext0), *p1 = bytes(m1, o1, ext1);
    unsigned char *pd = bytes(md, od, extd);
    if (!p0 || !p1 || !pd || REAL(clFinish)(q)) return false;
    q8_written(md, size_t(od), size_t(extd), false);
    for (uint64_t i3 = 0; i3 < ned[3]; ++i3)
      for (uint64_t i2 = 0; i2 < ned[2]; ++i2)
        for (uint64_t i1 = 0; i1 < ned[1]; ++i1)
          glue_combine(reinterpret_cast<float *>(pd + i3 * nbd[3] + i2 * nbd[2] + i1 * nbd[1]),
                       reinterpret_cast<const float *>(p0 + i3 * nb0s[3] + i2 * nb0s[2] + i1 * nb0s[1]),
                       reinterpret_cast<const float *>(p1 + (i3 % ne1s[3]) * nb1s[3] + (i2 % ne1s[2]) * nb1s[2] +
                                                       (i1 % ne1s[1]) * nb1s[1]),
                       ned[0], period, kind == GLUE_MUL);
    return true;
  }
  if (kind == GLUE_ADD_ROW || kind == GLUE_MUL_ROW) {
    cl_mem m0, m1, md;
    uint64_t o0, o1, od, ne;
    if (!dimensions || (dimensions > 1 && global[1] != 1) || (dimensions > 2 && global[2] != 1) || !a.mem(0, m0) ||
        !a.wide(1, o0) || !a.mem(2, m1) || !a.wide(3, o1) || !a.mem(4, md) ||
        !a.wide(5, od) || !a.count(6, ne) || !ne || ne > limit || !global[0] || global[0] > limit * 64)
      return false;
    const uint64_t floats = global[0] * 4, period = ne * 4;
    const unsigned char *p0 = bytes(m0, o0, floats * 4), *p1 = bytes(m1, o1, period * 4);
    unsigned char *pd = bytes(md, od, floats * 4);
    if (!p0 || !p1 || !pd || REAL(clFinish)(q)) return false;
    q8_written(md, size_t(od), size_t(floats * 4), false);
    for (uint64_t at = 0; at < floats; at += period)
      glue_combine(reinterpret_cast<float *>(pd) + at, reinterpret_cast<const float *>(p0) + at,
                   reinterpret_cast<const float *>(p1), std::min(period, floats - at), std::min(period, floats - at),
                   kind == GLUE_MUL_ROW);
    return true;
  }
  if (kind == GLUE_RMS_NORM) {
    cl_mem m0, md;
    uint64_t o0, od, ne[4], nb[4] = {4, 0, 0, 0};
    float eps;
    if (dimensions != 3 || !local || !a.mem(0, m0) || !a.wide(1, o0) || !a.mem(2, md) || !a.wide(3, od) || !a.real(11, eps))
      return false;
    for (unsigned i = 0; i < 4; ++i)
      if (!a.count(4 + i, ne[i]) || !ne[i] || ne[i] > limit || (i && !a.wide(7 + i, nb[i]))) return false;
    if (ne[0] % 4 || !local[0] || global[0] != ne[1] * local[0] || global[1] != ne[2] || global[2] != ne[3]) return false;
    const uint64_t row = ne[0] * 4;
    const uint64_t ext0 = (ne[3] - 1) * nb[3] + (ne[2] - 1) * nb[2] + (ne[1] - 1) * nb[1] + row;
    const uint64_t extd = ne[3] * ne[2] * ne[1] * row;
    const unsigned char *p0 = bytes(m0, o0, ext0);
    unsigned char *pd = bytes(md, od, extd);
    if (!p0 || !pd || REAL(clFinish)(q)) return false;
    q8_written(md, size_t(od), size_t(extd), false);
    for (uint64_t i3 = 0; i3 < ne[3]; ++i3)
      for (uint64_t i2 = 0; i2 < ne[2]; ++i2)
        for (uint64_t i1 = 0; i1 < ne[1]; ++i1)
          glue_rms_row(reinterpret_cast<float *>(pd) + (i3 * ne[2] * ne[1] + i2 * ne[1] + i1) * ne[0],
                       reinterpret_cast<const float *>(p0 + i3 * nb[3] + i2 * nb[2] + i1 * nb[1]), ne[0], eps);
    return true;
  }
  if (kind == GLUE_SWIGLU) {
    // Which of the backend's two exponentials: the 256-bit one unless SHIELD_OPENCL_GLUE_EXP=avx512.
    static const bool wide_exp = [] {
      const char *value = std::getenv("SHIELD_OPENCL_GLUE_EXP");
      return value && !std::strcmp(value, "avx512");
    }();
    static const bool wide_vectors = wide_exp ? __builtin_cpu_supports("avx512f") && __builtin_cpu_supports("avx512dq")
                                              : __builtin_cpu_supports("avx2") && __builtin_cpu_supports("fma");
    cl_mem m0, m1, md;
    uint64_t o0, o1, od, nb01, nb11, ne0, nb1, off0, off1;
    if (!wide_vectors || dimensions != 3 || !local || !local[0] || global[0] % local[0] || global[1] != 1 || global[2] != 1 ||
        !a.mem(0, m0) || !a.wide(1, o0) || !a.mem(2, m1) || !a.wide(3, o1) || !a.mem(4, md) || !a.wide(5, od) ||
        !a.wide(6, nb01) || !a.wide(7, nb11) || !a.count(8, ne0) || !a.wide(9, nb1) || !a.count(10, off0) || !a.count(11, off1) ||
        !ne0 || ne0 > limit || off0 > limit || off1 > limit)
      return false;
    const uint64_t rows = global[0] / local[0];
    if (!rows || rows > limit) return false;
    const uint64_t ext0 = (rows - 1) * nb01 + (off0 + ne0) * 4, ext1 = (rows - 1) * nb11 + (off1 + ne0) * 4;
    const uint64_t extd = (rows - 1) * nb1 + ne0 * 4;
    const unsigned char *p0 = bytes(m0, o0, ext0), *p1 = bytes(m1, o1, ext1);
    unsigned char *pd = bytes(md, od, extd);
    if (!p0 || !p1 || !pd || REAL(clFinish)(q)) return false;
    q8_written(md, size_t(od), size_t(extd), false);
    for (uint64_t r = 0; r < rows; ++r)
      (wide_exp ? glue_swiglu_row : glue_swiglu_row8)(reinterpret_cast<float *>(pd + r * nb1),
                                                      reinterpret_cast<const float *>(p0 + r * nb01) + off0,
                                                      reinterpret_cast<const float *>(p1 + r * nb11) + off1, ne0);
    return true;
  }
  return false;
}

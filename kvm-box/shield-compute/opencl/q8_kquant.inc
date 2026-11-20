// K-quant weights as signed codes with float scales, for the local product in
// q8_direct.inc. Pure functions of the flat buffers the OpenCL backend keeps
// (the GGUF block split apart), written as ggml's dequantize_row_q4_K, _q5_K
// and _q6_K read a block. test_q8_kquant.py compares them with gguf-py's.
inline float q8_half_to_float(uint16_t h) {
  const uint32_t sign = uint32_t(h & 0x8000) << 16, exponent = (h >> 10) & 31, fraction = h & 1023;
  uint32_t bits;
  if (exponent == 0) {
    if (!fraction) bits = sign;
    else {
      int shift = 0;
      uint32_t f = fraction;
      while (!(f & 1024)) { f <<= 1; ++shift; }
      bits = sign | ((113 - shift) << 23) | ((f & 1023) << 13);
    }
  } else if (exponent == 31) bits = sign | 0x7f800000u | (fraction << 13);
  else bits = sign | ((exponent + 112) << 23) | (fraction << 13);
  float value;
  std::memcpy(&value, &bits, 4);
  return value;
}

// Q4_K (high == nullptr) and Q5_K: eight parts of 32 weights to a block, each
// with a 6-bit scale and minimum; the low nibbles of 32 quant bytes are one
// part and their high nibbles the next; Q5_K adds a fifth bit for each weight
// from one bit of the matching high byte. A weight is
// scales[part] * codes[i] - minimums[part].
inline void q8_decode_q45_k(const unsigned char *quants, const unsigned char *high, const unsigned char *packed,
                            const unsigned char *d, const unsigned char *dmin, size_t blocks, int8_t *codes,
                            float *scales, float *minimums) {
  for (size_t ib = 0; ib < blocks; ++ib) {
    const unsigned char *q = quants + ib * 128, *sc = packed + ib * 12;
    const unsigned char *qh = high ? high + ib * 32 : nullptr;
    uint16_t half_d, half_m;
    std::memcpy(&half_d, d + ib * 2, 2);
    std::memcpy(&half_m, dmin + ib * 2, 2);
    const float scale_d = q8_half_to_float(half_d), scale_m = q8_half_to_float(half_m);
    for (size_t part = 0; part < 8; ++part) {
      const unsigned scale = part < 4 ? sc[part] & 63 : (sc[part + 4] & 0xF) | ((sc[part - 4] >> 6) << 4);
      const unsigned minimum = part < 4 ? sc[part + 4] & 63 : (sc[part + 4] >> 4) | ((sc[part] >> 6) << 4);
      scales[ib * 8 + part] = scale_d * float(scale);
      minimums[ib * 8 + part] = scale_m * float(minimum);
      const unsigned char *bytes = q + (part / 2) * 32;
      int8_t *out = codes + ib * 256 + part * 32;
      for (size_t l = 0; l < 32; ++l) {
        unsigned value = part % 2 ? bytes[l] >> 4 : bytes[l] & 0xF;
        if (qh && ((qh[l] >> part) & 1)) value += 16;
        out[l] = int8_t(value);
      }
    }
  }
}

// Q6_K: sixteen parts of 16 weights to a block, each with a signed 8-bit
// scale. Within each half block, the low nibbles of the first 32 low bytes
// are weights 0..31 and of the next 32 weights 32..63, the high nibbles the
// following 64; the two high bits come from the matching 2-bit field of the
// high byte. A weight is scales[part] * codes[i], codes being the 6-bit
// value less 32.
inline void q8_decode_q6_k(const unsigned char *low, const unsigned char *high, const unsigned char *packed,
                           const unsigned char *d, size_t blocks, int8_t *codes, float *scales) {
  for (size_t ib = 0; ib < blocks; ++ib) {
    const unsigned char *ql = low + ib * 128, *qh = high + ib * 64;
    const auto *sc = reinterpret_cast<const int8_t *>(packed + ib * 16);
    uint16_t half;
    std::memcpy(&half, d + ib * 2, 2);
    const float scale_d = q8_half_to_float(half);
    int8_t *out = codes + ib * 256;
    for (size_t part = 0; part < 2; ++part)
      for (size_t l = 0; l < 32; ++l) {
        const unsigned low0 = ql[part * 64 + l], low1 = ql[part * 64 + l + 32], bits = qh[part * 32 + l];
        out[part * 128 + l] = int8_t(int((low0 & 0xF) | ((bits & 3) << 4)) - 32);
        out[part * 128 + l + 32] = int8_t(int((low1 & 0xF) | (((bits >> 2) & 3) << 4)) - 32);
        out[part * 128 + l + 64] = int8_t(int((low0 >> 4) | (((bits >> 4) & 3) << 4)) - 32);
        out[part * 128 + l + 96] = int8_t(int((low1 >> 4) | (((bits >> 6) & 3) << 4)) - 32);
      }
    for (size_t i = 0; i < 16; ++i) scales[ib * 16 + i] = scale_d * float(sc[i]);
  }
}

// Launch admission for the recognized kernels: the same checks as
// validate_launch in q8_admission.py, which remains the reference (see
// test_q8_admission_native.py). Running them here keeps a launch from waiting
// on a helper process. Public launch geometry only; never tensor contents.
//
// The K-quant kernels have four weight buffers (Q5_K five) where the Q8_0
// kernels have two, so their other arguments come two (three) places later.
// Scalars are held that many places earlier than the kernel has them, which
// for all but the Q6_K vector kernel is the Q8_0 numbering; that kernel has
// its own, shorter list.
enum Q8Kind {
  Q8_NONE = 0, Q8_MM = 1, Q8_MV = 2, Q4K_MM = 3, Q4K_MV = 4, Q6K_MM = 5, Q6K_MV = 6, Q5K_MM = 7, Q5K_MV = 8
};
inline bool q8_kind_vector(int kind) {
  return kind == Q8_MV || kind == Q4K_MV || kind == Q6K_MV || kind == Q5K_MV;
}
// Q5_K has a fifth weight buffer, the high bit of each quant.
inline unsigned q8_kind_weights(int kind) {
  return kind == Q5K_MM || kind == Q5K_MV ? 5 : kind >= Q4K_MM && kind <= Q6K_MV ? 4 : 2;
}
// One past the last meaningful scalar.
inline unsigned q8_kind_scalars(int kind) { return kind == Q6K_MV ? 15 : 19; }
struct Q8BufferView {
  uint64_t root, root_bytes, offset, size;
};
struct Q8LaunchView {
  int kind;
  std::array<uint64_t, 19> scalars;      // Q8_0 numbering: 3 and 5..18 are meaningful
  std::array<Q8BufferView, 7> buffers;   // the weight buffers, then input and output
  std::array<uint64_t, 3> global, local, offset;
};
struct Q8Layout {
  size_t m, n, k;
  std::array<size_t, 6> read_bytes;      // the weight buffers, then the input
  size_t output_bytes;
};
std::optional<Q8Layout> q8_validate_launch(const Q8LaunchView &v) {
  using wide = unsigned __int128;
  constexpr uint64_t I32 = (1ull << 31) - 1, U32 = (1ull << 32) - 1, MAX_BYTES = 2ull << 30;
  if (v.kind < Q8_MM || v.kind > Q5K_MV) return std::nullopt;
  const bool vector = q8_kind_vector(v.kind), k_quant = q8_kind_weights(v.kind) >= 4;
  const bool q6 = v.kind == Q6K_MM || v.kind == Q6K_MV, q5 = v.kind == Q5K_MM || v.kind == Q5K_MV;
  const unsigned weights = q8_kind_weights(v.kind);
  struct Interval { uint64_t root; wide start, end; };
  auto interval = [&](const Q8BufferView &b, wide offset, wide length) -> std::optional<Interval> {
    if (wide(b.offset) + b.size > b.root_bytes || offset + length > b.size || length > MAX_BYTES)
      return std::nullopt;
    return Interval{b.root, wide(b.offset) + offset, wide(b.offset) + offset + length};
  };
  if (v.offset[0] || v.offset[1] || v.offset[2]) return std::nullopt;
  const auto &s = v.scalars;
  if (v.kind == Q6K_MV) {
    // offset1, offsetd, ne00, ne01, ne02, ne10, ne12, ne0, ne1, r2, r3 at
    // 3, 5 and 6..14. Every row is guarded before it is read. Block counts,
    // the input index and the output index are signed int products.
    for (unsigned i = 6; i < 15; ++i)
      if (s[i] > I32) return std::nullopt;
    const wide k = s[6], n = s[7], m = s[12];
    if (!k || !n || !m || k % 256 || k > I32 - 128) return std::nullopt;
    if (n > I32 - 8 || n * k / 256 > I32 || m * k > I32 || m * n > I32) return std::nullopt;
    if (s[8] != 1 || wide(s[9]) != k || s[10] != 1 || wide(s[11]) != n || s[13] != 1 || s[14] != 1)
      return std::nullopt;
    if (wide(v.global[0]) != (n + 7) / 8 * 16 || wide(v.global[1]) != m * 2 || v.global[2] != 1 ||
        v.local[0] != 16 || v.local[1] != 2 || v.local[2] != 1)
      return std::nullopt;
    if (s[3] % 4 || s[5] % 4) return std::nullopt;
    for (unsigned i = 0; i < 6; ++i) {
      if (!interval(v.buffers[i], 0, 0)) return std::nullopt;
      for (unsigned j = 0; j < 6; ++j)
        if (v.buffers[i].root == v.buffers[j].root && v.buffers[i].root_bytes != v.buffers[j].root_bytes)
          return std::nullopt;
    }
    const std::array<std::pair<uint64_t, uint64_t>, 6> aligned = {{{1, 0}, {1, 0}, {1, 0}, {2, 0}, {4, s[3]}, {4, s[5]}}};
    for (unsigned i = 0; i < 6; ++i)
      if ((wide(v.buffers[i].offset) + aligned[i].second) % aligned[i].first) return std::nullopt;
    const wide blocks = n * k / 256;
    const std::array<std::optional<Interval>, 5> reads = {
        interval(v.buffers[0], 0, blocks * 128), interval(v.buffers[1], 0, blocks * 64),
        interval(v.buffers[2], 0, blocks * 16), interval(v.buffers[3], 0, blocks * 2),
        interval(v.buffers[4], s[3], m * k * 4)};
    const auto write = interval(v.buffers[5], s[5], m * n * 4);
    if (!write) return std::nullopt;
    Q8Layout layout{size_t(m), size_t(n), size_t(k), {}, size_t(write->end - write->start)};
    for (unsigned i = 0; i < 5; ++i) {
      if (!reads[i]) return std::nullopt;
      if (write->root == reads[i]->root &&
          std::max(write->start, reads[i]->start) < std::min(write->end, reads[i]->end))
        return std::nullopt;
      layout.read_bytes[i] = size_t(reads[i]->end - reads[i]->start);
    }
    return layout;
  }
  for (unsigned i = 3; i < 19; ++i) {
    if (i == 4) continue;
    // The Q4_K and Q5_K vector kernels take their two pointer offsets as int.
    const bool is_wide = ((i == 3 || i == 5) && v.kind != Q4K_MV && v.kind != Q5K_MV) ||
                         (vector && (i == 8 || i == 9 || i == 10 || i == 12 || i == 13 || i == 14));
    if (!is_wide && s[i] > I32) return std::nullopt;
  }
  const wide k = s[6], n = s[7], m = s[vector ? 16 : 9];
  if (!k || !n || !m || k % (k_quant ? 256 : 32) || k > I32 - 128) return std::nullopt;
  std::array<std::pair<unsigned, wide>, 10> expected;
  std::array<wide, 3> grid, local;
  uint64_t input_alignment;
  if (!vector) {
    expected = {{{8, 1}, {10, 1}, {11, k}, {12, k}, {13, n}, {14, k * n}, {15, k * m}, {16, n * m}, {17, 1}, {18, 1}}};
    grid = {(n + 63) / 64 * 128, (m + 63) / 64, 1};
    local = {128, 1, 1};
    // Index arithmetic is signed int in the original kernel, including
    // rounded tile positions and the final pos_a/pos_b increment.
    if (std::max({((n + 63) / 64 * 64 + 32) * k, ((m + 63) / 64 * 64 + 32) * k, m * n}) > I32)
      return std::nullopt;
    input_alignment = 16;
  } else {
    const wide row = q5 ? k / 256 * 176 : k_quant ? k / 256 * 144 : k / 32 * 34;
    expected = {{{8, row}, {9, row * n}, {10, row * n}, {11, 1}, {12, k * 4}, {13, m * k * 4}, {14, m * k * 4},
                 {15, n}, {17, 1}, {18, 1}}};
    if (k_quant) {
      grid = {(n + 3) / 4 * 16, m, 1};
      local = {16, 1, 1};
      // Four rows are loaded before the output guards. The block index of a
      // row, its byte offset into the quants (128 bytes a block), the input
      // offset and the output index are all signed int.
      if (n % 4 || n > I32 - 4 || n * k / 2 > I32 || m * k * 4 > I32 || m * n > I32) return std::nullopt;
    } else {
      grid = {(n + 7) / 8 * 16, m * 2, 1};
      local = {16, 2, 1};
      // All four rows are loaded before the output guards. The source also
      // narrows packed row offsets to uint before dividing by 34.
      if (n % 8 || n > I32 - 8 || row * n > U32) return std::nullopt;
    }
    input_alignment = 4;
  }
  for (const auto &[index, value] : expected)
    if (wide(s[index]) != value) return std::nullopt;
  for (unsigned i = 0; i < 3; ++i)
    if (wide(v.global[i]) != grid[i] || wide(v.local[i]) != local[i]) return std::nullopt;
  if (s[3] % input_alignment || s[5] % 4) return std::nullopt;
  for (unsigned i = 0; i < weights + 2; ++i) {
    const auto &a = v.buffers[i];
    if (!interval(a, 0, 0)) return std::nullopt;
    for (unsigned j = 0; j < weights + 2; ++j)
      if (a.root == v.buffers[j].root && a.root_bytes != v.buffers[j].root_bytes) return std::nullopt;
  }
  // The Q4_K quants are read through uchar4 (matrix) or uint (vector)
  // pointers; the vector kernel reads the scales as ushort.
  std::array<std::pair<uint64_t, uint64_t>, 7> alignment{};
  // Q5_K is Q4_K with the high bits, read a byte at a time, after the quants.
  // The Q6_K matrix kernel reads quants and scales a byte at a time.
  if (q5) alignment = {{{4, 0}, {1, 0}, {vector ? 2u : 1u, 0}, {2, 0}, {2, 0}}};
  else if (q6) alignment = {{{1, 0}, {1, 0}, {1, 0}, {2, 0}}};
  else if (k_quant) alignment = {{{4, 0}, {vector ? 2u : 1u, 0}, {2, 0}, {2, 0}}};
  else alignment = {{{vector ? 1u : 4u, 0}, {2, 0}}};
  alignment[weights] = {input_alignment, s[3]};
  alignment[weights + 1] = {4, s[5]};
  for (unsigned i = 0; i < weights + 2; ++i)
    if ((wide(v.buffers[i].offset) + alignment[i].second) % alignment[i].first) return std::nullopt;
  std::array<std::optional<Interval>, 6> reads{};
  if (k_quant) {
    const wide blocks = n * k / 256;
    const std::array<unsigned, 5> sizes = q5 ? std::array<unsigned, 5>{128, 32, 12, 2, 2}
                                           : q6 ? std::array<unsigned, 5>{128, 64, 16, 2, 0}
                                                : std::array<unsigned, 5>{128, 12, 2, 2, 0};
    for (unsigned i = 0; i < weights; ++i) reads[i] = interval(v.buffers[i], 0, blocks * sizes[i]);
  } else {
    reads[0] = interval(v.buffers[0], 0, n * k);
    reads[1] = interval(v.buffers[1], 0, n * k / 16);
  }
  reads[weights] = interval(v.buffers[weights], s[3], m * k * 4);
  const auto write = interval(v.buffers[weights + 1], s[5], m * n * 4);
  if (!write) return std::nullopt;
  Q8Layout layout{size_t(m), size_t(n), size_t(k), {}, size_t(write->end - write->start)};
  for (unsigned i = 0; i <= weights; ++i) {
    const auto &read = reads[i];
    if (!read) return std::nullopt;
    if (write->root == read->root && std::max(write->start, read->start) < std::min(write->end, read->end))
      return std::nullopt;
    layout.read_bytes[i] = size_t(read->end - read->start);
  }
  return layout;
}

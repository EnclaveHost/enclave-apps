// Launch admission for the two recognized Q8 kernels: the same checks as
// validate_launch in q8_admission.py, which remains the reference (see
// test_q8_admission_native.py). Running them here keeps a launch from waiting
// on a helper process. Public launch geometry only; never tensor contents.
struct Q8BufferView {
  uint64_t root, root_bytes, offset, size;
};
struct Q8LaunchView {
  bool mv;
  std::array<uint64_t, 19> scalars;      // indices 3 and 5..18 are meaningful
  std::array<Q8BufferView, 4> buffers;   // kernel arguments 0, 1, 2 and 4
  std::array<uint64_t, 3> global, local, offset;
};
struct Q8Layout {
  size_t m, n, k;
  std::array<size_t, 3> read_bytes;
  size_t output_bytes;
};
std::optional<Q8Layout> q8_validate_launch(const Q8LaunchView &v) {
  using wide = unsigned __int128;
  constexpr uint64_t I32 = (1ull << 31) - 1, U32 = (1ull << 32) - 1, MAX_BYTES = 512ull << 20;
  struct Interval { uint64_t root; wide start, end; };
  auto interval = [&](const Q8BufferView &b, wide offset, wide length) -> std::optional<Interval> {
    if (wide(b.offset) + b.size > b.root_bytes || offset + length > b.size || length > MAX_BYTES)
      return std::nullopt;
    return Interval{b.root, wide(b.offset) + offset, wide(b.offset) + offset + length};
  };
  if (v.offset[0] || v.offset[1] || v.offset[2]) return std::nullopt;
  const auto &s = v.scalars;
  for (unsigned i = 3; i < 19; ++i) {
    if (i == 4) continue;
    const bool is_wide = i == 3 || i == 5 || (v.mv && (i == 8 || i == 9 || i == 10 || i == 12 || i == 13 || i == 14));
    if (!is_wide && s[i] > I32) return std::nullopt;
  }
  const wide k = s[6], n = s[7], m = s[v.mv ? 16 : 9];
  if (!k || !n || !m || k % 32 || k > I32 - 128) return std::nullopt;
  std::array<std::pair<unsigned, wide>, 10> expected;
  std::array<wide, 3> grid, local;
  uint64_t input_alignment;
  if (!v.mv) {
    expected = {{{8, 1}, {10, 1}, {11, k}, {12, k}, {13, n}, {14, k * n}, {15, k * m}, {16, n * m}, {17, 1}, {18, 1}}};
    grid = {(n + 63) / 64 * 128, (m + 63) / 64, 1};
    local = {128, 1, 1};
    // Index arithmetic is signed int in the original kernel, including
    // rounded tile positions and the final pos_a/pos_b increment.
    if (std::max({((n + 63) / 64 * 64 + 32) * k, ((m + 63) / 64 * 64 + 32) * k, m * n}) > I32)
      return std::nullopt;
    input_alignment = 16;
  } else {
    const wide row = k / 32 * 34;
    expected = {{{8, row}, {9, row * n}, {10, row * n}, {11, 1}, {12, k * 4}, {13, m * k * 4}, {14, m * k * 4},
                 {15, n}, {17, 1}, {18, 1}}};
    grid = {(n + 7) / 8 * 16, m * 2, 1};
    local = {16, 2, 1};
    // All four rows are loaded before the output guards. The source also
    // narrows packed row offsets to uint before dividing by 34.
    if (n % 8 || n > I32 - 8 || row * n > U32) return std::nullopt;
    input_alignment = 4;
  }
  for (const auto &[index, value] : expected)
    if (wide(s[index]) != value) return std::nullopt;
  for (unsigned i = 0; i < 3; ++i)
    if (wide(v.global[i]) != grid[i] || wide(v.local[i]) != local[i]) return std::nullopt;
  if (s[3] % input_alignment || s[5] % 4) return std::nullopt;
  for (const auto &a : v.buffers) {
    if (!interval(a, 0, 0)) return std::nullopt;
    for (const auto &b : v.buffers)
      if (a.root == b.root && a.root_bytes != b.root_bytes) return std::nullopt;
  }
  const std::array<std::pair<uint64_t, uint64_t>, 4> alignment = {{
      {v.mv ? 1 : 4, 0}, {2, 0}, {input_alignment, s[3]}, {4, s[5]}}};
  for (unsigned i = 0; i < 4; ++i)
    if ((wide(v.buffers[i].offset) + alignment[i].second) % alignment[i].first) return std::nullopt;
  const std::array<std::optional<Interval>, 3> reads = {interval(v.buffers[0], 0, n * k),
                                                        interval(v.buffers[1], 0, n * k / 16),
                                                        interval(v.buffers[2], s[3], m * k * 4)};
  const auto write = interval(v.buffers[3], s[5], m * n * 4);
  if (!write) return std::nullopt;
  for (const auto &read : reads) {
    if (!read) return std::nullopt;
    if (write->root == read->root && std::max(write->start, read->start) < std::min(write->end, read->end))
      return std::nullopt;
  }
  return Q8Layout{size_t(m), size_t(n), size_t(k),
                  {size_t(reads[0]->end - reads[0]->start), size_t(reads[1]->end - reads[1]->start),
                   size_t(reads[2]->end - reads[2]->start)},
                  size_t(write->end - write->start)};
}

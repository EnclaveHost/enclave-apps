// Test driver for q8_kquant.inc: reads "type blocks" and the flat buffers from
// stdin, writes the decoded weights as float32. See test_q8_kquant.py.
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

#include "q8_kquant.inc"

int main() {
  unsigned type = 0;
  unsigned long long blocks = 0;
  if (std::scanf("%u %llu\n", &type, &blocks) != 2 || blocks == 0 || blocks > 100000) return 2;
  auto read = [&](size_t bytes) {
    std::vector<unsigned char> data(bytes * blocks);
    if (std::fread(data.data(), 1, data.size(), stdin) != data.size()) std::exit(2);
    return data;
  };
  std::vector<int8_t> codes(blocks * 256);
  std::vector<float> weights(blocks * 256);
  if (type == 14) {
    const auto low = read(128), high = read(64), packed = read(16), d = read(2);
    std::vector<float> scales(blocks * 16);
    q8_decode_q6_k(low.data(), high.data(), packed.data(), d.data(), blocks, codes.data(), scales.data());
    for (size_t i = 0; i < weights.size(); ++i) weights[i] = scales[i / 16] * float(codes[i]);
  } else if (type == 12 || type == 13) {
    const auto quants = read(128);
    const auto high = type == 13 ? read(32) : std::vector<unsigned char>();
    const auto packed = read(12), d = read(2), dmin = read(2);
    std::vector<float> scales(blocks * 8), minimums(blocks * 8);
    q8_decode_q45_k(quants.data(), type == 13 ? high.data() : nullptr, packed.data(), d.data(), dmin.data(), blocks,
                    codes.data(), scales.data(), minimums.data());
    for (size_t i = 0; i < weights.size(); ++i) weights[i] = scales[i / 32] * float(codes[i]) - minimums[i / 32];
  } else {
    return 2;
  }
  return std::fwrite(weights.data(), 4, weights.size(), stdout) == weights.size() ? 0 : 2;
}

// Test driver for q8_admission.inc. One launch per input line:
//   kind (1 mm, 2 mv, 3 mm_q4_k, 4 mv_q4_k, 5 mm_q6_k, 6 mv_q6_k), scalars 3 and
//   5..18 two places before a K-quant kernel has them (zero where unused), six buffers (root root_bytes offset size; unused ones zero),
//   global[3], local[3], offset[3]  -- 54 unsigned decimal numbers.
// Prints "0", or "1 m n k reads... output".
#include <algorithm>
#include <array>
#include <cstdint>
#include <cstdio>
#include <iostream>
#include <optional>
#include <sstream>
#include <string>
#include <utility>

#include "q8_admission.inc"

int main() {
  std::string line;
  while (std::getline(std::cin, line)) {
    std::istringstream in(line);
    Q8LaunchView view{};
    uint64_t kind = 0;
    in >> kind;
    view.kind = int(kind);
    in >> view.scalars[3] >> view.scalars[4];
    for (unsigned i = 5; i < 19; ++i) in >> view.scalars[i];
    for (auto &b : view.buffers) in >> b.root >> b.root_bytes >> b.offset >> b.size;
    for (auto &v : view.global) in >> v;
    for (auto &v : view.local) in >> v;
    for (auto &v : view.offset) in >> v;
    if (!in) {
      std::puts("bad");
      return 2;
    }
    const auto layout = q8_validate_launch(view);
    if (!layout) {
      std::puts("0");
      continue;
    }
    std::printf("1 %zu %zu %zu", layout->m, layout->n, layout->k);
    for (unsigned i = 0; i <= q8_kind_weights(view.kind); ++i) std::printf(" %zu", layout->read_bytes[i]);
    std::printf(" %zu\n", layout->output_bytes);
  }
  return 0;
}

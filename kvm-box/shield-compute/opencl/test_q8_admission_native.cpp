// Test driver for q8_admission.inc. One launch per input line:
//   mv, scalars 3 and 5..18, four buffers (root root_bytes offset size),
//   global[3], local[3], offset[3]  -- 41 unsigned decimal numbers.
// Prints "0", or "1 m n k read0 read1 read2 output".
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
    uint64_t mv = 0;
    in >> mv;
    view.mv = mv != 0;
    in >> view.scalars[3];
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
    std::printf("1 %zu %zu %zu %zu %zu %zu %zu\n", layout->m, layout->n, layout->k, layout->read_bytes[0],
                layout->read_bytes[1], layout->read_bytes[2], layout->output_bytes);
  }
  return 0;
}

// Synthetic public Q8 fixtures against the packaged native GGML backend.
#include "ggml-backend.h"
#include "ggml-shielded.h"
#include "ggml.h"
#include "shielded-tee.h"
#include <algorithm>
#include <cassert>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

static constexpr int K = 64, N = 16;
struct Weight {
  std::string name;
  std::vector<unsigned char> bytes;
};
static std::vector<Weight> approved;
static int calls = 0;
static int verify(void *, const char *name, uint32_t type, const int64_t ne[4],
                  const void *bytes, size_t size) {
  for (const auto &w : approved)
    if (w.name == name && type == GGML_TYPE_Q8_0 && ne[0] == K && ne[1] == N &&
        ne[2] == 1 && ne[3] == 1 && size == w.bytes.size() &&
        !memcmp(bytes, w.bytes.data(), size)) {
      calls++;
      return 0;
    }
  return -1;
}
struct Graph {
  ggml_tensor *input;
  ggml_tensor *outputs[2];
  ggml_cgraph *graph;
  int rows;
};
static Graph graph(ggml_context *ctx, ggml_tensor **weights, int rows) {
  Graph result{};
  result.rows = rows;
  result.input = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, K, rows);
  ggml_set_input(result.input);
  // Identical full chunks must still use different masked payloads.
  for (int r = 0; r < rows; r++)
    for (int k = 0; k < K; k++)
      ((float *)result.input->data)[r * K + k] =
          float(((r % 32) * 3 + k) % 17 - 8) / 256.0f;
  result.graph = ggml_new_graph_custom(ctx, 16, false);
  for (int t = 0; t < 2; t++) {
    auto *out = ggml_mul_mat(ctx, weights[t], result.input);
    result.outputs[t] = out;
    ggml_set_output(out);
    std::fill_n((float *)out->data, rows * N, -999.0f);
    ggml_build_forward_expand(result.graph, out);
  }
  return result;
}
static void check(const Graph &g, bool untouched = false) {
  for (int t = 0; t < 2; t++)
    for (int r = 0; r < g.rows; r++)
      for (int j = 0; j < N; j++) {
        float expected = untouched ? -999.0f : 0.0f;
        if (!untouched)
          for (int k = 0; k < K; k++)
            expected += ((float *)g.input->data)[r * K + k] *
                        float((j + k + t) % 3 - 1);
        assert(((float *)g.outputs[t]->data)[r * N + j] == expected);
      }
}
// Diagnostic API coverage only. Production bootstrap always enables
// verification.
static int unverified_mode(int rows) {
  uint64_t exchanges = 999, macs = 999;
  sh_link_verified_stats(nullptr, &exchanges, &macs);
  assert(exchanges == 0 && macs == 0);
  int error = 0;
  auto *link =
      sh_link_open(std::getenv("SHIELDED_HOST"),
                   std::atoi(std::getenv("SHIELDED_PORT")), false, &error);
  assert(link && error == SH_OK);
  sh_link_configure(link, 0, 128u << 20, 1);
  std::vector<int8_t> weights(K * N);
  std::vector<int64_t> input(rows * K), output(rows * N, -999);
  for (int j = 0; j < N; j++)
    for (int k = 0; k < K; k++)
      weights[j * K + k] = (j + k) % 3 - 1;
  for (int r = 0; r < rows; r++)
    for (int k = 0; k < K; k++)
      input[r * K + k] = ((r % 32) * 3 + k) % 17 - 8;
  int node = sh_link_add_weight(link, "public-counter-fixture", weights.data(),
                                K, N, 32, -1);
  assert(node >= 0 && sh_link_start(link) == SH_OK);
  auto *result = output.data();
  assert(sh_link_gemm(link, &node, 1, input.data(), rows, &result) == SH_OK);
  for (int r = 0; r < rows; r++)
    for (int j = 0; j < N; j++) {
      int64_t expected = 0;
      for (int k = 0; k < K; k++)
        expected += input[r * K + k] * weights[j * K + k];
      assert(output[r * N + j] == expected);
    }
  sh_link_verified_stats(link, &exchanges, &macs);
  assert(exchanges == 0 && macs == 0);
  uint64_t legacy_exchanges = 0, failures = 999;
  sh_link_stats(link, &legacy_exchanges, nullptr, &failures);
  assert(legacy_exchanges == (uint64_t)(rows + 31) / 32 && failures == 0);
  sh_link_close(link);
  std::puts("{\"mode\":\"unverified-diagnostic\",\"verifiedExchanges\":0,"
            "\"verifiedMacs\":0,\"passed\":true}");
  return 0;
}

int main(int argc, char **argv) {
  assert(argc == 4);
  const int M = std::stoi(argv[1]), fault_at = std::stoi(argv[3]);
  const std::string mode = argv[2];
  const bool transport = mode == "transport-failure";
  const bool failure = mode == "failure" || transport, mixed = mode == "mixed";
  assert(M > 32 && M <= 256);
  if (mode == "unverified")
    return unverified_mode(M);
  assert(failure ? fault_at >= (transport ? 0 : 1) && fault_at <= (M + 31) / 32 : fault_at == 0);
  assert(ggml_backend_shielded_require_offload() != 0);
  assert(ggml_backend_shielded_set_weight_verifier(verify, nullptr) == 0);
  assert(ggml_backend_shielded_require_offload() == 0);
  uint64_t exchanges = 999, wire_macs = 999;
  ggml_backend_shielded_verified_stats(&exchanges, &wire_macs);
  assert(exchanges == 0 && wire_macs == 0);
  auto *ctx = ggml_init({8u << 20, nullptr, false});
  assert(ctx);
  ggml_tensor *weights[2];
  for (int t = 0; t < 2; t++) {
    auto *w = ggml_new_tensor_2d(ctx, GGML_TYPE_Q8_0, K, N);
    weights[t] = w;
    ggml_set_name(w, t ? "blk.0.ffn_up.weight" : "blk.0.ffn_gate.weight");
    auto *raw = (unsigned char *)w->data;
    for (int j = 0; j < N; j++)
      for (int b = 0; b < K / 32; b++) {
        auto *block = raw + (j * (K / 32) + b) * 34;
        const ggml_fp16_t d = ggml_fp32_to_fp16(1.0f);
        memcpy(block, &d, 2);
        for (int k = 0; k < 32; k++)
          ((int8_t *)(block + 2))[k] = int8_t((j + b * 32 + k + t) % 3 - 1);
      }
    approved.push_back({ggml_get_name(w),
                        std::vector<unsigned char>(raw, raw + ggml_nbytes(w))});
  }
  auto g = graph(ctx, weights, M);
  auto *backend = ggml_backend_shielded_init();
  assert(backend);
  const auto rc = ggml_backend_graph_compute(backend, g.graph);
  uint64_t off = 0, local = 0, macs = 0, fail = 0;
  ggml_backend_shielded_stats(&off, &local, &macs, &fail);
  ggml_backend_shielded_verified_stats(&exchanges, &wire_macs);
  assert(calls == 2);
  assert(ggml_backend_shielded_require_offload() != 0);
  if (failure) {
    assert(rc != GGML_STATUS_SUCCESS && (transport ? fail == 0 : fail > 0) && off == 0);
    assert(exchanges == (uint64_t)std::max(fault_at - 1, 0));
    assert(wire_macs == (uint64_t)std::max(fault_at - 1, 0) * 32 * K * N * 2);
    check(g, true);
    assert(ggml_backend_graph_compute(backend, g.graph) != GGML_STATUS_SUCCESS);
    check(g, true);
  } else if (mixed) {
    assert(rc == GGML_STATUS_SUCCESS && off == 0 && local == 2 && fail == 0);
    assert(macs == (uint64_t)M * K * N * 2 && exchanges == 0 && wire_macs == 0);
    check(g);
    auto remote = graph(ctx, weights, 32);
    assert(ggml_backend_graph_compute(backend, remote.graph) ==
           GGML_STATUS_SUCCESS);
    check(remote);
    ggml_backend_shielded_stats(&off, &local, &macs, &fail);
    ggml_backend_shielded_verified_stats(&exchanges, &wire_macs);
    assert(off == 2 && local == 2 && fail == 0);
    assert(macs == (uint64_t)(M + 32) * K * N * 2);
    assert(exchanges == 1 && wire_macs == (uint64_t)32 * K * N * 2);
  } else {
    assert(rc == GGML_STATUS_SUCCESS && off == 2 && local == 0 && fail == 0);
    assert(exchanges == (uint64_t)(M + 31) / 32 &&
           wire_macs == (uint64_t)M * K * N * 2);
    check(g);
    if (mode == "exhaustion") {
      auto next = graph(ctx, weights, M);
      assert(ggml_backend_graph_compute(backend, next.graph) != GGML_STATUS_SUCCESS);
      check(next, true);
      assert(ggml_backend_graph_compute(backend, next.graph) != GGML_STATUS_SUCCESS);
      check(next, true);
      uint64_t after_off = 0, after_local = 0, after_mac = 0, after_fail = 0;
      ggml_backend_shielded_stats(&after_off, &after_local, &after_mac, &after_fail);
      assert(after_off == off && after_local == local && after_mac == macs && after_fail == 0);
      ggml_backend_shielded_verified_stats(&exchanges, &wire_macs);
      assert(exchanges == (uint64_t)(M + 31) / 32 && wire_macs == (uint64_t)M * K * N * 2);
    }
  }
  ggml_backend_free(backend);
  uint64_t after_exchanges = 999, after_macs = 999;
  ggml_backend_shielded_verified_stats(&after_exchanges, &after_macs);
  assert(exchanges == after_exchanges && wire_macs == after_macs);
  ggml_backend_shielded_verified_stats(nullptr, nullptr);
  printf(
      "{\"rows\":%d,\"mode\":\"%s\",\"authenticatedWeights\":%d,\"offloaded\":%"
      "llu,\"local\":%llu,\"graphMacs\":%llu,\"verifiedExchanges\":%llu,"
      "\"verifiedMacs\":%llu,\"verificationFailures\":%llu,\"passed\":true}\n",
      M, mode.c_str(), calls, (unsigned long long)off,
      (unsigned long long)local, (unsigned long long)macs,
      (unsigned long long)exchanges, (unsigned long long)wire_macs,
      (unsigned long long)fail);
  ggml_free(ctx);
}

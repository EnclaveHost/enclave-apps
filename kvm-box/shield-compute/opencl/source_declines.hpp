// Optional build-time hints which only select the existing trusted CPU path.
// A match never provides a plan, approves data, or skips dispatch verification.
#pragma once
#include <algorithm>
#include <array>
#include <cstdint>
#include <string_view>
#include <openssl/evp.h>

namespace shield_source_declines {
#include SHIELD_SOURCE_DECLINES

constexpr bool valid_table() {
  for (size_t i = 0; i < fingerprints.size(); ++i) {
    const auto key = fingerprints[i];
    if (key.size() != 64 || (i && !(fingerprints[i - 1] < key))) return false;
    for (const char c : key)
      if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
  }
  return true;
}
static_assert(valid_table(), "decline hints must be sorted unique SHA-256 strings");

inline bool contains(std::string_view source, std::string_view options) {
  // Include the explicit NUL separator but exclude the implicit C terminator.
  constexpr char prefix[] = "shield-source-decline-v1\0";
  auto *ctx = EVP_MD_CTX_new();
  if (!ctx) return false;
  bool ok = EVP_DigestInit_ex(ctx, EVP_sha256(), nullptr) == 1 &&
            EVP_DigestUpdate(ctx, prefix, sizeof(prefix) - 1) == 1;
  for (auto part : {source, options}) {
    std::array<unsigned char, 8> length{};
    const uint64_t size = part.size();
    for (unsigned i = 0; i < length.size(); ++i)
      length[i] = static_cast<unsigned char>(size >> (8 * i));
    ok = ok && EVP_DigestUpdate(ctx, length.data(), length.size()) == 1 &&
         EVP_DigestUpdate(ctx, part.data(), part.size()) == 1;
  }
  std::array<unsigned char, 32> digest{};
  unsigned length = 0;
  ok = ok && EVP_DigestFinal_ex(ctx, digest.data(), &length) == 1;
  EVP_MD_CTX_free(ctx);
  if (!ok || length != digest.size()) return false;
  constexpr char hex[] = "0123456789abcdef";
  std::array<char, 64> encoded{};
  for (unsigned i = 0; i < digest.size(); ++i) {
    encoded[2 * i] = hex[digest[i] >> 4];
    encoded[2 * i + 1] = hex[digest[i] & 15];
  }
  return std::binary_search(fingerprints.begin(), fingerprints.end(),
                            std::string_view(encoded.data(), encoded.size()));
}
} // namespace shield_source_declines

# Eyesoff-AI search-context release, 2026-09-30

Version **1.0.70**, source `b699f6c`, was activated for the existing Eyesoff-AI
deployment. The release bounds search source text using query-relevant,
verbatim excerpts, preserves source links and citation markers, and labels
omissions. See [the behavior and tradeoff](search-context.md).

The inference runtime remains `4bb9f020`; the staged compact-layout runtime
`5d48542a` and unpublished request-scoped speculative control are not part of
this rollout. No isolation, masking or verification checks were relaxed.

## Activation and identity

- Activation transaction: `0x6a05cd0cf1ea60e61118488a81e653f32fffa49c27fc16fcbaeeec44443f9630`
- Base block: `51996920`; receipt status: success.
- Deployment: `0x9eb4e60063aa079cebed355f96b2d049457ae77bdbcd49086040282e1e4b871c`
- Guest: `gde77e3ac6` on metal0.
- WASM SHA-256: `c1bd531eb264f8b66585d5ad6933452099506905a6e5c49fbaa7537bbed071a2`
- App identity: `4a9188f0fb55b6ce56ac98bd1aae9fada17df6639bf594aa9c60e43473506b45`
- Measurement: `f89271b101c6817396dddea56b180fd6234451e4b2cf92f7cac03247e6fd01140d32eb64ff18059e2eb3c9510be25099`

Fresh nonce-bound SNP verification passed at the app address and eyesoff.ai,
including deployment binding, runtime identity, minimum TCB and TLS SPKI.
Ordinary public HTTPS certificate validation subsequently passed with HTTP 200.
The five other app guests remained running.

The initial replacement create was refused by the unchanged 16-GiB host
memory floor. Closing our completed browser tabs and resetting our browser
control worker freed about 1.7 GiB; the next scheduler retry admitted the
replacement. No shared host service or other app was restarted. New guest
certificates were issued automatically; the custom domain became ready once
its certificate was installed.

## Validation

The artifact had passed 224 Rust tests, 19 browser tests, and 84 relay tests.
Its retrieved content hash matched the published artifact. Controlled local
0.5B tests reduced prompt tokens from 3,728 to 1,702 and prefill from 41,645 ms
to 14,172 ms with the same answer. Those are **not production 27B timings**.
The previous production search baseline ended in `kv_pool_full`, so it is
not a valid before/after latency comparison.

Startup prefix warmup completed. A live search with general tool-loop support
retrieved six NASA sources in 2,384 ms, then failed with `kv_pool_full` after
296,880 ms (4,347 prompt tokens; last progress at 4,275). The user confirmed
a concurrent request. No user sessions were inspected or evicted. This is
an unresolved capacity limitation, not evidence that the search fix solves
concurrent KV pressure. A smaller search-only functional check completed with six sources and a cited
two-sentence answer: 3,362 prompt tokens, 241,696 ms prefill, first delta at
244,449 ms, and 89 generated tokens in 5,684 ms (15.7 tok/s). Search retrieval
took 2,050 ms; total request time was 250,007 ms. Both requests ran during
user activity and are functional checks, **not isolated latency benchmarks**.
Prefill remains slow; no production latency percentage improvement is claimed.

Structured release evidence is in
[evidence/search-release-20260930.json](evidence/search-release-20260930.json).
Raw preparation and verification artifacts remain in
`/home/steven/enclave-bench/eyesoff-search-prefill-20260930`.

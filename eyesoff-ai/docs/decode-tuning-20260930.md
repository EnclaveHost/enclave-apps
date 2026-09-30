# Decode investigation, 2026-09-30

Status: **no production change and no validated throughput gain yet**.
Production remains app 1.0.69 on compact64 runtime `4bb9f020`.
Published app 1.0.70 is still inactive; its previously prepared activation was
deferred at the owner's request. The new request control described below is
not in either published artifact. Publish it under a new version if selected.

## Current application baseline

Four serial, authenticated `/chat` requests to eyesoff.ai, with the first
excluded as warmup. Prompt: “Write a vivid short story about a robot tending a
garden on the Moon.” Model `qwen3.8-27b-mtp`, 128-token cap, temperature zero,
thinking/search/image generation/tools off. All answer hashes matched.
Offline CPU experiments finished before this profile; no concurrent benchmark
model was loaded onto the serving V100s.

| Warm sample | Decode | First visible token | Cached prefill |
| --- | ---: | ---: | ---: |
| 1 | 14.7 tok/s | 818 ms | 1 ms |
| 2 | 14.6 tok/s | 997 ms | 1 ms |
| 3 | 14.9 tok/s | 823 ms | 1 ms |

Aggregate: **14.713 tok/s** (384 tokens / 26.099 decode seconds).
This is the unchanged runtime's current baseline, not an optimization result.
The earlier 14.075 tok/s result demonstrates why small microbenchmark gains
cannot be credited as application improvements without an alternating A/B.

Each sample accepted 49 of 79 MTP proposals, with zero speculative-gate wait.
The target verification calls account for about 95% of measured decode time;
the draft head accounts for about 4%. Target-call timing includes execution
below the app boundary, and does not distinguish CPU pad waits, transport,
or GPU compute. It does not prove MTP is slower than plain decode.

## Offline kernel experiments

Public GGUF tensors, deterministic synthetic masks, existing CRT oracle,
byte-for-byte weight readback, rotated variant ordering, one low-priority CPU
thread, bounded RAM, and no swap or GPU/network access. No live mask banks or
seeds were read. Medians exclude the first two repetitions.

* Alternative bit-plane instruction sequences and 128/256/512/768/1024-row
  tiles had inconsistent wins across tensor shapes. None selected.
* Vectorized field reduction, VBMI decompression, and fused mask validation
  also lacked a reliable overall gain. No runtime arithmetic changed.
* Transposed signed GEMM variants failed the exact-output oracle and were
  rejected immediately. A transposed unsigned version passed the measured
  oracle but was roughly 60–65% slower on large batches; stopped early.
* The beginning of the fused-validation trial overlapped the tail of that
  rejected experiment on the same core. Its timings are not qualification
  evidence; it was not selected.

Results and hashes are in [the evidence](evidence/decode-tuning-20260930.json).
Scratch sources, build scripts and raw logs remain in
`/home/steven/enclave-bench/shield-decode-tuning-20260930`.
These kernel timings are not tok/s measurements, and none of the experimental
implementations has been installed into a runtime.

## Undeployed comparison control

`"speculative": false` now selects the existing plain-decode path for only
that request's answer/tool loop, including child agents. The same switch is
honored by `/chat` and streaming/buffered `/v1/chat/completions`. Router and
title helpers already use plain decode. `true`, null, or an absent field
retains existing configuration and capability checks; it cannot enable a
new draft model. Invalid non-boolean values are rejected by request parsing.
No sampling, masking, verification, storage, or isolation behavior is changed.

Before measuring a future test deployment, verify that `/chat` emits
`speculative decode off: disabled for this request` for the plain run. Older
versions silently ignore unknown request fields, so submitting the flag to
1.0.69 or 1.0.70 does **not** produce a valid A/B. Also require zero drafted
tokens in plain runs and nonzero MTP drafts in configured runs.

Warm both modes, alternate request order across several fixed prompts, and
compare equal token budgets, actual output hashes, prefill, gate wait,
decode time and total latency. Alternate modes on repeated prompts to test
prefix-cache compatibility; also test a fresh prompt and a tool-result turn.
Do not reuse the native non-speculative benchmark as the application's plain
baseline: it runs outside the isolated guest. This end-to-end comparison is
still outstanding and requires a test deployment or a separately scheduled
production test window. No production rollout is authorized by this note.

Validation: 226 native Rust tests passed, including opt-out locality,
unchanged defaults, capability restrictions, and strict request parsing.
The optimized `wasm32-wasip2` build also passed. The unpublished test artifact
and its SHA-256 are recorded in the evidence; it has not been performance
qualified inside the serving guest.

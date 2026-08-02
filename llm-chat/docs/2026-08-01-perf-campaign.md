# 2026-08-01 performance campaign — rollout notes

One overnight-and-a-day campaign across the engine (enclave repo, wasmtime
toolchain mm2→mm6) and this app (0.26.0→0.32.1). Everything below is live
on the fleet engine; the app side ships to users when a catalog version
carrying it is published.

## What a user gets from publishing 0.32.1

| axis | before | after | change |
|---|---|---|---|
| decode | ~59 tok/s (39 on the shipped mtp16 config) | 62–65 tok/s | topk sparse logits, drop `draft` keys |
| prefill (792-tok prompt) | 800–1150 ms | ~360 ms | n_batch-aware chunks (host was already at 512) |
| TTFT app-side | ~1.9 s | ~1.4 s | host tokenizer (`tokenizer: "host"`) |
| tokenizer init | ~500 ms/request | 21–28 ms | GGUF-side encode + piece table |

**Recommended catalog config changes** (see `~/llm-chat-fast-config.json`
on the workstation, ready for `enclave config set`):

- remove `draft`, `draft_tokens`, `draft_p_min` (speculation loses on this
  model — measured, see below)
- add `"tokenizer": "host"` (falls back to local automatically on engines
  that predate the verbs)

## Why speculation is off

Measured with the per-verb timing instrument (`verb_us` in the /chat done
frame): a verify pass costs ~46 ms for 5 tokens and ~109 ms for 9 — the
cost scales ~linearly with batch size because the model is expert-sparse
(k distinct tokens route to ~k expert sets; batching amortizes nothing).
Even perfect drafts can't out-earn a 15.5 ms plain step. This holds for
any expert-sparse target; dense targets keep the classic speculation win.
`draft: "lookup"` (prompt-lookup, added 0.28.x) is safe everywhere —
no-match rounds are plain decode — but not faster here.

## Where the remaining latency lives (all measured)

- ~700 ms cold-connection transport floor (350 ms TLS, skipped on
  keep-alive) + ~350 ms proxy/auth hop — infra surface, not app
- ~170 ms effort router (its quality battery is 2026-07-30, 8/8 — tune
  only against it)
- ~360 ms prefill, ~25 ms tokenizer init, 15.5 ms first step
- decode steady state 15.1–15.5 ms/step = CUDA-graph replay floor under
  confidential compute (warmup visibly 20–24 ms for the first steps)

## Validation trail

- host-vs-local tokenizer byte-identity (local hybrid smoke + fleet: same
  output hashes, same prompt token counts)
- /chat, /title, /v1 streaming, /v1 stop strings, /v1 non-stream all
  exercised on the fleet post-refactor
- 119 cargo tests incl. the new seams (piece-table UTF-8 withholding,
  sparse rows, lookup semantics, sparse sampler); CI runs them on every
  push/PR (`.github/workflows/llm-chat-test.yml`), and the engine repo
  CI-checks the wasmtime patch stack
- instruments ship in the app: `verb_us` (per-verb µs), `init_ms`
  (pre-generation phases), `feed_warm` (graph warmup visibility)

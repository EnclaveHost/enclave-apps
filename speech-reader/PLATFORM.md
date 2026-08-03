# The `audio` verb: host-side spec

This app needs ONE addition to the platform (`EnclaveHost/enclave`): an
`audio` input verb on the ggml wasi-nn backend, the exact mirror of the
existing `image` verb. Everything hard already exists — the pinned llama.cpp
(bec4772, 2026-07-07) compiles the granite-speech conformer graph
(`tools/mtmd/models/granite-speech.cpp`, `conformer.cpp`,
`whisper-enc.cpp`), libmtmd's miniaudio already decodes wav/mp3/flac
(that is why `ell_mtmd_eval_image` can *detect and reject* audio bitmaps
today), and the shim's capability probe already reports the audio bit.

## 1. Shim (`wasm/llama-shim/enclave_llama.{c,h}`)

**`ell_mtmd_caps_file`** (`enclave_llama.c:645`) — no change. It already
returns `(inp_vision ? 1 : 0) | (inp_audio ? 2 : 0)`; the backend just never
consumes bit 2.

**`ell_mtmd_new`** (`enclave_llama.c:667`) — relax the vision-only gate:

```c
if (!mtmd_support_vision(c) && !mtmd_support_audio(c)) {
    mtmd_free(c);   /* projects nothing this backend can feed */
    return NULL;
}
```

(The comment "an audio-only projector against an image verb: refuse at load"
stops being true the moment the audio verb exists; the per-verb checks below
keep the cross-modality refusals.)

**`ell_mtmd_eval_audio`** — a sibling of `ell_mtmd_eval_image`
(`enclave_llama.c:686`), same signature, three deltas:

- the bitmap check inverts: `if (!mtmd_bitmap_is_audio(w.bitmap)) return 2;`
  (an image against the audio verb is undecodable-for-this-verb, same code)
- the non-causal/ubatch guard can stay as-is (it only inspects IMAGE chunks
  and audio chunks decode causally) or be dropped;
- `logits_last=false` stays: the instruction text follows the audio, exactly
  as image-reader's question follows the picture.

Return codes keep their meanings: 0 ok, 1 eval failure, 2 undecodable
(miniaudio could not read the bytes — wav/mp3/flac are what it reads), 3
unused. Header docs mirror `ell_mtmd_eval_image`'s block
(`enclave_llama.h:236-245`).

## 2. Backend (`wasm/wasmtime-nn-ggml.patch`)

- **dlsym** `ell_mtmd_eval_audio` alongside `ell_mtmd_eval_image`
  (patch ~line 454, the `MtmdSyms` block). Optional symbol, like the rest:
  an older shim tarball simply lacks it and the verb refuses with
  `[audio_unavailable]` — the same graceful-degradation contract vision has
  (patch ~line 409).
- **caps** — one new slot, index 7 (after `rewind_depth`):
  `caps[7] = audio`, true when the volume's mmproj reports the audio bit
  (bit 2 of `ell_mtmd_caps_file`, already surfaced today at patch ~line 735
  for vision) AND the shim exports `ell_mtmd_eval_audio`. Guests read
  missing slots as "no", so old hosts stay honest automatically.
- **dispatch** — next to the `image` arm (patch ~line 1714):

  ```
  {"audio": U8 [n]} = one audio FILE (wav/mp3/flac, whatever miniaudio
  reads); encode + project + splice into the sequence at the current
  position; answer {"audio_pos": I32 [1]} = positions consumed.
  ```

  The lazy mtmd load path is shared with vision verbatim (same mmproj file,
  same `ENCLAVE_GGML_MTMD_THREADS`, same deferred-VRAM reasoning); only the
  eval call and the error markers differ.
- **error markers** — `[audio_unavailable]` (no libmtmd / no audio-capable
  projector / shim predates the symbol), `[audio_undecodable]` (rc 2: not
  wav/mp3/flac). `[kv_pool_full]` flows through unchanged.

## 3. Volume registration (`enclaves/gpu/tinfoil-config.yml`)

```yaml
  - name: "granite-speech-4.1-2b-gguf"
    repo: "EnclaveHost/granite-speech-4.1-2b-gguf@<rev>"
```

built by this app's `fetch-model.sh` (IBM's own GGUFs, sha-pinned:
Q8_0 LM + f16 conformer mmproj + tokenizer.json). The mmproj rides the
existing `*mmproj*` naming convention, so volume plumbing needs nothing new.

## What the guest already does about all of this

`src/nn.rs` reads caps slot 7, maps unknown-verb host errors to
`[audio_unsupported]` with a sentence pointing here, and
`GET /health?probe=1` answers "can this node hear" without sending a clip.
The app is deployable before the platform change lands — it just says, in
words, exactly what is missing.

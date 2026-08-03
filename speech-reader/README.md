# speech-reader

Speech to text inside a TEE, as a `wasm32-wasip2` component on Enclave's
wasi-nn GPU interface. Record or upload audio, get the transcript streamed
back; the recording never leaves the enclave and is kept by no one.

The model is **[Granite Speech 4.1 2B](https://huggingface.co/ibm-granite/granite-speech-4.1-2b)**
(IBM, Apache-2.0): a conformer audio encoder + window q-former projector
feeding a dense granite LM with the speech adapters merged. Six languages
(en fr de es pt ja), punctuated transcription, and speech translation to
English.

## Why this model

"Highest quality open source STT" has a moving leaderboard, but a stable
shape: at mid-2026 the Open ASR top ten is Granite Speech 4.1 2B (**5.33%
mean WER**), Canary-Qwen 2.5B (5.63%), Granite Speech 3.3 8B (5.85%), and a
tail of NeMo/PyTorch-runtime models this fleet cannot run — the GPU
interface is llama.cpp behind wasi-nn, and no tenant kernels is settled
platform law. Granite Speech 4.1 2B is the one at the very top that is
*also* llama.cpp-shaped, with **IBM's own GGUF conversion** and the encoder
graph already compiled into the fleet's pinned llama.cpp
(`tools/mtmd/models/granite-speech.cpp` at bec4772). Whisper large-v3 — the
name people expect here — sits at ~7.4% WER *and* would need a whole new
host backend (whisper.cpp); Voxtral's current leaderboard entry is 7.68%.
The best model and the zero-new-backend model are, conveniently, the same
model.

## How it runs (the split)

```
  audio file (wav/mp3/flac bytes)
        │
        ▼  wasi-nn "audio" verb - the image verb's mirror (PLATFORM.md)
  miniaudio decode -> log-mels -> conformer encoder -> q-former projector
        │              (all host-side, inside the model's own mmproj)
        ▼  "audio_pos" = positions consumed
  guest renders "USER: [audio]<instruction>\n ASSISTANT:" around it,
  decodes greedily, streams the transcript
```

- **The host needs one small addition**: an `audio` input name on the ggml
  backend, dispatching to the same libmtmd machinery the `image` verb uses.
  [PLATFORM.md](PLATFORM.md) is the complete spec with file/line anchors —
  the shim already links miniaudio's decoder and already *reports* the audio
  capability bit; it has just never been asked. Until a node carries it,
  this app refuses recordings with `[audio_unsupported]` and the sentence
  that names the fix; `GET /health?probe=1` asks the host directly.
- **The volume looks exactly like a vision volume**: LM gguf + `*mmproj*`
  gguf (here the audio encoder) + tokenizer.json. No new volume plumbing.
- **Long audio chunks itself**: the model is trained at 4096 positions
  (~10 audio positions/second), so a long WAV is cut at the quietest 100 ms
  near each ~4-minute boundary into separate episodes, each a fresh session;
  segment offsets are reported in `verbose_json`. Compressed inputs
  (mp3/flac) pass whole under a byte cap. ogg/opus/webm are refused *by
  name* — which is why the playground records raw PCM into its own WAV
  instead of trusting MediaRecorder.
- **Greedy decode, penalties off** — sampling noise on an ASR model is only
  differently wrong, and real speech repeats ("no, no, no") so the
  repetition *penalty* stays off while the exact-block loop *guard* (hold
  music's best friend) stays on.

## Routes

| Route | What |
| --- | --- |
| `GET /` | The playground: mic recording (16 kHz WAV built in-page), file drop, live transcript. |
| `GET /ping` | Liveness; touches no wasi-nn. |
| `GET /health` | Volumes, VRAM budget, node tuning; `?probe=1` opens a session and asks the host whether this node can hear. |
| `GET /models` | The catalog (with `hears`/`why_deaf` per volume). |
| `POST /transcribe` | The playground's SSE engine: raw audio body (or multipart, or JSON `{audio: base64}`); `?task=translate` switches task. |
| `GET /v1/models` | OpenAI-shaped catalog. *(Bearer-gated when api_key is set)* |
| `POST /v1/audio/transcriptions` | OpenAI-compatible: multipart `file`, `response_format` json\|text\|verbose_json. *(Bearer-gated)* |
| `POST /v1/audio/translations` | Same shape, translate-to-English. *(Bearer-gated)* |

```bash
# the one-liner
curl -s --data-binary @meeting.wav https://<deployment>/transcribe

# OpenAI SDK, pointed at the deployment
from openai import OpenAI
client = OpenAI(base_url="https://<deployment>/v1", api_key="<key>")
text = client.audio.transcriptions.create(
    model="granite-speech-4.1-2b", file=open("meeting.wav", "rb"),
    response_format="text")
```

Two OpenAI-compat notes, both deliberate. `prompt` **replaces** the task
instruction rather than "guiding style" — on this model the instruction *is*
the task, and pretending otherwise would be the lie. And long jobs stream
**newline keepalives ahead of the JSON** (leading whitespace is valid JSON;
every SDK's `.json()` eats it) because the fleet's gateway cuts a response
~180 s after its last byte and an hour of audio is minutes of work.
`srt`/`vtt` formats are refused: word timestamps are the `-plus` model
variant's trick, not this one's.

## Privacy, precisely

The component imports `wasi:nn` and `wasi:http`'s *incoming* handler and
nothing else. No outbound socket exists in this world: a recording sent here
cannot be forwarded anywhere, is never written anywhere (the model volume is
the only mount, read-only), and exists in enclave RAM for the length of one
request. The attestation covers the component; a caller can verify all of
that rather than believe it.

## The model volume

```bash
./fetch-model.sh     # builds model-volume/granite-speech-4.1-2b-gguf/ from pinned sha256s
```

| File | What | From |
| --- | --- | --- |
| `granite-speech-4.1-2b-Q8_0.gguf` (2.0 GB) | the LM, adapters merged | `ibm-granite/granite-speech-4.1-2b-GGUF` @ `8267dad` |
| `mmproj-model-f16.gguf` (1.2 GB) | conformer encoder + projector, f16 (quantizing the encoder is where accuracy dies) | same repo |
| `tokenizer.json` (4 MB) | guest-side tokenization | `ibm-granite/granite-speech-4.1-2b` @ `de575db` |

For the fleet: publish as `EnclaveHost/granite-speech-4.1-2b-gguf`, register
in the platform's `enclaves/gpu/tinfoil-config.yml`, and ship the audio verb
(PLATFORM.md) in the same shim/manager release.

## Build, test, deploy

```bash
cargo test                                            # host-target unit tests
cargo component build --release --target wasm32-wasip2
# -> target/wasm32-wasip2/release/speech_reader.wasm (~2.9 MB)

enclave publish target/wasm32-wasip2/release/speech_reader.wasm \
  --slug speech-reader --name "Speech Reader" --version 1 \
  --mem 256 --cpu-gflops 10 --ports "http:8000"
enclave deploy speech-reader:1 --gpu 0.07 --cpu 0.02 --fund 5
```

Sizing is friendly: 2 GB weights, only 4 KV heads (~21 KiB/token at q8_0
K/V, so even a 176K node window costs ~3.7 GB), the 1.2 GB encoder priced in
because it loads lazily on the first clip — ~9 GB ≈ gpu 0.07. **Trust
`GET /health` over these numbers**; it computes from the node's own
environment and refuses what cannot fit rather than aborting the tenant.

Local smoke test: `fetch-model.sh` prints the `wasmtime serve` incantation
(CPU-only ggml, end to end once the local shim carries the audio verb).

## Files

```
src/lib.rs        routes, the three request transports, SSE + keepalive plumbing
src/nn.rs         volumes, VRAM guard, session queue, caps, the transcribe loop
src/audio.rs      sniffing, WAV parse, downmix, quiet-point chunking
src/multipart.rs  the form-data parser OpenAI SDKs need
src/sampling.rs   temperature/top-k/top-p sampler (greedy by default here)
src/config.rs     embedded defaults + ENCLAVE_CONFIG overlay
PLATFORM.md       the audio-verb spec for the platform repo
fetch-model.sh    the pinned volume recipe
```

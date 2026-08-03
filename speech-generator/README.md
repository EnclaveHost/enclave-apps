# speech-generator

Text to speech inside a TEE, as a `wasm32-wasip2` component on Enclave's
wasi-nn GPU interface. Type text, get a spoken 24 kHz WAV stream back; the
text never leaves the enclave and the audio exists only in your response.

The model is **[Maya1](https://huggingface.co/maya-research/maya1)**
(maya-research, Apache-2.0): a Llama-3.2-3B that predicts
[SNAC](https://github.com/hubertsiuzdak/snac) audio-codec tokens, with the
voice designed in natural language and 20+ inline emotion tags:

```
voice:  "Male voice in his 40s, deep and calm, American accent, measured pace"
text:   "I can't believe it <laugh> that actually worked!"
```

## Why this model

"Best available open TTS" was the brief, and by mid-2026 the open models at
the top of the blind-test boards split into two piles: the ones that need a
PyTorch runtime and a custom inference stack (Chatterbox, OpenAudio S1,
VibeVoice, Dia2 - some of them non-commercial-licensed on top), and the ones
that are **an ordinary Llama plus an audio codec** (Maya1, Orpheus, OuteTTS).
Only the second pile can run here at all, because the fleet's GPU interface is
llama.cpp behind wasi-nn and [no tenant kernels, ever] is settled platform
law. Within that pile Maya1 is the newest and strongest: Apache-2.0,
voice-by-description (no fixed speaker list), emotion tags, and quality that
holds up against much larger closed systems. Orpheus-3B is the same
architecture a year earlier; OuteTTS trades quality for size. Kokoro-82M -
the famous small one - is ONNX + espeak-ng G2P, i.e. a different runtime AND
a phonemizer this component would have to carry; it won on none of the axes
that matter here.

## How it runs (the split)

```
 text ──tokenize──> [SOH][BOS]<description="..."> text[EOT][EOH][SOA][SOS]
                        │
                        ▼  wasi-nn "tokens" verb, dense logits back
              Maya1 3B on the GPU share          (the host thinks it's an LLM)
                        │  guest samples SLOT-CONSTRAINED audio tokens
                        ▼
        7-token frames -> SNAC codebook streams (12/23/47 Hz)
                        │
                        ▼  src/snac.rs - 13M-param convnet, pure Rust, in-wasm
              SNAC 24 kHz decoder on the CPU
                        │
                        ▼
              16-bit WAV, streamed as it decodes
```

- **The host needs no audio support.** Maya1 is an ordinary GGUF volume to
  it; any node that serves llm-chat serves this. The audio protocol lives
  entirely in which token ids the guest samples and what it does with them.
- **The guest owns sampling** (the host returns the full dense logits row),
  so the sampler enforces the frame structure *by construction*: at step `g`
  only the 4096 ids of slot `g % 7` are candidates (plus the audio EOS at
  frame boundaries after the minimum length). A malformed frame is not
  unlikely - it is impossible.
- **The SNAC decoder is in-component** (`src/snac.rs`): the official 24 kHz
  decoder's exact architecture - snake activations, transposed convs, noise
  blocks - hand-written in dependency-free Rust with tiled matmul kernels,
  loading weight-norm-fused f16 weights from the model volume. Validated
  against the official PyTorch implementation at **52-59 dB SNR** (the f16
  quantization floor); chunked/streamed decode is bit-exact against
  whole-take decode; ~2-3x realtime on one x86 core.
- **Long text** is split at sentence boundaries into ~600-char episodes (the
  single-episode budget of every SNAC-token model is ~25 s of audio); each
  episode is a fresh session, and the WAV stream stitches them.
- **The response streams**: the WAV header goes out before the session
  opens, decoded frames follow as they earn their halo context, and waits
  are kept alive with milliseconds of silence - so the gateway's ~180 s
  idle cut never fires and you hear speech while the model still talks.

## Routes

| Route | What |
| --- | --- |
| `GET /` | The playground: voice picker, emotion-tag chips, progressive playback. |
| `GET /ping` | Liveness; touches no wasi-nn. |
| `GET /health` | Volumes, VRAM budget, node tuning; `?probe=1` opens a real session AND parses the volume's SNAC decoder - the two halves of "will this deployment speak". |
| `GET /voices` | Preset table (name → description) and the default. |
| `GET /models` | The catalog. |
| `GET\|POST /speak` | Text/voice/description as query params or JSON; WAV stream back. |
| `GET /v1/models` | OpenAI-shaped catalog. *(Bearer-gated when api_key is set)* |
| `POST /v1/audio/speech` | OpenAI-compatible TTS: `{model, input, voice, instructions, response_format: wav\|pcm}`. `instructions` is the voice description - the same meaning the field has on OpenAI's endpoint. *(Bearer-gated)* |

```bash
# the one-liner
curl -o hello.wav 'https://<deployment>/speak?text=Hello+from+the+enclave&voice=sage'

# OpenAI SDK, pointed at the deployment
from openai import OpenAI
client = OpenAI(base_url="https://<deployment>/v1", api_key="<key>")
audio = client.audio.speech.create(
    model="maya1", voice="onyx", input="The attestation checks out. <chuckle>",
    response_format="wav")

# a voice nobody preset: put the casting brief in `voice` or `instructions`
curl -o demon.wav 'https://<deployment>/speak' -d '{
  "text": "Welcome to my lair! <laugh_harder>",
  "description": "Demon voice, deep and gravelly, slow menacing pace, theatrical"
}'
```

`voice` accepts a preset name (`aria`, `marcus`, `poppy`, `sage`, `iris`,
`kit`, plus the OpenAI names `alloy`/`echo`/`fable`/`onyx`/`nova`/`shimmer`
mapped to fitting descriptions) or a free-form description. `seed` makes a
take reproducible. `response_format: pcm` is raw s16le mono 24 kHz (exactly
OpenAI's `pcm`). There is no mp3/opus encoder in this world, deliberately:
WAV is universal and an encoder is attack surface the component does not
need. `speed` is refused rather than faked - ask the description for a
faster speaker.

Emotion tags ride inline in the text: `<laugh> <giggle> <chuckle> <sigh>
<gasp> <whisper> <cry> <angry> <excited> <sing> <snort> <scream>` and more -
unknown tags are simply read out, so the worst case is audible, not broken.

## Privacy, precisely

The component imports `wasi:nn` and `wasi:http`'s *incoming* handler and
nothing else. No outbound socket exists in this world: text sent here cannot
be forwarded anywhere, and the audio is never stored (the model volume is the
only mount, read-only). The attestation covers the component, so a caller can
verify all of that rather than believe it.

## The model volume

```bash
./fetch-model.sh     # builds model-volume/maya1-gguf/ from pinned sha256s
```

| File | What | From |
| --- | --- | --- |
| `maya1-q8_0.gguf` (3.5 GB) | the voice model | `Mungert/maya1-GGUF` @ `83fad52` |
| `tokenizer.json` (23 MB) | guest-side tokenization | `maya-research/maya1` @ `21c682a` |
| `snac_decoder.bin` (26 MB) | the guest-side codec, **derived**: checkpoint → weight-norm fuse → f16 container | `hubertsiuzdak/snac_24khz` @ `d73ad17` via `tools/export_snac.py` |

The derivation is deterministic (byte-identical output, sha256 pinned in
`fetch-model.sh` like any fetched file; needs python3 + torch + numpy).
Q8_0 rather than a smaller quant because audio-token models degrade into
*audible artifacts* - clicks, warbles, broken prosody - before text models
degrade into wrong words, and against a 3.5 GB total the savings would be
noise. `tools/export_snac.py --golden <dir>` emits the reference vectors
(official implementation, noise zeroed) that the Rust decoder is validated
against.

For the fleet: publish the volume as an `EnclaveHost/maya1-gguf` HF repo
built by this recipe, register it in the platform repo's
`enclaves/gpu/tinfoil-config.yml` at that revision, and it appears in the
console's volume picker under the name the catalog keys on.

## Build, test, deploy

```bash
cargo test                                            # host-target unit tests
cargo component build --release --target wasm32-wasip2
# -> target/wasm32-wasip2/release/speech_generator.wasm (~2.9 MB)

enclave publish target/wasm32-wasip2/release/speech_generator.wasm \
  --slug speech-generator --name "Speech Generator" --version 1 \
  --mem 512 --cpu-gflops 10 --ports "http:8000"
enclave deploy speech-generator:1 --gpu 0.12 --cpu 0.05 --fund 5
```

`--mem 512`: the SNAC decoder wants ~150 MB per request (weights parsed to
f32 plus activations); 512 is comfortable headroom for a few concurrent
takes. `--cpu 0.05` buys the codec's realtime margin - the decode runs on
the wasm CPU, ~2-3x realtime per core natively and slower under wasm, so a
starved CPU share is the one thing that makes audio arrive slower than it
plays. Sizing detail for the GPU share is in
`assets/deploy-config.template.json`; **trust `GET /health` over any number
written here** - it computes from the node's own environment and refuses
models that don't fit rather than aborting the tenant trying.

Local smoke test without a GPU host: `fetch-model.sh` prints the `wasmtime
serve` incantation (ggml on CPU with `N_GPU_LAYERS=0` - slow, but end to
end), then `curl -s -o hello.wav '127.0.0.1:8170/speak?text=hello'`.

## Files

```
src/lib.rs        routes, auth, the streaming WAV plumbing
src/nn.rs         volumes, VRAM guard, session queue, the speak loop
src/sampling.rs   slot-constrained audio sampler (temp/top-p/rep-penalty)
src/maya.rs       Maya1 token protocol: prompt ids, frame unpack, text chunking
src/snac.rs       SNAC 24 kHz decoder, pure Rust, tiled kernels
src/wav.rs        streaming WAV header + pcm16
src/config.rs     embedded defaults + ENCLAVE_CONFIG overlay + voice table
tools/export_snac.py  checkpoint -> SNACDEC1 container (+ --golden vectors)
fetch-model.sh    the pinned volume recipe
```

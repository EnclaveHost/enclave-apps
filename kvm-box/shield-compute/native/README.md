# Native engines and the guest compute boundary

This integration runs native llama.cpp and stable-diffusion.cpp inside the
desktop SNP VM. It does not implement a CUDA driver or claim that arbitrary
CUDA binaries run unchanged. General GPU API/compiler compatibility remains
part of the unfinished acceptance ledger in `../IMPLEMENTATION.md`.

The separate standard-OpenCL application experiment is documented in
`OPENCL_APPLICATION.md` and `STANDARD_SD_MASKED.md`. Both applications now
execute verified masked Q8 operations in host tests. `STANDARD_PACKAGE.md`
describes the candidate image and the remaining VM acceptance work.

## Trust and execution

The desktop calls a guest-local Unix socket. A separate uid runs a measured
service, which selects fixed native binaries, model paths, calibration and
public-weight policy. Requests cannot supply environment variables, arbitrary
commands, libraries, worker addresses or public-data approvals. Model files
are mounted read-only with dm-verity; their root is part of the measured image.
The service also checks whole-file model and policy digests at startup. Before
an offload, the backend authenticates a private tensor copy against the exact
approved name, type, dimensions, byte count and SHA-256.

The only remote computation is the existing Shield verified masked field
protocol. No CUDA/Vulkan driver or raw GPU device is installed. Desktop
processes inherit a seccomp filter denying VSOCK, raw packet sockets and
io_uring; `no_new_privs` prevents privilege escalation through exec. nftables
denies their direct route to the host worker. Only uid 1001 can reach its fixed
endpoint. The existing root attestation proxy retains access to its two pinned
enclave-service ports. Models, masks, pads, private activations and unmasked
results remain in guest memory; no host shared-memory pad transport is enabled.

This llama.cpp revision starts an internal HTTP server. It is patched to bind
127.0.0.3 inside the guest. Only the compute uid can connect there. It is not
exposed through the owner proxy or desktop network. Every native subprocess
gets an allowlisted environment; unsafe diagnostic, alternate backend and
external pad-source variables are excluded.

`llama-literal-prompt.patch` makes initial `-p` and `-f` text literal model
input. The pinned CLI otherwise interprets leading `/read`, `/glob`, `/exit`,
`/clear` and `/regen` as interactive commands even when supplied through a prompt
file. A synthetic pre-fix test confirmed a file read and unbounded output until
its test file-size limit intervened. Interactive commands remain available
when entered through the CLI's interactive input; the service supplies no stdin.

The service sets an 8 MiB hard per-file limit before starting its request
threads. Native children inherit it; a stricter inherited hard limit stays in
force. This bounds each native stdout, stderr and image file, not aggregate
disk use or pipe/socket traffic. Native failure still retires the service and
releases no result. `test_file_limit.py` starts the real Unix-socket service with
a synthetic native executable: positive controls complete, kernel EFBIG occurs
at exactly 8 MiB (and at an inherited 1 MiB limit), successful counters remain
unchanged, temporary files are removed and later jobs are refused.

`test_prompt_boundary.py` runs the packaged native Qwen binary through the real
service socket. Linux inotify watches synthetic files and directories, with
positive controls for both watch types. CPU and masked `/read` and `/glob`
prompts generate responses without opening those paths; `/exit`, `/clear` and
`/regen` also generate responses. Direct `-p` and interactive `/exit` are checked
separately. This test requires public models at `/models`, this test directory
at `/tests`, writable `/evidence` and the dedicated test worker on port 9741.
Host container evidence does not by itself prove the deployed VM boundary.

Release 0.6.8 also passed the ordinary-user `test_prompt_guest.py` inside the
attested VM: both file/directory watch controls and four CPU/masked CLI cases
passed. Normal SD/Qwen, the CUDA-source self-test, logical nvtop accounting and
Moonlight were rechecked after deployment. Receipts and terminal screenshots
are exported in `evidence/prompts-vm/`. Earlier development images retain the
initial-prompt command-dispatch defect and should not be used as a secure
prompt gateway. The masked native arithmetic and its known accuracy/performance
limitations are unchanged.

The service serializes work and rejects overlapping jobs. An engine failure,
missing verification report, timeout or integrity failure retires the service
without returning results. Recovery currently requires an administrator guest
restart, which also destroys ephemeral desktop state. There is no silent
retry through an unprotected GPU or CPU after a failed masked job. CPU reference
mode is selected explicitly before a job starts.

The 0.6.6.1 development release enforces this policy inside the native GGML backend too.
The bootstrap requires `ggml_backend_shielded_require_offload()` after installing
the public-weight verifier and before any weight registration. This one-way
policy rejects older backends and latches execution transport, resource and
pad-exhaustion failures for the process lifetime. Connection/HELLO negotiation
may retry twice, after 100 and 200 ms, before public uploads or masked work on
that connection. This accommodates asynchronous release of the old worker
reservation when SD changes graphs. A malformed version, failed upload, failed
matrix exchange or exhausted pad bank cannot use this retry. Unsupported shapes may still execute locally;
a failed attempted exchange cannot silently become a successful CPU job.
The 0.6.5 backend could silently finish SD on CPU after a worker disconnect;
the regression was reproduced before applying this fix. Do not interpret the
earlier service-level wording as proof that 0.6.5 enforced this native policy.
The first 0.6.6 candidate rejected normal SD during graph re-registration and
was not accepted as a working release; 0.6.6.1 adds the bounded negotiation path.

This mediates the supplied GPU path. Desktop Internet access remains available;
an application deliberately sending its own secrets elsewhere is outside this
device-isolation claim. Approved public weights, tensor shapes, traffic lengths,
timing and availability remain visible to the GPU operator. This is not an
access-pattern-oblivious protocol or protection against all VM side channels.

## Use

Inside the desktop:

```sh
shield-compute status
shield-compute llama --approximate --prompt 'Explain a binary tree' --tokens 128
shield-compute sd --approximate --prompt 'A watercolor cabin beside a lake' --output cabin.png
shield-compute llama --cpu --prompt 'Explain a binary tree' --tokens 128
```

The **Native Shield Compute** desktop menu opens a local UI with masked and CPU
reference modes. The owner connector also exposes `/desktop/compute/native/`.
Direct native executables live under `/opt/shield`; using the service is required
for GPU access. Native CLI flags and arbitrary model files are deliberately not
forwarded through the trusted service. New public models require an authenticated
policy and a newly measured image.

Default models are Qwen2.5 0.5B Q8_0 for text and SD 1.5 Q8_0 for images. The image
profile is currently fixed at 256×256, eight Euler steps, CFG 7. Native Shield
uses calibrated fixed-point approximations; it is not IEEE floating-point or
pixel-exact CPU equivalence. CPU reference mode is available for comparison.

Starting with the 0.6.7 development release, native CLI requests must choose `--cpu` or
`--approximate`. The native service requires `arithmetic: "shield-fixed-point-v1"`
for masked requests; missing, unknown or conflicting choices are rejected before
launch without retiring the service. CPU requests use `native-model-v1` (the
default for explicitly selected CPU mode). This means the loaded Q8 model's
native arithmetic, not equivalence to an original full-precision model.
Successful responses identify the arithmetic and model digest, and masked
responses also identify the authenticated policy and calibration digests.
The bootstrap independently requires the fixed-point contract in its controlled
environment. The exact integer graph/CUDA path does not accept this approximation
flag and retains its existing arithmetic. Explicit consent does not establish
acceptable accuracy; the numerical acceptance gate remains open.

## Build

`sources.lock.json` pins all four source revisions. `prepare.py` exports isolated
Git snapshots, applies the recorded patches and records archive/patch hashes.
It never edits the source checkouts. Build the exported Dockerfile, then run its
`build.sh` with the context at `/input` and a writable build directory at `/build`.
Use `JOBS=1` on a memory-constrained host. The result is `/build/out`.

The native build uses one GGML ABI (`GGML_MAX_NAME=128`), disables CPU weight
repacking and excludes native GPU and dealer builds. Its artifact audit rejects
CUDA/Vulkan dependencies and executable stacks. `bootstrap.cpp` authenticates
policy/calibration and installs the weight verifier before model load. Wire
names use the full SHA-256 digest encoded in base32, fitting the protocol's
63-character limit while preserving full original names for authentication.

`model-policy.py` requires a pinned whole-model digest and an explicit public
disclosure decision. Calibration must use public fixtures. Do not approve
private fine-tuned weights simply because their names match a public model.

Build the public-model disk with Enclave's `metal/volumes.mjs`. Then use
`prepare-desktop.py` with the SDK, policy directory, trusted core library, model
volume, pinned enclave services configuration and Enclave source. Build its
desktop Dockerfile and pass that image and `--shield-volume` to KVM Box's image
builder. The broker attaches the model disk read-only and checks release hashes;
the guest verifies every model block with dm-verity. The existing desktop image
and controller release can be retained for rollback.

## Validation and remaining limitations

### Broader public diagnostics (0.6.6.1 arithmetic, applicable to 0.6.7)

`evaluate_logits.cpp` links the packaged llama/GGML libraries and collects
teacher-forced logits for four authored public passages, with the same token
sequence in CPU and masked modes. `evaluate_numerics.py` compares full-vocabulary
distributions and generates four CPU/masked SD pairs. The evaluator is test-only
and absent from the guest image. These host-container tests use the dedicated
RTX 3070 worker, two CPU threads and the same authenticated Q8 models/calibration.
They are diagnostic fixtures, not a representative benchmark or a quality gate.

| Text fixture | Predicted positions | CPU perplexity | Masked perplexity | Top-1 agreement |
| --- | ---: | ---: | ---: | ---: |
| Science | 57 | 16.111 | 16.713 | 91.23% |
| Reasoning | 68 | 16.635 | 18.490 | 89.71% |
| Code | 68 | 9.618 | 9.922 | 92.65% |
| Multilingual | 67 | 21.895 | 21.781 | 97.01% |

Aggregating negative log-likelihood by predicted position gives perplexity
15.363 CPU versus 16.030 masked, a 4.34% increase. Overall top-1 agreement is
92.69%. These teacher-forced comparisons do not measure free-running answer
quality. Masked runs took 2.39–2.55 seconds versus 0.50–0.60 seconds CPU.

| Image fixture | Seed | Pixel MAE / 255 | PSNR dB |
| --- | ---: | ---: | ---: |
| Portrait | 17 | 13.026 | 20.156 |
| Geometry | 29 | 3.980 | 27.599 |
| Landscape | 53 | 10.088 | 22.326 |
| Typography | 71 | 5.062 | 24.296 |

All image pairs differ. Visual inspection shows changed face/clothing and
mountain/reflection details; both CPU and masked geometry images fail to depict
the requested cube and sphere, and neither typography image reliably spells
HELLO. Pixel similarity alone does not establish prompt adherence or quality.
Each masked SD job completed 320 offloads, 1,136 verified exchanges and zero
verification failures. Masked SD took 28.22–28.72 seconds versus 23.21–23.65 CPU;
there is no measured speedup in these fixtures. The full receipts, PNG pairs,
source hashes and container/SDK provenance are exported under
`evidence/native-numerics/` with `native-numerical-validation.json`.

The 0.6.7 SDK changes only the bootstrap contract gate and provenance manifests;
engines, arithmetic backend and calibrated model artifacts are unchanged.
Explicit approximation selection makes the behavior visible but does not close
the numerical or performance acceptance requirements.

### Deployed allocator exhaustion fixture

The separate `release-v6.7-exhaustion-test` image uses the real allocator with a
fixed capacity of 16 instead of 2^40. It is a test artifact, not a normal release.
After fresh attestation and a successful CPU control, native SD reached the
actual capacity, propagated error -11 through GGML and retired the service.
No result or successful GPU counters were released, and subsequent masked and
CPU requests were rejected. Guest service diagnostics and `nvtop -s` confirmed
the origin and retired state. The unchanged 0.6.7 image was then restored and
passed fresh attestations and positive native workloads; Moonlight was restored.
See `../FAULT_TESTING.md` for the reproducible fixture and exact limitations,
and `../IMPLEMENTATION.md` for the deployed receipts. Production capacity and
normal native artifacts remain unchanged.

### Earlier smoke evidence

Host native smoke tests generated the same short Qwen answer in masked and CPU
modes. The masked run recorded 726 offloaded matrix operations and zero
verification failures. This is a small functional check, not an accuracy suite.

The SD host smoke generated a 256×256 image with 32 remotely executed matrix
operations, 288 local backend operations and zero verification failures.
Compared with the same-seed CPU reference, pixel MAE was 12.36/255 and PSNR
22.71 dB. The single image differed visibly. Generation took 86.76 seconds in
masked mode versus 23.66 seconds on CPU on that test setup. This profile is not
a demonstrated SD speedup. These measurements do not replace VM-level tests or
a broader model quality evaluation.

Twenty-four compiler/service tests passed, including policy substitution,
environment injection, unknown GPU requests, concurrent admission, fail-closed
retirement, private-constant CPU execution and framing limits. Native bootstrap
tests rejected wrong policy/calibration digests and a substituted backend.
Guest boot checks exercise raw-device absence, VSOCK/io_uring denial, privilege
restrictions, direct-worker isolation and local service availability. Record
the measured guest's end-to-end results separately; host containers do not
provide confidential-VM evidence.

### Bounded SD exchanges

The service now sets `SHIELDED_WIDE=1` for masked SD only. The native C link
splits image-token batches into at most 32 rows per exchange. Every chunk uses
fresh pads and has its reply verified before the backend reconstructs the
GGML output. A failed verification aborts the graph and retires the backend;
the service returns no image and refuses subsequent work. This is the existing
Shield protocol, with no new cryptographic construction or plaintext route.
The authenticated weight-copy and fixed-point arithmetic rules are unchanged.
LLM batching and explicitly selected CPU reference mode are unchanged.

A matched host-container test (256×256, eight Euler steps, CFG 7, seed 42,
two CPU threads, public watercolor-cabin fixture) took 87.12 seconds with the
old narrow profile, 29.67 seconds with chunking, and 24.46 seconds on CPU.
The two masked images were pixel-identical. Both differed from CPU by MAE
5.75/255 and PSNR 29.24 dB. Backend offloaded/local operation counts changed
from 32/288 to 317/3, with zero verification failures. These counts describe
GGML operations, not the larger number of wire chunks. This one fixture is
neither a general speedup claim nor a model quality acceptance suite.

Before 0.6.4, the native `macs` statistic included local graph products, and the
service's `maskedMacs` field overstated GPU work. See the accounting correction
below; historical receipts retain their original fields and interpretation.

`test_wide.py` compiles `test_wide.cpp` against the pinned SDK and runs it
against the backend in the selected desktop image. Five positive cases cover
33, 64, 65, 77 and 256 rows, grouped weights, exact known outputs, and partial
final chunks. Three negative cases corrupt the first, middle and final replies
of a 65-row graph; they require untouched output tensors, failure counters,
retirement, and no additional wire request on a repeated graph. With `--models`,
the harness also injects corruption into the second chunk of a real SD service
job and requires retirement without an image or completed-job increment.

Run from a host with the dedicated test worker on loopback port 9741:

```sh
python3 shield-compute/native/test_wide.py \
  --image enclave-kvm-box-desktop:0.6.3-wide \
  --builder enclave-kvm-shield-native-builder:1 \
  --sdk "$NATIVE_BUILD/out" \
  --headers "$NATIVE_BUILD/src/shield/wasm/ggml-shielded" \
  --models "$PUBLIC_MODELS" --output "$NEW_EVIDENCE_DIRECTORY"
```

`benchmark_sd.py` reproduces CPU, old narrow, and current wide runs using the
image's actual service environment and model authentication. Mount this native
directory at `/tests`, the public models read-only at `/models`, and a writable
results directory at `/results`. Run each mode sequentially in the desktop OCI
image with `--network=host --memory=8g --cpus=2` and command
`python3 /tests/benchmark_sd.py cpu` (then `narrow`, then `wide`). Each mode
requires a new output subdirectory. Compare decoded RGB pixels, not PNG file
hashes, because image metadata contains different generation parameters and
paths. These host tests provide no confidential-VM or display-security claim.

### Verified work accounting (0.6.4)

The SDK adds `sh_link_verified_stats` and
`ggml_backend_shielded_verified_stats`. Their counters advance only after all
rows and members of a wire reply have passed verification. They exclude local
products, pad preparation, cached graph results, rejected replies and the C
link's unverified diagnostic mode. Existing diagnostic APIs retain their old
meaning. Counter state survives backend quiescence; it is not reset at graph
boundaries. Earlier verified chunks remain counted by the SDK if a later chunk
fails, but the service commits telemetry only after the entire job succeeds.

Native responses now distinguish:

| Field | Meaning |
| --- | --- |
| `offloaded`, `local` | Backend graph-operation counts |
| `graph_macs` | Shield-backend graph-product work, including local execution |
| `exchanges` | Completed, verified wire exchanges, including wide-batch chunks |
| `macs` | Logical field multiply-accumulates in those verified exchanges |
| `verification_failures` | Native backend verification failures |

The service's cumulative `verifiedOffloads` and `maskedMacs` use `exchanges`
and `macs` from successful jobs. They are not hardware instructions, floating
point FLOPs, physical GPU occupancy, or utilization percentages. No nvtop/NVML
device is implemented by this change. The new bootstrap refuses a backend
without the verified-counter API; the service refuses the old ambiguous report
format rather than silently treating its graph counts as GPU counts.

The native fixture suite also tests mixed local/remote graphs, corruption in
the second member of a grouped reply, zero verified counts in unverified
diagnostic mode, and retention after teardown. In the mixed fixture, 198,656
graph MACs correspond to only 65,536 verified GPU MACs. The corrected host SD
fixture returned identical pixels to 0.6.3 and reported 316 offloaded / 4 local
operations, 1,116 verified exchanges, 178,589,270,016 verified MACs and
181,525,282,816 graph MACs. Per-run local fallback counts can differ.

`shield-verified-counters.patch` is applied to the pinned Shield source by
`prepare.py`; its digest and build-recipe digests are recorded. The tested
incremental SDK rebuilds the native backend and policy bootstrap, preserving
the other pinned engine binaries byte-for-byte and regenerating the artifact
manifest. The separate OpenCL service core is unchanged.

### Confidential VM tests (2026-10-23)

Both the controller and desktop passed the SNP attestation gate for release
`release-v4-fixed`. In that desktop, the masked Qwen smoke returned
“The capital of France is Paris,” recording 726 offloads and zero verification
failures (4.49 seconds end to end; CPU reference 0.82 seconds). SD recorded
32 offloads, 288 local backend operations and zero verification failures
(99.73 seconds end to end; CPU reference 31.27 seconds). These short functional
checks do not establish speedup, broad accuracy or general CUDA compatibility.

A separate interactive xterm run inside the desktop also executed
`/opt/shield/bin/llama-cli --version` (Linux x86-64) and
`shield-compute llama -p "The capital of France is" --tokens 16 --seed 7`.
It returned the same answer and 726 offloads with zero verification failures.
`shield-compute` is our local Unix-socket CLI client; it is not an upstream
CUDA CLI. The measured service launches the native upstream-derived binaries.

The guest currently exposes no GPU device, CUDA driver or NVML interface.
Consequently it does not appear in nvtop. Standard compute API compatibility
and monitoring integration are still unfinished. Native execution and verified
offload must not be presented as a completed general virtual GPU.

### Native UI availability regression

The initial UI checked availability only at page load. Opening it during a job
left Run disabled after the job finished. The UI now checks every two seconds,
aborts a stalled status request after five seconds, and retries connection
failures. It preserves prompts/results, cancels stale status requests when
submitting, and requires a fresh ready response after completion. Retired
services stay disabled and explain that a restart is required.

Run `node --test tests/test_native_ui.mjs` from the kvm-box directory. Six
regressions cover busy recovery, HTTP/network recovery, stale-response races,
job completion, retirement, timeout and both guest/connector URL prefixes.

See `STANDARD_NUMERICS.md` for the installed standard OpenCL numerical
comparisons, Claude-assisted performance investigation, and candidate
float-validation optimization.

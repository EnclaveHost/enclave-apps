# Standard OpenCL Stable Diffusion through Shield

Status: experimental host-container integration, 2026-10-24. Stable Diffusion
now uses the standard GGML OpenCL backend for supported Q8 matmul and native
GGML CPU for other operations. Only the existing authenticated, masked and
verified Shield protocol reaches the physical GPU. The host results below precede the 0.6.9 candidate deployment. See
`STANDARD_PACKAGE.md` for the installed image and confidential VM checks.

## Placement and correctness

Set `GGML_SHIELD_NATIVE_CPU_FALLBACK=1` to restrict the known logical Shield
OpenCL device to contiguous, nonbatched Q8_0/F32 matmul with K*N >= 2,000,000;
MV additionally requires N divisible by eight. This eligibility check only
selects an implementation: the frontend/service must still authenticate actual
public tensor bytes and enforce the explicit `shield-fixed-point-v1` contract.
Unknown weights or unsupported operations remain on trusted CPU before any
protected transaction. Failure after protected execution never retries on CPU.

SD previously assumed one backend could execute its entire graph. The patches
activate its existing scheduler and CPU fallback, disable segmented execution
when that scheduler is active, and retain the multi-device callback behavior.
Temporary in-place graph operations now permit scheduler-selected storage;
persistent inputs and weights preserve their mutation semantics. The scoped
helper only affects the active graph context. Attention construction considers
both OpenCL and CPU capabilities so fallback preserves the original flash
attention graph. Weight uploads serialize shared OpenCL kernel argument use.

Q8 SoA buffer readback now uses the parent tensor's layout and adds a view's
accumulated byte offset. Nested, partial and strided views copy correct bytes
into CPU buffers. The Adreno transpose branch is repaired by source inspection
but not runtime-tested; the analogous Q4 path is outside this change.

For llama.cpp's same restricted profile, use `--no-kv-offload`. Its persistent
KV cache must reside on CPU because SET_ROWS is intentionally unsupported by
the restricted device. Weight matmul still uses the logical OpenCL device.
The probe adds this flag only when its native CPU fallback option is requested.

## Executed evidence

Public fixture: SD 1.5 Q8, watercolor cabin prompt, 256x256, two Euler steps,
CFG 7, seed 42, two CPU threads and flash attention enabled. Single-run times
include startup and are diagnostic, not a performance benchmark.

| Route | Seconds | Result |
| --- | ---: | --- |
| Original CPU | 10.679 | Reference image |
| Modified CPU graph control | 10.881 | Byte-identical to original CPU |
| OpenCL storage, all compute on CPU | 22.852 | Byte-identical to original CPU |
| Direct masked Shield SDK | 13.389 | 392 verified exchanges, 51,921,289,216 MACs |
| Standard OpenCL masked route | 38.405 | Same 392 exchanges/MACs; image byte-identical to direct SDK |

The shared masked image SHA-256 is
`048b2445a2422e1fca0c8c8272557e6ab829dbcc74cfa818edf8e0c61d7abf80`.
Compared with original CPU, its pixel MAE is 7.9045/255 and PSNR 26.2958 dB.
That difference is the explicitly opted-in approximate Shield arithmetic, and
the exact SDK match establishes agreement on this fixture only. It does not
establish broader perceptual quality or original floating-point equivalence.
No end-to-end speedup is demonstrated.

The final candidate rejects both a corrupted worker reply and a disconnected
worker: the process fails, no image is published, the service retires, and a
second attempt is refused. Neither test records completed GPU work. The lower
level MM/MV tests separately verify untouched output sentinels on failure.

All-layer Qwen with CPU KV placement generates the tested Paris response in
37.990 seconds, with 511 verified exchanges and 14,478,327,808 GPU MACs. This
is a short compatibility check, not model-quality acceptance.

Other checks: 34 service/protocol/admission tests pass; the mixed CPU/OpenCL
scheduler fixture produces 4,096 exact outputs; 17 Q8 view/read/copy checks pass
(the baseline fails 13). Applying all patches reproduces the candidate source
byte-for-byte across 29 files. The view test preceded only the later matmul
eligibility size change; its implementation is unchanged.

## Reproduction and limits

Apply SD patches to the pinned exported SD tree in this order:

1. `sd-opencl-fallback.patch`
2. `sd-opencl-graph-operations.patch`
3. `sd-opencl-upload.patch`
4. `sd-graph-diagnostic.patch`
5. `sd-opencl-capabilities.patch`

Apply `llama-shield-opencl.patch`, then `llama-shield-cpu-fallback.patch`, then
`llama-opencl-q8-views.patch` to the pinned GGML export. Keep the existing
PoCL repairs and masked frontend. The diagnostic SD environment option is
only for comparing CPU graph construction; default CPU behavior is unchanged.

Final runtime digests:

- SD library: `bf223acc83eb6a54bb8cba59bbfbb740fe4762a36b4a53cccb5eb1a206125c80`
- OpenCL GGML plugin: `0b3e7d029cfb3f10aab5c7fe1b31e4adde5f5ae842eaa1561a1ab9707c73aee4`
- Frontend: `8394127f5c9e0a88cb7195d030dd98123bf1352fc499f8298ce8b870ab683219`
- PoCL: `90e4f6a7bbbe32d6170ca9575f3ad2630d31cd21fe3427e63dcb0faf881bb2ee`

Exact container commands, public probe receipts, patch snapshots and test logs
are saved with the task's `standard-sd-masked-progress.json` and SHA-256 manifest.
Claude proposed the Q8 view repair/test; Codex reviewed, amended, built and
executed it. Claude also reviewed the scheduler/service boundary and the llama
placement issue. Review proposals alone are not execution evidence.

Remaining work includes broader numerical/quality testing, API coverage and
performance, reproducible image packaging, actual confidential VM validation,
production identity binding, and publication/wallet/nucbox acceptance. Full
CUDA binary compatibility and masked general graphics are not implemented.
Compute menu removal remains gated on full implementation.

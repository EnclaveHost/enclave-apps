# Standard OpenCL Q8 execution through Enclave Shield

Status, 2026-10-24: the experimental frontend now executes the two pinned Q8
matrix kernels through the guest-local service and the existing native Shield
backend. Real Qwen, Stable Diffusion and worker-failure probes pass in host containers.
See [SD integration](../native/STANDARD_SD_MASKED.md) for the latest shared runtime. The separately packaged 0.6.9 candidate now passes ordinary-user CLI checks
inside the confidential VM; see [package evidence](../native/STANDARD_PACKAGE.md).
Broad model quality and performance acceptance remain open.

## Execution and arithmetic

Build the frontend with `SHIELD_Q8_CAPTURE` and `SHIELD_Q8_MASKED`. The application
must explicitly set `SHIELD_OPENCL_ARITHMETIC=shield-fixed-point-v1`; without it,
the capture build executes original CPU kernels. The service must separately
have `enableQ8Experimental: true` in its administrator-controlled configuration.
A requested route with that service feature disabled fails explicitly.

The typed compiler recognizes the original MM/MV kernels. Real runtime scalar
widths, storage bounds, aliases, contiguous strides, batch factors and grids are
validated before queue insertion. After dependencies complete, the frontend
snapshots codes, scales and private activations and authenticates the original
public Q8 tensor bytes. Unknown weights and unsupported layouts remain on CPU.

For an approved tensor, the frontend submits its reference and activation
snapshot to `/run/shield/compute.sock`. The service accepts no client paths,
worker addresses, libraries or policy overrides. It checks the configured model
reference and loads the model's own immutable, approved tensor bytes. The native
executor checks their metadata and digest again; the existing bootstrap also
verifies the backend's private registration copy. The worker sees only public
weights and the existing Shield protocol's masked payloads.

The service reuses a native executor for each engine, replacing it when switching
engines or before its 100,000-request cap. Failed requests are never retried as
part of rollover. Requests are at most 32 rows, 2 MiB of float32 input and 2 MiB
of output. Larger frontend operations are chunked. Every chunk must succeed
before any application output is written. A decline may select CPU only before
the first chunk executes; later declines and all protected failures fail the
OpenCL event without CPU retry. Corruption/transport failures retire the service.

`shield-fixed-point-v1` is the existing approximate native arithmetic contract.
It is not general IEEE float equivalence. Its authenticated calibration, weight
encoding, private outlier handling, masking and verification are reused rather
than reimplemented. Unsupported operations execute their original CPU kernels.
An eligible service operation can also reuse an already verified group result;
zero *new* exchanges must not be misreported as fresh GPU work or necessarily as
CPU arithmetic. Trace label `shield-service-no-new-exchange` makes that distinction.

## Queue and resource behavior

The first integration retained the audit prototype's 16-job/512 MiB reservation
fallback. Under real asynchronous Qwen execution that changed eligible operations
to original CPU arithmetic based on queue pressure. Those runs are historical,
not acceptance evidence for deterministic placement or comparative performance.

The final implementation retains up to 4,096 pending jobs, with kernel clones,
events and buffer references. An ICD callback schedules a job only once its
dependencies finish. One process-lifetime worker captures and executes ready
jobs, bounding aggregate snapshot/result payload storage to 512 MiB without
reserving that memory for jobs waiting on future user events. Queue overflow
returns an explicit resource error. It does not select another arithmetic path.
The enqueue thread does not wait for a user event it may itself need to signal.

A bounded cache stores only layout validation keyed by all freshly queried
allocation identities/ranges, scalar values, grids and offsets. It never caches
private inputs, weight authentication, output values or permission to disclose
weights. Every launch still snapshots and authenticates its actual contents.

## Executed evidence

- 34 service/protocol/admission regressions pass, including explicit arithmetic, model and
  policy binding, malformed requests/results, non-finite values, truncated output,
  timeouts, planned executor rollover and the original packed-weight size limit.
- 34 compiler/admission/frontend regressions pass. The original 12 MM/MV fixture
  cases still produce 18,694 exact dyadic outputs with guards and verified snapshots.
- Five capture failure/lifetime/order/pressure cases pass, with all 45 expected
  snapshots. The pressure case enqueues 40 jobs behind an unopened user event;
  all 40 are captured and execute correctly. Audit-log failure also preserves output.
- Actual OpenCL MM and MV kernels each produce 14,592 nonzero-reference outputs
  matching a separate native fixed-point execution, with intact guards. Together
  they record two verified exchanges and 26,148,864 GPU multiply-accumulates.
- Corrupting a real worker reply or disconnecting the worker fails the OpenCL
  command, preserves the complete sentinel output, retires the service and refuses
  a second attempt. Neither case reports successful work or falls back to CPU.
- Final all-layer Qwen returns the tested Paris response in 44.049 seconds:
  1,686 captures (1,521 MV, 165 MM), all authenticated; 1,183 layout-cache hits;
  726 supported service operations, including 511 new verified exchanges and
  14,478,327,808 GPU multiply-accumulates. Another 215 service operations issue no
  new exchange, and 960 operations take the original CPU route before offload.

These are host-container tests using the dedicated loopback Shield worker.
No raw GPU device was passed to the client/container. They are not evidence of
confidential-VM deployment or production identity binding. The single short
prompt is not numerical quality acceptance. Earlier CPU execution was faster;
no end-to-end speedup is claimed. The layout-cache-only intermediate run was
faster partly because queue pressure skipped captures; it is retained as a
superseded diagnostic, not a performance result.

## Review and reproduction

Claude Code wrote the initial persistent C++ executor and reviewed the integrated
boundary. Codex reviewed, compiled, integrated and tested it. Review fixes include
planned request-cap rollover, matching packed-weight limits, tighter accounting
checks and an explicit result-extent check. The existing Python layout validator
already rejects the batched/strided/offset cases raised conditionally by review.
The pinned backend supports all admitted chunk widths from 1 to 32; later refusal
still fails closed. Its verified group cache legitimately permits an offloaded
node with no new exchange and can precompute extra group members, so the proposed
rule equating every graph node with one fresh exchange was not adopted.

Saved build/probe scripts, exact container commands, source snapshots, runtime
hashes and public test receipts accompany `q8-masked-progress.json`. The final
frontend SHA-256 is
`8394127f5c9e0a88cb7195d030dd98123bf1352fc499f8298ce8b870ab683219`
after the bounded pre-connect admission retry fix. The earlier evidence bundle
retains its original frontend digest.
The native executor uses the installed bootstrap's whitelist and policy checks.
The public fixture compares the exact same arithmetic profile on both paths;
it does not compare approximate output to an exact-float oracle.

Next work: broader numerical and performance
acceptance, additional API coverage, packaging and actual confidential
VM tests. Protected graphics, production binding, publication assets,
wallet/nucbox rollout and final compute-menu removal remain incomplete.

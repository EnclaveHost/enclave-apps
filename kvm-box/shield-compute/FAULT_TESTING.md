# Deployed worker fault checks

Use only a dedicated development worker and synthetic public fixtures. The
worker must run on loopback port 9742; `test_worker_proxy.py` listens on loopback
9741 and forwards the existing Shield wire protocol. Never put this proxy in
front of production workers or change a production worker's port.

The tool is a deliberately untrusted transport fixture. It has no masks, keys,
plaintext inputs or verification state. Reports contain command IDs, counters
and SHA-256 fingerprints of masked request bodies, not packet contents.

```sh
python3 test_worker_proxy.py --mode replay --report /private-work/replay.json --seconds 300
```

Modes are `pass`, `replay`, `corrupt`, and `disconnect`. Replay forwards the first
matrix reply and substitutes it for a later reply of the same size. Corrupt
flips one bit in every successful nonempty matrix reply. Disconnect closes the
connection before forwarding the first matrix command. The proxy is bounded
by its requested lifetime (at most 1,800 seconds), with 15-second socket
timeouts and a 16 MiB frame bound. Restore the original dedicated worker after
each session, including failures; do not leave an injection proxy running.

## Guest assertions

Use the measured v6.1 image and its `shield-cuda-selftest` executable, compiled
from `hip/test_cuda.cu`. Run the normal test first for replay, then run:

```sh
CHIP_LOGLEVEL=crit shield-cuda-selftest failure
```

The failure test requires a visible CUDA synchronization error, checks that the
output sentinel is unchanged, destroys an independent healthy stream, disposes
the failed stream while observing its error, and uses a fresh stream for exact
CPU memory operations. It does not clear the protected service's retired state.

For replay, require two distinct masked request fingerprints, exactly one
substituted reply, and only the initial job counted as completed. Repeat the
failure test while retired: the proxy must see no additional connection or
matrix command. For corruption, require one modified reply and retirement.
For disconnect, require one dropped matrix request, no matrix reply, and
retirement. Every case must preserve output and expose the runtime error.

Confirm the original worker is listening again before restarting the VM.
Desktop stop is asynchronous: wait until its status is `stopped` before issuing
start. Inspect the new SNP verification gate, wait for the service to become
ready, then run a successful exact CUDA test and inspect its verified-offload
counter. Re-pair Moonlight after the new ephemeral desktop identity is created.

## Scope

These cases exercise a real worker and the deployed confidential guest boundary,
including native runtime error propagation. They do not prove all corruption
patterns, all concurrency schedules, pad exhaustion, timing/shape secrecy or
general CUDA compatibility. Attestation handshake replay checks are separate
from GPU matrix-reply replay checks; do not substitute one for the other.

## Native strict-policy host tests

First run `native/test_wide.py` against the candidate SDK/image. Then run
`native/test_strict.py --image IMAGE --builder BUILDER --headers PREPARED_HEADERS
--wide WIDE_TEST_OUTPUT --models PUBLIC_MODELS --legacy-sdk PREVIOUS_SDK
--output NEW_OUTPUT`. The headers directory must contain the production C objects
from that SDK build. Use only the dedicated worker on loopback 9741.

The tests compile fixtures in a bounded, network-disabled builder. They check
startup refusal of the previous backend; unavailable-worker and first/middle/
final chunk disconnects; unchanged output sentinels and rejection of repeated
graphs; and service retirement with no result or successful-job counters using
the real native SD engine. The pre-fix 0.6.5 image returned success after a first
exchange disconnect, demonstrating the regression these checks prevent.

`test_maskbank.c` includes the actual production C source, advances its internal
counter to eight windows before its real 2^40 capacity, and races 16 issuers.
Exactly eight succeed and eight report exhaustion, the counter does not wrap,
rejected outputs remain untouched, and later issuance stays rejected. It does
not consume 2^40 windows or prove a full deployed session of that length.
`test_exhaust_shim.c` separately injects the same exhaustion error after one real
backend call to test propagation through GGML and the real SD service. This is
explicitly synthetic failure injection, not deployed pad-exhaustion evidence.
Neither fixture nor any configurable pad limit is packaged in the desktop.

`native/test_negotiation.py --image IMAGE --wide WIDE_TEST_OUTPUT --output NEW_OUTPUT`
tests the connection/HELLO exception: two refusals followed by a fully verified
job, retirement after the third refusal, immediate rejection of an incompatible
protocol version, and immediate retirement after an upload disconnect. Refused
connections must carry only HELLO; retries cannot send masked work. The actual
full SD workload must also pass `native/test_positive_service.py` inside the
candidate container, with public models mounted at `/models` and writable
evidence at `/evidence`. Two consecutive fixed-seed jobs must produce identical
images and the expected 320 eligible offloads, 1,136 verified exchanges and
181,525,282,816 verified field MACs each. Narrow synthetic success tests alone
missed the reservation-release race between SD graph registrations.

For the deployed native path, verify the expected guest attestation first and
confirm the service is idle. Arrange the dedicated worker's unavailability or
the disconnect proxy, then run `test_vm_native_failure.py --fault unavailable
--output /private-work/native-unavailable.json` (use `--fault disconnect` for
the proxy case). The script sends a fixed public SD prompt through the owner
connector, requires a service error with no image/text/outputs/stats, observes
retirement, and requires a repeated request to be rejected with unchanged
successful-job counters. Application errors use HTTP 200 with `ok: false`;
HTTP status alone is not evidence of job success. For disconnect also require
exactly one proxy connection and one dropped matrix request. Restore the
dedicated worker in a finally block, restart the desktop, verify its fresh
attestation, and run positive native and CUDA checks. Never run this test
against a production worker or use a fault script as the deployed GPU service.

## Non-destructive deployed admission checks

After separately verifying attestation and confirming the development instance
is idle, run `test_vm_admission.py --output /private-work/admission.json`. It
uses the loopback owner connector, one public SD job, eight simultaneous native
contenders, 18 invalid requests, and a later exact local graph. Require unchanged
counters after rejected inputs, only the admitted job counted during contention,
exact verified native totals, and successful later work without a restart. This
requires no fault proxy and performs no service restart. It does not test thread
schedules inside OpenCL/HIP or pad-pool exhaustion.

## Real allocator exhaustion in a separate measured fixture

`native/prepare_exhaustion_fixture.py --source PREPARED_GGML_DIRECTORY
--service service.py --output NEW_FIXTURE` creates an isolated test snapshot.
It changes the mask bank's fixed capacity from 2^40 to 16, adds a diagnostic
at the actual exhausted-counter branch, and adds a service witness that forwards
only two fixed messages from native stderr. The key generator, monotonic counter,
locking, pad generation, verification and `SH_ERR_EXHAUST` return path are
unchanged. No error code is injected and the counter starts at zero. This script
is deliberately absent from the normal native build recipe.

Rebuild `shielded-tee.o` and the backend shared library with the pinned builder
and existing production objects. Package a distinctly labeled test-only OCI
image containing that backend and instrumented service, with updated SDK
provenance/file hashes. Preserve native engines, bootstrap, policies and models
byte-for-byte. First run `native/test_allocator_exhaustion.py` in the test OCI
image against the dedicated worker on 9741. Require observation of the actual
16-window boundary, strict backend retirement with error -11, no result or
successful-job counters, and refusal of subsequent masked and CPU requests.

Build a separate measured desktop from that OCI image. After verifying its
expected controller and desktop SNP attestations, run
`test_vm_native_failure.py --fault allocator-exhaustion --output RECEIPT`.
That script proves service failure/retirement, not the fault's origin. Inspect
the guest's `/run/shield/service.log` for both exact witnesses:

```text
[test-only] actual maskbank exhausted: issued=16 capacity=16
[shielded] protected offload failed (-11); backend retired
```

Record the measured fixture identity, source diff, unchanged engine/bootstrap
hashes, owner-API results and guest diagnostic evidence together. Restore the
unchanged regular image in a `finally` path, verify fresh attestations and
successful native work, and restore Moonlight. The test image is unsuitable for
normal use and must never be published as a functioning application release.

This reduced-capacity deployment exercises real pad allocation and error
propagation through the VM boundary. Combined with the separate production
2^40 counter-boundary/concurrent-issuer test, it is stronger than a synthetic
error shim. It still does not represent a full 2^40-window deployed session,
every pad-pool scheduling race, or all GPU API paths.

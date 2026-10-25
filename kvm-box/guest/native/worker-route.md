# Native worker route — prepared, not deployed

The native masking service uses `unix:/run/shield-worker/gpu0`. The C transport
now requires connected `SO_PEERCRED` UID 0 before returning a usable pipe or
sending protocol data. Socket file ownership is insufficient: a non-root
listener remains rejected even when a privileged fixture changes its socket
file to root ownership. There is no peer-UID environment override or fallback
from a rejected Unix connection to TCP.

UID 0 identifies the measured local broker in the guest user namespace. It does
not authenticate the remote worker or prove attestation. The privileged
platform must reserve/authenticate the worker before publishing that socket,
admit only masking-service UID 1001, and enforce the native network/role policy.
The compiler, public-weight approval and masking/verification boundaries remain
mandatory. Desktop programs still must not receive a raw worker or GPU route.

## Two existing library versions

The deployed SDK contains a standalone trusted core and a separately compiled
ggml backend. They have different source baselines. The backend already had
Unix transport; the standalone core did not. Replacing either with an unrelated
newer core would also change numerical code, so both were patched against their
own recorded baseline:

- `shield-compute/native/shield-unix-peer.patch` adds connected-peer validation
  to the backend's existing Unix transport. `native/prepare.py` includes it in
  the full SDK source recipe. The staged incremental backend preserves all 12
  non-transport object files byte for byte.
- `shield-compute/native/shield-core-unix-peer.patch` adds the same Unix route
  and root-peer check to the deployed standalone core's older transport file.
  `build-core.py --native-worker-route` applies it to a fresh source copy and
  records both original and compiled source hashes. Only shielded-wire.c differs
  from that core's original source manifest. The source directory is not edited.

The separate staged SDK was compared against the exact existing OCI image.
Its new complete manifest covers 417 files; 414 retain their bytes. The changed
files are the two libraries and source-provenance.json, plus the manifest itself.
No running service, published image or model approval was changed.

## Service configuration

`build_service_config.py` checks the legacy service fields, fixed model paths,
policy digests and calibration digests, then changes exactly worker and
clientUids. The new file is installed as `/shield/native-service.json`:

- worker: Unix path above, port field 19595 (ignored by the Unix transport).
- clients: `[1000, 1003]`; UID 0 is not a client authorization shortcut.

Threads, deadlines, model/reservation settings, public constants and the
experimental Q8 flag retain their source values. The builder creates a fresh
output only and rejects duplicate/unknown fields, changed input routes, unsafe
paths and mismatched policy/calibration bytes. Its receipt explicitly does not
claim authenticated storage, reservation or runtime admission.

## Evidence and remaining work

Six real Unix credential test groups pass for each selected library, including
three non-root impostors with root-owned socket files and 100 repeated refusals
without a descriptor leak. Both staged libraries load inside the existing
Debian guest image as UID 1001. The standalone core matches the deployed core on
317440 finite weight encodings, 65536 float-to-half conversions and 12 synthetic
weight matrices. The standalone postmortem self-test also passes there. Eight
configuration build test groups pass. These checks perform no GPU work and do
not establish full model quality or performance.

The ggml backend expects the engine's global libggml symbols. Its test harness
loads that provider explicitly; loading the plugin alone is not a valid engine
environment. Full nsinit, authenticated model/worker resources, runtime admission,
native workload acceptance, Moonlight/NucBox and publication remain open.

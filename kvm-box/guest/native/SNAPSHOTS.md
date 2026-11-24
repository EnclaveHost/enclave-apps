# Native resumable snapshots: platform dependency

Status: **not implemented / not qualified**. No native snapshot or restore
endpoint is enabled by the private-snapshot encryption change. A home backup
does not preserve CPU registers, RAM or device state and must not be labelled
a resumable VM snapshot.

The native desktop runs as a namespace child inside the outer measured
Hyper-V domain. The app does not own a KVM VM file descriptor or a virtual CPU
serializer. `supervisor.c` implements Q/B/H (status/begin/halt), and halt sends
SIGTERM then SIGKILL. `gateway/gateway.go` only exposes status/start/stop for
lifecycle; the Wasm `shield-desktop.rs` controller forwards those operations.

The outer runtime's `windows/vbslike/manager/backend.mjs` and `server.mjs`
likewise implement start/stop/survey/teardown, without a checkpoint route.
The liveness sweep reclaims non-Running partitions. `wmi-launcher.mjs` stop
removes the VM and closes worker, egress, custody and package transports.
Existing `shield-secrets.mjs` release excludes recovered inventory and binds
fresh nonce, app identity and transport key to the current TLS key. Worker
transport credentials and control streams are per-start.

These boundaries were checked against native app source
`7f2f7063fbd23efc819c9179f0add5b32555a681` and platform source
`82ea9c16f56b608f3819e6a0e63b56027c789dd9`; the relevant files in the newer
coordination checkouts had no changes. Running overlay provenance is separate
and must be checked before deploying anything.

Confirmed platform restriction (2026-10-27): the pinned OpenHCL source
`db3c09ed5752703ff7ee6493c7e6761f719f82f9`,
`openhcl/underhill_core/src/worker.rs`, calls
`validate_isolated_configuration` for isolated guests (lines 2220–2233) and
rejects `hibernation_enabled` with “hibernation is not supported” (4084–4085).
The pinned OpenHCL archive SHA-256 is
`71a91101f8f9b870199946076db2b220c68167e029539d4619a6b7600432124e`,
matching the native package/build input pins. This establishes source and
artifact-input provenance; it is not a fresh binary rebuild. Host `Save-VM`
is also refused for this VM type, as recorded in VBS-ISOLATION.md.
Kernel CONFIG_HIBERNATION / CONFIG_SNAPSHOT_DEV being enabled does not
establish support at the isolated platform boundary. Do not bypass that
validation or replay an old attestation/transport lease to claim resume.

This restriction applies to host-advertised/managed hibernation. It does not
prove guest-driven Linux checkpointing is impossible. In the pinned kernel,
`kernel/power/user.c` initializes userspace snapshot `platform_support=false`;
CREATE_IMAGE / ATOMIC_RESTORE call the Linux snapshot paths without ACPI
platform callbacks. `hibernation_available()` checks nohibernate, lockdown,
secretmem and CXL state, not ACPI S4. Hyper-V syscore and VMBus freeze/restore
callbacks remain active without that S4 condition. This is an **unqualified
candidate**, not a supported native resume implementation, and may preserve
the current isolation platform.

The custom `drivers/char/tpm/enclave_openhcl_tpm.c` driver currently registers
its two DMA32 control pages with OpenHCL only at module initialization. It has
no PM freeze/restore callbacks. A restored kernel may contain different page
addresses from a fresh boot, so safe quiescence and mapping re-registration
must be implemented and tested before fresh attestation can be trusted.
OpenHCL TPM/custody state outside the Linux guest is not in its checkpoint.
The ordinary native desktop has neither /dev/snapshot nor CAP_SYS_ADMIN;
checkpoint access must remain in a narrowly authenticated platform helper.
Opening /dev/snapshot itself invokes PM prepare notifiers and acquires global
hibernation state; even an open-only probe belongs in a disposable guest.

Required implementation and acceptance work:

1. Establish a supported confidential checkpoint/export/import primitive for
   the measured outer VM. Qualify on a disposable guest with the exact IGVM,
   VM type and kernel. Host-readable RAM exported and encrypted afterward
   does not establish guest-owned private snapshot protection. Existence of
   generic Windows Save-VM is not compatibility evidence.
2. Add explicit preparing/suspended/restoring states to manager liveness and
   cleanup; serialize concurrent operations and preserve the old checkpoint
   on failure. Bind restore to the owner, deployment, image and format.
3. Quiesce desktop/compute and capture CPU, RAM, writable disks and devices
   consistently. Either reconstruct external GPU resources or reject saves
   while unsupported resources exist. Do not silently replace continuation
   with restarting applications.
4. Encrypt checkpoint bytes inside the authorized trust boundary, before
   ordinary S3 storage. `snapshot-crypto` supplies a bounded-memory envelope,
   not the capture primitive. Define key bootstrap separately: a full outer
   snapshot can contain the controller and its keys as well as desktop state.
5. Resume through fresh admission, anti-clone/rollback epoch, attestation and
   secret release. Recreate worker/egress endpoints and credentials, refresh
   clock/entropy, and invalidate old connections. Do not replay old leases.
6. Demonstrate real execution continuation (CPU instruction position, RAM,
   disk changes and desktop state), failure recovery, wrong-owner/image and
   tamper rejection, interruption at each transition and stale-lease refusal.
   A successful encrypt/decrypt round trip alone is insufficient.

Until these pass, native full snapshot capability remains unsupported. The
live native desktop and its cursor/game/home customizations must be preserved.
Do not reboot it to test a speculative checkpoint path.

## Disposable availability result (2026-10-27)

The non-debug IGVM `0a8fe8682750fd8827cb03164852944a4c23b94764579a6d02292817811f9c71`
booted a synthetic, diskless 2 GiB/one-vCPU guest. `/dev/snapshot` opened and
closed successfully without freeze/restore ioctls. `/sys/power/state` included
`disk`; the VMBus hibernation attribute remained `0`. The missing lockdown
sysfs file is not proof of a security property. Exact diagnostic VM and task
absence, unchanged preexisting VM states and unchanged master VMGS hash were
verified after cleanup.

The subsequent capture/read/discard candidate built deterministically as
`abe26d3cd1740d4b47f1445beb6f81ddf61505d13841e32d9debb5a8d81eab38`.
It subsequently ran successfully on 2026-10-28 at 02:23 UTC: the guest read
92,291,072 bytes, then freed the image, thawed and closed the device. The
independent watchdog and a final separate query confirmed the owned VM and
both scheduled tasks absent. All four preexisting VM IDs/states and pinned
host modules/master VMGS remained unchanged. Receipts are under
`snapshot-probes/evidence/`. This qualifies capture/read/discard only.

## Native encrypted checkpoint transport

`snapshot-crypto::native` now packs the memory image and a consistent writable
storage stream into an authenticated private envelope. The trusted expected
binding covers kernel, platform, root filesystem, configuration, RAM, vCPUs
and admission epoch. App scope is fixed to `kvm-box-native`; owner/deployment,
bucket and object are also authenticated. It rejects short/extra components,
wrong bindings and truncated or damaged objects. Opening authenticates the
complete object into an unlinked mode-0600 private guest staging file before
exposing either component. The caller must retain exclusive ownership of that
file and provide private backing storage. Error and drop paths truncate it.
Epoch equality is NOT an anti-clone service: fresh external exclusive admission
and secret release remain prerequisites to production restore.

Six new transport tests pass alongside nine existing crypto/policy tests.
The capture acceptance parser has eight passing tests. No native app endpoint
is connected to this preparatory transport yet.

A two-boot synthetic encrypted roundtrip probe is under `snapshot-probes/roundtrip`.
It is not an app helper and must never ship in a user image. Its fixed key and
binding markers are public test material, not real secret custody/measurement.
It requires one newly created blank 1 GiB virtual disk and an independent host
watchdog. The capture branch saves authenticated state and powers off without
closing `/dev/snapshot`, following the pinned kernel's userland-swsusp contract.
The separate restore boot authenticates all bytes before loading the kernel
restore device. Success requires returning to the original CREATE_IMAGE call
with original=0, restored RAM sentinel and functioning TPM transport. The static musl helper and deterministic non-debug image have built successfully
(image SHA-256 cd5b06ded55dec460547531e47ca4de22775b9fc11e1d52d58c9e34e92df41e7). This
probe has not been booted; no restore or desktop continuation claim is made.
The small synthetic disk marker tests the transport's second component, not
filesystem consistency or full writable-disk restoration.

The two-cold-boot host harness is prepared but unexecuted. Its acceptance tests
have not run; a read-only host syntax/cmdlet check timed out. Native operations
are now held for priority Metal0 recovery. The successful build receipt is in
`snapshot-probes/evidence/roundtrip-build.json`; the earlier failed build was
caused by an incorrectly applied global file-size cap and is preserved locally.
The corrected build kept the original 2 GiB memory / 512 MiB total scratch caps.

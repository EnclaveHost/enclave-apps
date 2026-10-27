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

A platform redesign is therefore required before the acceptance work below.
The owner has been asked whether to retain the current platform with native
snapshots explicitly unsupported or authorize that separate redesign. The
private RISC snapshot implementation can be completed independently.

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

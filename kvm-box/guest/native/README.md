# Native desktop companion — development components

The signed native desktop profile has been deployed on NucBox as
`kvm-box-native:0.9.0`; the earlier startup-only checkpoint below is no longer
its deployment status. See [DEPLOYMENT_STATUS.md](DEPLOYMENT_STATUS.md) for the
current release pins and the distinction between deployed code and candidates.
Native nsinit, the Wasm controller, gateways and supervisor have composed KVM
startup/stop and failure tests. The measured profile launcher and authenticated
worker transport also have native fixture coverage; standard sd-cli and
llama-cli have completed masked operations on NucBox. Owner testing verified
usable Moonlight streaming. This does not certify all graphics APIs, games or
untrusted-host confidentiality. See nsinit.md for the component boundary and
../../shield-graphics/qualification/CTS.md for the exact test scope.
Never install the test nsinit
or the kernel-check bypass binary in a release image.

## Process boundary

The owner-authenticated Wasip2 controller runs as the domain runtime UID
(5001–1048575) and connects to TCP 127.0.0.1:19080. The unprivileged Go gateway
runs as UID/GID 1002, outside the nested desktop PID/mount/IPC/UTS namespaces.
It uses the root supervisor's Unix control socket for lifecycle and proxies
allowed desktop traffic to the inner gateway, UID 1003 on 127.0.0.1:18080.
The desktop session is UID 1000; the trusted masking service is UID 1001.
All share the domain's private loopback network namespace. The measured front
UID is runtime UID + 1048576, matching the M3 monitor.

The privileged launcher must install network_policy.py's generated nftables
rules before starting workloads. Both source-port and destination-port UID
checks precede established-flow handling, preventing a different UID from
impersonating a stopped service. Source UID gates cover the controller, both
gateways, noVNC/UI (1003), RFB/Sunshine (1000), SOCKS/DNS (front), and the private
masking-service address (1001). Only the front may connect to controller port
8000. Every unlisted UID fails closed; root and the front are explicit trusted
exceptions for other egress. The raw worker channel must be a Unix socket
broker accepting only UID 1001. No TCP GPU bypass is allowed.

The exact pinned kernel currently has `CONFIG_NFT_CT=m`. The profile launcher
must supply a measured matching `nft_ct` module and load it before installing
these rules, or use a separately qualified kernel with the expression built in.
The composed KVM test found the missing-module dependency and refused startup.
A matching module was built against the pinned linked kernel symbol CRCs with
the source tree mounted read-only; the kernel ELF/bzImage remain unchanged.
Do not omit connection tracking or continue when nft policy installation fails.

TCP does not provide a trustworthy peer UID. The gateway relies on the measured
network policy, not headers or RemoteAddr, for controller identity. Native
roles must lose capabilities and use seccomp to prohibit namespace changes,
AF_VSOCK and io_uring. Those launch steps are not implemented by this gateway.
Root platform components remain trusted. This is not an untrusted-host GPU
confidentiality proof.

## Supervisor

supervisor.c requires UID/EUID 0, the exact kernel release
6.6.87.2-enclave-kvm-box, a root-owned read-only /desktop.profile containing
KVM-BOX-MASKED/1 followed by newline, and a root-owned read-only executable
/plat/native-desktop/nsinit. It executes the opened nsinit file descriptor,
not a later path lookup. Args are exactly vCPUs 1–16 and memory MiB 8192–65536;
these are declared status values, not a replacement for outer cgroup limits.

/run/native-desktop must be root:1002 mode 0750. The supervisor creates its
control.sock root:1002 mode 0660 and refuses an existing socket. Only a peer
with Unix SO_PEERCRED UID 1002 may send one byte Q (status), B (begin), or H
(halt), followed by EOF. Replies are u32 big-endian length plus canonical JSON:
{"status":"stopped","pid":0,"vcpus":4,"memoryMiB":8192}
The gateway validates the server peer UID 0 and both filesystem permissions
before sending. Protocol errors expose only generic HTTP failure.

The domain root must already be a mount point when the supervisor starts.
The child becomes PID 1 of new PID/mount/IPC/UTS namespaces, and recursively
makes its mount tree private before exec. The supervisor resets inherited
signal masks/SIGCHLD and normalizes all three null stdio descriptors, retaining
the verified executable outside those descriptor numbers. fd 3 is a readiness
pipe: nsinit sends exactly R then closes it. fd 4 is the supervisor pidfd and
must remain monitored. Other inherited descriptors are closed; environment is
cleared. PR_SET_PDEATHSIG and pidfd checks close the parent-death race.
Readiness times out after 120 seconds, allowing full model verification on
slower storage. Stop uses pidfd SIGTERM, then SIGKILL
after five seconds. Killing namespace PID 1 also removes descendants that
escaped their process group. Unexpected termination becomes failed.

The real nsinit now composes read-only root preparation, private writable
mounts, device/input mediation, role credentials/capability removal, seccomp,
readiness and child reaping. Its composed KVM tests use actual desktop services with synthetic compute
resources. The real controller/supervisor and service graph now pass the
combined functional test described below; platform admission and authenticated
GPU resource integration remain open.

## Gateway and controller

Build gateway/gateway.go using CGO_ENABLED=0 go build; it has no external Go
dependencies. It requires UID/EUID 1002, no arguments, and no inherited
KVMBOX_OWNER_PASSWORD or KVMBOX_ACCESS_SHA256 variables. Production endpoints
are fixed; test configuration is private to the package.

GET /v1/status and POST /v1/start or /v1/stop are bodyless and queryless.
POST requires X-KVM-Box-Request: 1. The Wasm controller now emits that header
only after its own owner/action validation. The gateway's serialized control
exchange has a 3-second connect timeout and one 5-second I/O deadline.
Already cancelled actions do not contact the supervisor.

Desktop routes include noVNC static assets, /compute/*, Sunshine pairing,
/websockify and the fixed-port Moonlight tunnel. The gateway strips owner
Authorization, cookies and spoofed forwarding headers; the separate compute
authorization header is preserved. Request bodies are capped at 8 MiB and
headers at 16 KiB. Only 32 connections, including upgrades, can be active.
WebSocket accept values and Moonlight destination queries are checked.
Streams propagate half-close and have bounded idle and write timeouts.
Go net/http collapses identical duplicate Content-Length fields; the outer
Wasm controller performs raw framing validation before this gateway.

## Static UI image assets

`build_ui_assets.py` stages a fresh tree for `/usr/share/kvm-box/native-ui`
from the pinned noVNC distribution and `guest/native.html`. It includes only
vnc.html, app, core and vendor, plus the native compute page. Every noVNC
lookup uses openat2 beneath a pinned directory descriptor; internal aliases
are flattened and escaping aliases, cycles and special files are refused.
File, aggregate, traversal and path limits bound the build. Existing outputs
are never overwritten. The manifest records each file's SHA-256 and size.

Image packaging must assign UID/GID 0 and mount the resulting tree read-only;
staging modes alone do not provide that boundary. Build hashes establish byte
identity, not authenticated image admission. The inner gateway must check its
fixed asset tree and must not serve desktop-home content. The native compute
page defaults to CPU/native-model-v1 arithmetic; the user must explicitly
select the approximate masked mode.

## Native worker SDK and service policy

The two SDK libraries now have a prepared Unix worker route with connected
root-peer validation, each patched against its own existing source baseline.
The separately staged service policy admits desktop UID 1000 and inner UI UID
1003 and uses only the fixed Unix route. See `worker-route.md` for build recipes,
verification and the remaining authenticated-resource/admission requirements.

## Verification

- Nine supervisor checks use real namespaces, pidfds and Unix credentials,
  including escaped descendants, abnormal readiness, supervisor death, inherited
  signals/closed stdio, child mounts under a shared parent, the actual two-minute
  startup deadline, and explicit stop during startup.
- Nine network checks use actual nftables and packets, including wrong-UID
  server impersonation and raw-worker TCP denial.
- Gateway tests use real Unix sockets and TCP, protocol rejection, credentials,
  body limits, upgrades, half-close, cancellation and connection limits.
  They run with Go's race detector; go vet and the static build also pass.
- Thirty-two native controller tests and nine actual Wasip2 checks pass.
- Five combined-chain checks run the real Wasip2 controller as UID 7501,
  production gateway as UID 1002, root C supervisor and installed nft policy.
  They verify owner/action checks, start/stop/restart, direct TCP denial,
  missing-desktop failure, header stripping, WebSocket and Moonlight streams.
  The nsinit and inner HTTP/stream service are explicit fixtures.

Test commands and exact binary/source hashes are recorded in the task output
evidence. These checks establish neither full desktop/attestation acceptance
nor end-to-end GPU speedup. Runtime admission remains disabled.


## Confined input udev helper

udev-launch.c executes only the fixed systemd-udevd binary. It closes inherited
fds above stderr, clears its environment, bounds workers to four, keeps only
CHOWN, FOWNER and NET_ADMIN, and installs NoNewPrivs plus
an architecture-checked seccomp filter. The capability bounding set is trimmed
against the running kernel, including capabilities newer than build headers.
CAP_NET_ADMIN is needed to broadcast libudev events. Its socket policy allows
only AF_UNIX and NETLINK_KOBJECT_UEVENT; NETFILTER/ROUTE/generic netlink, all IP,
vsock and packet sockets are denied. ioctl, namespace/mount operations, BPF,
io_uring, mknod/mknodat and privileged tracing/module/raw-I/O operations are also denied.
The helper is measured trusted code, not a sandbox for arbitrary root apps.

Real syscall tests verify allowed private-netns multicast and denial paths,
then exec and recheck capabilities, seccomp and NoNewPrivs. A separate isolated
probe started the packaged real systemd-udevd and observed its private control
socket with CapEff/CapBnd 0000000000001009 and NoNewPrivs=1. That probe passed no
host input/uinput/UHID/GPU devices and did not trigger host sysfs events.

Three isolated local KVM probes now establish real uinput keyboard creation,
udev hotplug/ownership and rootless Xorg key press/release events (no XTest).
That stage uses the four-capability udev policy and production role policy
for Xorg and the input application. Guest devtmpfs creates event nodes, so
udev needs neither CAP_MKNOD nor mknod syscalls. A subsequent root-layout KVM probe also passes with the restricted native
device view. Moonlight input and NucBox desktop input still require qualification. The native launcher must provide a read-only
root and sysfs, private /run/udev and an input-only device tree. Guest /dev/input
may be bound for dynamic device nodes only after exclusive-desktop-guest
admission is enforced; Linux input devices are not namespace-scoped. Never
bind the physical host's input tree. The real nsinit now composes these tested
helpers; exclusive production admission remains unfinished.

## Review resolutions and remaining invariants

The controller now rejects WebSocket queries consistently with the gateway,
forces nosniff and same-origin framing policy on desktop responses, and evicts
an old unauthenticated header connection when capacity is full. Established
authorized streams are not evicted. This mitigates idle-slot exhaustion; it is
not a general Internet denial-of-service guarantee.

Desktop HTML/JS must come only from measured read-only assets served as UID
1003. Serving executable content controlled by desktop UID 1000 on the owner's
origin would let it act with cached browser credentials. Response headers alone
do not fix that. `inner/inner.go` now enforces this for its static routes and
emits compute responses only as JSON. The measured launcher must install the
fixed assets and process boundary; see `inner/notes.md` for the tested scope.

A stale supervisor socket is intentionally refused after SIGKILL. The measured
launcher must tear down/recreate the domain's private /run after supervisor
failure; it must not blindly restart against existing state. Automatic stale
socket recovery is not provided. Remaining nsinit obligations include closing
fds 3/4 in roles, PID-1 signal/reaping behavior, nosuid/NoNewPrivs and role
seccomp, Unix server peer authentication, sockets created after UID drops,
and outer cgroup resource enforcement. Declared status limits alone do not
enforce CPU or memory limits.


## Native role and root preparation integration

`role-policy.h` clears every capability set, drops real/effective/saved IDs,
sets NoNewPrivs and installs the native 101-rule seccomp filter
(a496482d694447a8c9f1869fe4455d1e18e75052ca47132f8fe5652a716ad410).
`wx-policy.h` refines the unchanged pinned M3 71-rule app-seccomp program
(d4d17c9f53832439c92a3232fd09feed8b28f0e3c7dd357d26468a9566f62b66)
by replacing its final fall-through ALLOW. The pinned baseline has no early
ALLOW for the memory syscalls below; recheck this when updating that baseline.
All existing architecture checks and syscall denials remain in place.
UID 1001 keeps primary group 1000 for its desktop-facing compute socket and
supplementary group 1001 for the worker socket. UID 1003 has supplementary
group 1000; compute service peer-UID authorization must explicitly include it
in the measured native profile. Group access alone does not authorize requests.

The native refinement returns EPERM before `mmap`, `mprotect`, or
`pkey_mprotect` can create a simultaneously writable and executable mapping.
It also denies writable `shmat` with SHM_EXEC and setting READ_IMPLIES_EXEC;
role startup refuses an inherited READ_IMPLIES_EXEC personality. Ordinary RW
memory and RW-to-RX JIT transitions remain available. Each role installs the
filter before exec, so descendants inherit it. The Firefox preference
`javascript.options.content_process_write_protect_code=true` is still needed:
it makes Firefox use compatible transitions while leaving JIT and WebAssembly
enabled. A denied syscall does not guarantee an application will recover;
an application may exit if it cannot handle the error.

This is preventive enforcement for explicit syscall requests, not a replacement
for the attestation W^X scan. Seccomp cannot inspect ELF program headers or
detect writable/executable aliases to the same backing storage. Keep admission
fail-closed if the scan detects an actual violation. The change requires a new
measured image and qualification before deployment; editing these sources does
not change running instances. `tests/check_native_wx.c` tests the real filter
in a disposable child, including denied transitions preserving old permissions,
working JIT code, thread/fork/exec inheritance, and existing isolation denials.

Four real namespace tests cover irreversible IDs, setuid exec, capability
bounds, syscall denials, working threads and CPU JIT. The actual Go gateway
now runs under this policy in all five combined-chain checks. The adapter and
nsinit remain fixtures, and the caller must close inherited descriptors,
provide a clean environment, and enforce filesystem/network/cgroup boundaries.

Claude contributed `native-root.h` and its notes. The helper compiles with
strict warnings and passes a real local KVM root-layout/input probe. It requires an exclusive
guest, fixed read-only image/model mounts and the root worker socket, creates
private writable mounts, and binds only fixed guest input devices. Observable
mount/socket metadata does not establish attestation or authentication. Full
nsinit integration and authenticated admission remain open; default-home seeding
is now implemented and tested below.


The root-layout probe uses the exact production header and checks four specific
rejection cases, private/RO mounts, fd3/fd4 preservation with no helper FD leaks,
role-specific worker access with reciprocal peer UID checks, PTY/UHID access,
and real udev/libinput/Xorg key events. The worker and model sources are fake
fixtures; this root-layout checkpoint alone does not qualify full integration.
Later composed tests are recorded below; Moonlight streaming, authenticated
resources and runtime admission remain unqualified. The exact probe logs and reconstructible
initrd overlay are exported as evidence/native-root.


## Independent root-layout review hardening

Claude's review led to read-only sysrq/PCI/IRQ controls and fixed device inode
metadata, nodev on private /dev, removal of udev DAC_OVERRIDE, guards for the
original procfs/namespace view, stricter source device mounts, and mode-0600
desktop PTYs. The final isolated KVM run passes eight negative/fault cases,
all four process roles and real keyboard input with three udev capabilities
(mask 0x1009). It also checks blocked terminal writes from UID1001/1003, denied
udev UHID access, read-only procfs controls and device chmod, and cleanup after
an injected late chroot failure. The actual daemon and syscall/exec tests pass.
The previous four-capability input/root checkpoints remain historical evidence.

Authenticated models/worker reservation, cgroups and runtime admission remain
open. Later nsinit/control-graph integration is recorded below. Home seeding and authenticated X component
qualification are recorded below. The
probe is a component test, not the published enclave app.


## Native desktop defaults and authenticated X — 2026-10-24

`home-seed.h` restores exactly 14 pinned public default files (15745 bytes) into
the fresh private home from compiled immutable data. The builder rejects
unexpected/private state, changed contents, links and invalid paths. Empty-home,
UID/mode/filesystem checks and exclusive file creation prevent overwriting
an existing home. Failure requires nsinit to abort, never start roles or emit
readiness. All current menu entries are preserved pending compute acceptance.

`desktop-auth.h` creates a fresh 16-byte MIT-MAGIC-COOKIE-1 credential only in
private runtime tmpfs, UID1000 mode0600. Authenticated Xorg uses no TCP or
abstract listener. The UI role cannot read the cookie; unauthenticated X clients
are refused. The custom raw-X11 FreeDoom client now supports the cookie without
adding Xlib or changing its fast rendering loop. Explicit invalid XAUTHORITY
fails closed; absent XAUTHORITY retains compatibility with old development
images. Seven parser test groups cover bounds, truncation, identity and unsafe
files. A statically rebuilt test binary runs against real authenticated Xorg.

The composed local KVM test passes the eight root failure cases, home parity,
X authorization, real keyboard events, Fluxbox, xterm's controlling PTY, a
Firefox window, FreeDoom and an active PulseAudio stream. `/dev/tty` and
`/dev/full` are now fixed guest-device binds with read-only metadata; no raw
GPU/KVM/TPM/vsock device is exposed. The exact source and logs are exported in
evidence/native-home. This remains software rendering, fixture model/worker
sources, and a test init. Full service supervision/readiness, authenticated
compute, Moonlight streaming, native admission and publication remain open.


## Combined native control graph — 2026-10-24

A disposable exact-kernel KVM VM now joins the real Wasip2 controller, outer
Go gateway, production C supervisor (no kernel bypass), nsinit, inner gateway,
compute status service and desktop graph. Nine checks pass: owner/action
gates; actual start; immutable UI/compute status; VNC binary WebSocket to real
x11vnc; Sunshine server-info through the fixed Moonlight tunnel; front/runtime
direct-bypass rejection; clean stop; fresh restart; and supervisor-death
removal of nsinit plus all ten direct desktop services. After supervisor death,
control reports 503 and desktop proxying reports 502.

The Wasm component runs in Wasmtime 49.0.3 under UID7501 with no capabilities,
NoNewPrivs and the pinned app seccomp filter, through a fixed test exec adapter.
The front connection driver uses UID1056077; the outer gateway uses the
production role policy. Complete nft rules are installed before workloads.
The worker listener accepts UID1001 and receives no compute payload during
three readiness probes. Model and calibration files are synthetic.

This is functional control/desktop integration, not a production M3 domain
launcher or attested/reserved GPU test. No GPU execution, model-quality or
performance claim follows. The Sunshine test verifies server-info transport,
not Moonlight video/audio/input streaming. No NIC, disk, host devices or shared
filesystem are attached. The test uses only synthetic owner credentials and
does not change the live desktop. Evidence is in evidence/native-control-graph.
The initial incorrect bypass/status expectations are retained as harness
failures; the final nine-check run exits successfully.


## Ancestor process privacy and integrated input — 2026-10-24

Claude's focused review found that private child procfs alone does not protect
the early Sunshine credential argv from an outer process namespace. nsinit now
requires hidepid=2/invisible on the original procfs and every visible procfs
alias, with no nonzero bypass gid. It pins the actual mount ID and fails before
root preparation or credentials. The outer launcher must establish this before
untrusted roles start, reserve the role UIDs, remove group0 membership, and
keep higher ancestor namespaces within the trusted platform boundary.

The latest actual control/desktop KVM run passes eleven checks, including an
argv canary hidden from outer gateway/controller/front roles and real keyboard
press/release through uinput, the confined udev daemon, libinput and Xorg while
Fluxbox is running. The fixed minimal udev rule assigns event/js nodes to
1000:1000 mode0600. Compute1001 and UI1003 cannot open those nodes despite their
shared compute-socket group. No XTest or synthetic X input events are used.
The initial popup focus race was corrected in the harness with an ordinary
override-redirect window. A separate negative KVM run rejects unprotected
ancestor procfs; thirteen parser cases pass. See evidence/native-proc-input.

The image-tool preparation above is superseded by the separate-image
qualification below. Production admission and full GPU/streaming acceptance
remain open.


## Separate immutable native image and CLI qualification — 2026-10-24

Built the separate 564,912,128-byte SquashFS/xz desktop root with SHA-256
15315e3669ad945f1d11133d48039c5075e2df8efaad07a16ea6b9b394798224 and dm-verity
root 0ff673f600eff89d2dc3bd84371d429862e50a67f48be3682ff228cf08a76f59.
The unprivileged packager uses private user/mount namespaces, a read-only base
and private overlay. The complete base content/mode identity is unchanged.
The candidate installs the selected hardened nsinit, Wasip2 controller,
supervisor/gateways, worker SDK, input rule, native service policy and xdoom.
The existing Debian Firefox ESR engine, uBlock, FreeDoom WAD and desktop defaults
are preserved; Firefox now has the locked local SOCKS/DNS route. A custom x86
Firefox engine build is not claimed. Both compute menu entries remain pending
compute acceptance.

Claude completed a focused seven-turn packager review. Its SDK closure, privacy
cleanup and umask findings were resolved: every SDK leaf must be covered,
absolute/escaping SDK symlinks are refused, copied trees are compared, walk
errors fail, directory modes are stable, and common session/private state is
cleared. Machine IDs/hostname/hosts use a fixed public image identity rather
than the build container identity; per-session unlinkability is not claimed.
Six malformed SDK cases fail before output creation. Final SquashFS inspection
checks all fourteen component pins, parity assets and minimal input rules.
Build tools/host remain trusted; bit-reproducible timestamps are not claimed.

The exact candidate passes eleven real KVM control/CLI checks using the pinned
kernel. Root and real model disks are read fully through read-only dm-verity
mappings (564,912,128 and 2,546,024,448 data bytes). Four separate corruption
cases receive kernel EIO for root data/hash and model data/hash. Only disposable
qcow2 overlays are modified. The image's trusted model-volume descriptor pins
the model hash tree, and the native service checks model/policy/calibration
hashes. Production authentication of these pins remains a launcher obligation.

Inside the real confined desktop, UID1000 with zero capabilities, NoNewPrivs and
seccomp successfully runs clinfo -l, nvtop -s, llama-cli --version and sd-cli
--help. clinfo lists the Shield logical device; the Shield-aware nvtop displays
logical service counters and null physical GPU telemetry. A short native CPU
Qwen inference succeeds with native-model-v1 arithmetic. No GPU execution is
implied: the synthetic root-owned broker receives three readiness probes only.
The full owner-gated control graph, VNC, Sunshine server-info tunnel, restart,
clean stop and supervisor-death cleanup also pass.

This is a candidate image, not an admitted/published enclave app. The test uses a
fixed Wasmtime UID7501 adapter and synthetic owner credentials. Production M3
launcher/cgroups/exclusive-guest admission, authenticated worker reservation and
routing, platform attestation/authorization, GPU numerical/performance acceptance,
real Moonlight streaming, NucBox deployment and wallet publication are still open.
No end-to-end GPU speedup is established. The live controller, desktop, worker
and Moonlight client remain running unchanged. Candidate and evidence are in
native-root-candidate and evidence/native-root-image under outputs/kvm-box.


## Native resource primitives qualified — 2026-10-24

The new M3 desktop resource package now passes two real KVM tests on the pinned
kernel and selected immutable root/model volumes. It verifies canonical profile
and controller binding, actual active dm-verity tables, read-only filesystem
roots and mount identities. It retains resource descriptors through launch.
An atomic guest tmpfs claim admits one contender out of 32 and stays consumed
through errors and teardown. The single-use claim cannot create another cgroup
after removal. CPU quota, 8 GiB memory, no swap, group OOM, 1024 tasks and whole
descendant cleanup are enforced through the same cgroup fd used for clone.

Claude found claim-object reuse and bind-mounted-subdirectory acceptance; both
were fixed and tested inside KVM. The OOM test records an actual kernel group
OOM killing the allocator and an idle sibling. CPU/process-limit and descendant
cleanup checks pass separately. Go race tests and vet pass. Disk enumeration
now tolerates changing sd names and still authenticates data with verity hashes.

This package is not yet wired into production monitor/domexec admission, which
remains disabled. Worker lease/authentication and the final measured release
binding remain unresolved. No GPU work or owner secret is used by these probes.
Evidence, failures and review resolutions are in evidence/native-resources.


## Native broker endpoint prepared — 2026-10-24

Claude implemented an opt-in root-owned Unix endpoint in the shared broker.
The reviewed final code passes all eleven registered tests inside KVM, with
real credential-dropped clients. Root and same-group impostors are rejected
before upstream dialing; the authorized UID succeeds and authenticates a root
server peer. Existing broker behavior, connection limits and cancellation pass.
Host race tests/vet and both legacy broker command builds pass.

The native launcher must still authenticate/reserve the worker and bind its
lifetime to the desktop claim before publishing this endpoint. The option is
not enabled by any production caller yet. No GPU work occurs in these tests.
See evidence/native-broker-endpoint for sources, Claude's draft and KVM results.


## App-bound worker transport qualified in KVM — 2026-10-24

The new native worker lease binds one guest boot/claim to the host's verified
AppID, profile digest and GPU share. Only the root monitor holds its stream
capability. Control disconnect or unexpected bytes revoke every stream, with
no second claim or reconnect. Both sides bound pending/active streams to eight
and drain canceled dials. This is the NucBox operator-trusted T0-hv transport;
it does not make the GPU trusted or provide host-excluded SNP authentication.

Claude implemented the guest client draft. Review added pending-dial drainage,
closed-buffer handling and short-write checks. Host/guest race and socket tests
pass. Actual KVM qualification connects the guest Go lease and root-owned Unix
broker to the Node host server through vsock: a foreign VM is refused before
authentication, root and same-group Unix impostors are refused, 32 successive
UID 1001 clients each exchange 40960 public bytes, and revocation closes a 33rd
held stream. All 33 mock worker streams drain. No GPU operation is performed.

Investigation of the first failed integration run found a reproducible bug in
the existing vsock connection wait. The old blind-first-wait pattern times out
20/20 times on an already-connected socket. The fix checks actual connection
state before waiting; unconnected/refused cases remain rejected. The final KVM
run and existing front/monitor regression checks pass with this fix. Historical
failures and diagnostic passes are retained in evidence/native-worker-lease.

The Windows bridge itself, manager GPU allocation, separate image selection,
production monitor/native launch, release binding and actual GPU workloads are
not yet integrated by this phase. Desktop admission remains disabled. No owner
secret, running user service, NucBox deployment or publication was changed.

Desktop CLI helpers may use IPv4 TCP on their own 127.0.0.1 loopback address.
Desktop-initiated connections carry a reserved conntrack mark; replies must also
originate from UID 1000 before the established-flow rule can accept them. This
allows llama-cli's internal HTTP helper while refusing helper impersonation by
other roles. Existing fixed-port guards and the UID 1001-only 127.0.0.3 service
address take precedence. `test_loopback_policy.py` checks the real nft policy in
an isolated network-none container, including cross-role and control-port denial.

The pinned public home inputs now live in `home-defaults/`. Rebuild the header
with `python3 build_home_defaults.py --root home-defaults --output home-defaults.h`
from this directory. The manifest authenticates every input. The two diagnostic
compute menu entries have been removed after native standard CLI acceptance;
Firefox, Terminal and DOOM remain. The protected backend is unchanged.

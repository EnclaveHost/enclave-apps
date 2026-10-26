# Native namespace init

`nsinit.c` composes the fixed root, home, X authorization and role helpers. It
accepts no arguments or environment configuration. It requires UID/EUID 0,
PID 1, the exact measured kernel, a write-end readiness pipe at fd 3 and a live
supervisor pidfd at fd 4. Services receive neither descriptor nor owner secrets.

The fixed graph contains ten monitored foreground children: constrained udev,
Xorg, session D-Bus, PulseAudio, Fluxbox, Sunshine, x11vnc, websockify, the native
compute service and the inner gateway. The desktop role is 1000, compute 1001,
UI 1003; readiness requests to the inner gateway run as 1002. Other than udev,
roles have no capabilities, NoNewPrivs and the pinned seccomp policy. No shell
or tenant-selected executable participates in startup.

Sunshine gets a fresh per-boot random credential before any other desktop
process starts. Only its early credential helper receives the value in argv;
the child procfs is already private, and all visible ancestor procfs mounts
were checked for hidepid=2/invisible before any credentials were generated. The UI role's mode-0600 tmpfs file holds
the value for pairing. Parent buffers are wiped. Owner/wallet credentials are
never inputs. Xorg uses the separately generated private Xauthority file.

Startup is bounded by one 115-second monotonic deadline, including exec/probe
retries. `readiness.py` validates the root local worker peer without sending
protocol bytes, the actual compute status through its peer-checked Unix socket,
and the inner JSON/HTML routes. Desktop socket and fixed TCP readiness checks
also run. Only success emits exactly R and EOF. Process exit, parent death or a
failed deadline tears down the namespace; clean external SIGTERM stops it.
Shutdown sends TERM/CONT, then KILL within a total three-second grace. Liveness
is monitored after readiness; hung-but-live daemons are not yet health-checked.

The supervisor's outer deadline is 120 seconds. The service verifies about
2.44 GB of model data before listening; a controlled 32 MiB/s model-device
limit reproduced failure under the former 55-second budget. The longer budget
allows that verification to finish without skipping it or reporting early
readiness. It is a fixed limit, not an indefinitely renewable progress timer;
slower or stalled storage can still fail startup. This reproduction does not
establish the cause of the earlier uninstrumented timeout.

The composed native-platform test with the new budget reaches readiness in
80 seconds at the same 32 MiB/s limit, then draws changing Vulkan and OpenGL
frames and cleans up the domain and cgroup. The root filesystem differs only
in nsinit/supervisor; its other 26,029 paths remain identical. This is a
controlled storage-delay regression, not general reliability qualification.

## Packaging and platform requirements

Install root-owned read-only `readiness.py` at
`/usr/local/libexec/kvm-box/readiness.py`, the fixed `inner` and `udev-launch`
binaries beside it, and nsinit at the supervisor's fixed measured path.
Stage immutable UI with `build_ui_assets.py`, and build `/shield/native-service.json`
with `build_service_config.py`. Use the selected Unix/root-peer SDK transport.
Set `/etc/pulse/client.conf` to `autospawn = no`. Preserve the seed manifest,
Sunshine/Xorg configs, fixed binary paths and role credentials.

The outer launcher must mount every visible ancestor procfs with hidepid=2
(or invisible) before any untrusted roles start; a nonzero hidepid-bypass gid
is refused. All untrusted roles must have no group 0 membership. Role UID1000
must not be reused by other outer processes during credential setup. Higher
ancestor namespaces outside the domain remain trusted platform TCB and must
not expose another procfs view to untrusted code. `proc-privacy.h` pins the
actual /proc mount ID and checks every visible procfs alias before root/home
preparation. These checks close the temporary credential argv visibility gap.

The root image must contain only `70-kvm-box-input.rules` in its udev rule
set (and no extra /etc overrides). Its input_id classification and seat tag
allow real hotplug while event/js nodes are finally assigned 1000:1000 mode0600.
That excludes compute/UI roles even though they share group1000 for compute
sockets. This requires the exclusive guest/device admission described below.

The outer privileged launcher must authenticate and reserve models/worker,
install the complete nft policy (including its nft_ct kernel dependency),
supply read-only resources, enforce exclusive-guest/cgroup resource limits and
admit the measured image/profile before starting the supervisor. Root-owned
socket metadata, peer UID and hashes alone are not remote authentication.
Neither nsinit nor readiness implements those outer obligations.

## Current verification

A disposable KVM guest with the exact pinned kernel starts the actual ten-service
graph, checks the actual inner gateway through role 1002, and stops cleanly. A second run kills Fluxbox and verifies failure teardown.
Both runs verify ten direct service children, the sole constrained root udev
helper, zero capabilities for the other roles, NoNewPrivs and seccomp.
Its model/calibration files and local worker are synthetic fixtures. The test
has no NIC, host input, GPU, disks or shared filesystem. It loads the matching
nft_ct module before installing the production isolation policy. The initial
fixture's insufficient memory and missing module failures are retained in the
evidence. Host entry and unexpected arguments are refused.

The real controller/outer gateway/supervisor and this graph also pass a
combined nine-check KVM test, including actual VNC and Sunshine server-info
transport, restart and supervisor-death tree removal. Its fixed runtime exec
adapter is a fixture; GPU resources remain synthetic. Claude's focused review identified the ancestor-proc credential exposure,
now fixed and regression-tested. No further actionable defect was reported
within that review's stated scope. This component is not a full
controller-to-worker acceptance run and is not deployed. Masked
GPU correctness/performance, authenticated resources, production admission,
Sunshine/Moonlight streaming, NucBox acceptance and wallet publication remain
separate incomplete requirements. Compute menu removal remains gated on compute
acceptance.


The latest composed run passes eleven checks, adding a credential argv canary
hidden from outer UID1002/controller7501/front1056077 and real uinput→udev→
libinput→Xorg key press/release under the actual Fluxbox session. Input nodes
are checked mode0600, and UID1001/1003 reads fail with EACCES. A test popup
uses ordinary X override-redirect to avoid window-manager focus races; it does
not synthesize X events. A separate actual-kernel negative run refuses startup
with hidepid=0 before root preparation or any worker probe. Thirteen parser
cases cover mount-ID selection, aliases, weak options and bypass groups.
Latest evidence: evidence/native-proc-input.


## Separate graphics image

Compile nsinit with `-DKVMBOX_SHIELD_GRAPHICS` only when packaging the new
measured Mesa prefix at `/opt/shield/mesa`, its runtime dependencies and the
Lavapipe ICD. This build fixes desktop-launch environment settings to that ICD
and Zink, with two llvmpipe threads. Ordinary menu/terminal applications inherit
them; no per-application service API is required. The baseline build remains
unchanged. These settings choose the implementation, while the role's device
and worker policies enforce physical GPU isolation. Unsetting them can select
software rendering but cannot grant a physical GPU device or worker route.
The graphics build has a different measured nsinit binary and root image.

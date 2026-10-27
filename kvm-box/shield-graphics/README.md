# Standard graphics driver work

The requested endpoint is usable OpenGL/Vulkan in the native desktop, with
physical GPU work restricted to the masked channel and unsupported work kept
inside the VM. This directory contains the graphics reference and acceptance
path. A [protected OpenGL/Vulkan affine compiler](driver/README.md) now passes real
worker tests through a patched Mesa ICD in a host container and the actual
native KVM desktop. General masked rendering remains incomplete.

The desktop base contains Mesa's software OpenGL implementation and a Vulkan
loader, but lacks a Vulkan ICD. The separate development reference adds Mesa
Lavapipe and tests OpenGL over Vulkan using Zink. Keeping both APIs on one
renderer gives future protected compiler work a common reference path.
[Mesa documents Zink's Vulkan requirements](https://docs.mesa3d.org/drivers/zink.html).

## Multi-workgroup candidate

The [larger-dispatch checkpoint](qualification/grid-checkpoint.json) extends
the proved integer path to 1–64 workgroups (64–4,096 rows). It distinguishes
local and global invocation IDs, tiles approved products through the existing
masked channel, and publishes results only when every tile has verified.
All 44 host driver cases pass, including final-tile corruption and CPU-only
negative controls. Separate native KVM qualification also passes; production platform packaging,
hardware admission and deployment remain pending.
This extends compute coverage; general graphics rendering remains local.

## Current generalized compiler checkpoint

The compiler now extracts a bounded 4×4 integer matrix plus modular bias and row
terms from ordinary Vulkan and Zink-generated OpenGL shaders. It keeps bias and
row terms inside the VM, sends only the bounded dot product to the existing
Shield service, and requires separate approval of the exact matrix digest.
Unapproved constants stay local. This broadens the former single transform;
it does not introduce float quantization or arbitrary shader approval.

The [affine checkpoint](qualification/affine-checkpoint.json) records a separate
native image with 32 verified exchanges / 262,144 MACs across the old and new
Vulkan/OpenGL fixtures. Each of four programs checks 2,048 outputs. Both Vulkan
frames match independent CPU references. Corrupting actual worker reply 33
causes OpenGL context loss and service retirement; OpenGL/Vulkan retries make
no additional worker exchange. All 14 native lifecycle checks pass.

The same boot passes Firefox WebGL 1/2 and encrypted Moonlight streaming:
1280×800 H.264, 29.93 FPS received/decoded, 29.83 displayed in the short test.
The image SHA-256 is
`f473f553d0a4a5b29e205fd5d8af449bd4e5bc9e2d1dfe5c818736792ecb4b71`;
dm-verity root is
`ffaae3cfbb40e61a4bdd39fe5898634288cabe57fdf58dd9b62e952fa04e6b54`.
Vulkan library SHA-256 is
`aa600783f4e5d7e8763a5b0159fb90309d8deb19d9ece27ed9d5d6f266fc95a8`.
Gallium remains the previously qualified build. This is a separate test image,
using synthetic owner admission and public fixture policies; it has not replaced
the user's VM or established production attestation.

Validation includes 32 baseline and 41 generalized compiler cases, the 25-case
original driver regression suite, eight affine driver cases, and 1,028 new
ASan/UBSan client cases plus the original parser suite. Exact matrix identity is
checked in the service-side test receipts. All 15 patched upstream Mesa files
reproduce the tested source. Offline proof normalization differs from runtime;
actual driver tests separately establish runtime eligibility for their fixtures.
The scoped independent Claude review found no confirmed defect in the reviewed
compiler/client/runtime delta; it did not review cryptography or the platform.

These are correctness and isolation results. The device still reports CPU;
ordinary floating-point rendering and video encoding remain local. There is no
measured speedup or general protected game-rendering claim. Earlier checkpoints
below retain their original hashes and narrower acceptance scope.

## What has been demonstrated

Using Mesa 25.0.7-2+deb13u1 from the guest's Debian packages, an isolated
container derived from the desktop image reported:

- Vulkan device API 1.4.305, device type CPU, driver llvmpipe/Lavapipe.
- OpenGL 4.6 core and compatibility profiles through Zink on that Vulkan ICD.
- OpenGL 4.5 through llvmpipe directly.
- A rendered, textured Vulkan cube and rendered OpenGL gears; captured images
  contained varied pixels and were visually inspected.

The test has no network, host display or physical GPU device. It runs as a
non-root user with two CPU cores, a 2-GiB container memory limit, a read-only
filesystem and temporary session storage. These are **container reference
results**, not native VM qualification, hardware acceleration, graphics API
conformance testing, or Steam game compatibility results. The driver's
reported memory total is not a measurement of the container's memory budget.

## Earlier software-only native checkpoint

The separate graphics root image now boots under the real native supervisor
and nsinit in KVM with the existing desktop role policy. As UID 1000, with all
capabilities dropped, no-new-privileges enabled and seccomp active, it renders
Vulkan cubes and Zink OpenGL gears on both an isolated test display and the
actual 1280×800 Xorg desktop. No physical GPU node is present.

The [compute-to-draw fixture](qualification/README.md) also passes inside that
VM: eight Vulkan dispatches check 2,048 integer values, preserve buffer guards,
and feed a rendered triangle. Its final frame is byte-identical to both the
Debian reference and an upstream source-built Mesa 25.0.7 driver. Existing CLI,
CPU llama, desktop lifecycle, VNC RFB and Sunshine server-info tunnel checks pass.
These checks use a synthetic worker and a test runtime adapter. They do not
qualify production admission, GPU execution or Moonlight video streaming.

The root squashfs SHA-256 is
`6ca695df864f610993ef0454055daee81200a68ae81830510cca0d68c29a5ba5`,
with dm-verity root
`bea25bcec2365087b1e3e5ca02933776fe4a50e584118a13b5d72e2fad24a95c`.
This is a separate development image; no running user VM was replaced.


## Earlier exact-module native checkpoint

A second separate image includes the patched ICD and a measured nsinit build
with `KVMBOX_SHIELD_GRAPHICS`. Desktop applications inherit the Vulkan/Zink
selection. The native role (UID 1000, no capabilities, NNP, seccomp, no physical
GPU nodes) renders Vulkan and OpenGL on the 1280×800 desktop. Its standard
Vulkan fixture makes eight verified GPU exchanges / 65,536 MACs through the
existing Shield service, fixed guest-vsock proxy and dedicated loopback worker.
All 2,048 checked values and the complete triangle frame match the reference.
Thirteen graph checks also cover CLI/CPU inference, VNC, Sunshine server-info,
clean stop, restart and supervisor-death teardown. A further KVM run corrupts
an actual GPU reply: the driver reports device loss, publishes no output,
skips the later draw and refuses retry without another worker connection.
Both OpenGL and Vulkan clients map the pinned patched Vulkan library.
A standard OpenGL compute fixture also checks 2,048 exact outputs and SSBO
guards inside that VM; its Zink-generated shader currently remains local.

Root squashfs SHA-256:
`2b3f3969cf5927895c2d33bdd5548ded9b5cf631be2f2d2d0db9f8c9b86b8c6c`.
Verity root:
`b60655da14252a5c18e79d55657fd8ec23f9f4c9e252479ae7b68c5a1c3449c1`.
The guest's `json-c` runtime is a normal measured Debian package. Matching
virtio-vsock modules are in the test initrd. The original images and running
VMs were preserved. Production runtime admission, hardware attestation,
Moonlight video and general shader acceleration remain unqualified.

## Structural OpenGL/Vulkan checkpoint

The current source replaces byte matching with a bounded conditional NIR proof
for equivalent shaders and Zink's descriptor-array/scalar-store representation.
The standard OpenGL and Vulkan fixtures both use the masked route. CPU fallback
remains available for unsupported semantics, layouts and API features. The
separate desktop selects the measured Mesa libraries and lazy Zink descriptors;
physical GPU nodes remain absent.

Compiler admission has 32 positive/negative cases. The real-ICD regression suite
has 25 cases, including OpenGL query accounting, descriptor-buffer fallback and
actual worker corruption. Zink now propagates asynchronous Vulkan device loss
to robust GL reset notification; subsequent commands report context loss.
Shared loss flags use atomic accesses. Temporary proof/layout diagnostics have
been removed. Independent compiler and driver reviews found no confirmed
unsound admission; their scope excludes the underlying cryptographic proof.

The native qualification checks 16 successful protected exchanges / 131,072
MACs across both APIs, then corrupts the seventeenth response. OpenGL loses its
context, and OpenGL/Vulkan retries fail without an eighteenth GPU exchange.
Process maps verify both private Vulkan and Gallium libraries. Exact candidate
hashes are recorded in [the qualification checkpoint](qualification/checkpoint.json)
and with the exported evidence.
This remains a test-adapter image with synthetic public-matrix approval;
production attestation/admission and full-frame acceleration are not established.

## Firefox/WebGL acceptance

The browser launcher now honors the graphics image's inherited Zink selection
instead of overriding it with direct llvmpipe. It keeps its one-thread limit
and the existing lightweight Firefox profile. In the actual 2-vCPU/8-GiB KVM
desktop, the installed browser passes complete-frame texture/depth/blending
checks in both WebGL 1 and WebGL 2 and runs the controlled 512-triangle animation on a 640×360 canvas
at approximately 30 FPS. Both private Mesa library hashes are confirmed in the
browser process. Worker counters remain unchanged: this workload uses CPU
rendering, with no hardware-acceleration claim.

The same image passes the protected OpenGL/Vulkan prepass and corruption/retry
checks. Exact image hashes, browser version, profile identity and measured
frame times are in [the browser checkpoint](qualification/browser-checkpoint.json).
This is application acceptance for a bounded workload, not API conformance,
Steam compatibility or a long-duration performance result.

## Native Moonlight video acceptance

The same separate browser-qualified image now streams the actual Vulkan cube
and Zink OpenGL gears through Sunshine and the unmodified owner-side Moonlight
adapter. An isolated Moonlight 6.2.0 client receives H.264 at 1280×800, requested
30 FPS and 5 Mbps. The final short run measured 29.93 FPS received/decoded and
29.83 FPS displayed, with 0% network loss and 0.33% jitter drops. Captured frames
were visually inspected; a gears-region comparison independently confirms
animation outside the statistics overlay. Sunshine captures X11 and encodes on
the VM CPU. The client uses software decoding and the SDL software frontend
with a DRM buffer backend; it is not a device-isolated client test.

Both scene processes map the pinned Vulkan library, and gears also maps the
pinned Gallium library. The same boot passes Firefox/WebGL, 16 verified masked
compute exchanges, corruption/retry checks, and all 14 native graph checks,
including clean stop, restart and supervisor-death teardown. See the
[streaming checkpoint](qualification/moonlight-checkpoint.json).

This qualification uses synthetic owner credentials, a fixture TLS CA, and a
test vsock relay into the existing authenticated gateway. Certificate checks
remain enabled; fixed application ports and guest role policies are unchanged.
It establishes application/video functionality, not production attestation or
owner admission. The user's existing Moonlight pairing and VM remain unchanged.
Audio packets reached a dummy sink; audio fidelity, game/controller input,
WAN performance and long-duration behavior remain unqualified.

## Reproduce

Build from an immutable local desktop image ID. The output directory must be
new. The builder creates a content-named local alias for BuildKit and records
the resulting image ID, source hashes and build log. It never changes a
running VM or publishes an app.

```sh
python3 reference.py build --image sha256:BASE_IMAGE_HASH --output /absolute/work/build
python3 reference.py probe --image sha256:REFERENCE_IMAGE_HASH --output /absolute/work/probe
```

The probe pins the Lavapipe ICD, refuses physical GPU device nodes, exercises
real Vulkan and OpenGL clients, and saves driver reports, package versions,
PNG frames and a receipt. Xvfb runs under an init process so its readiness
signal reaches its launcher correctly. Disposable probe containers are
removed even if the client times out.

The current development Dockerfile resolves packages from the base image's
APT sources. The installed versions are recorded; this is not yet a fully
reproducible release recipe. A release must pin the package artifacts or
build sources and include the driver in a new authenticated root image.

## Preserve the native desktop when packaging

The VM base contains custom files added after its container image was built.
Using a plain container export as the native base loses assets such as
`/xorg.conf`; the first graphics packaging attempt failed Xorg startup for
that reason. The native packager now refuses missing required base assets.

Export the original immutable container image and its graphics-derived image
to separate directories. Compose their package difference onto the customized
VM filesystem before passing it to the native root-image builder:

```sh
python3 merge-native-root.py --base /work/original-image-export \
  --graphics /work/graphics-image-export --customized /work/native-desktop-root \
  --output /work/combined-root --receipt /work/composition.json
```

The composer rejects conflicting customizations, symlink-parent traversal and
directory removals that discard custom assets. It verifies the resulting tree
and that all input trees stayed unchanged. The executed merge contained 141
changed paths and no conflicts. Inputs must be immutable development exports.

## Building the driver from source

`mesa-source.json` pins the upstream 25.0.7 archive and its official checksum.
`mesa-tools.Dockerfile` supplies a separate Debian build environment;
`build-mesa.sh` builds Lavapipe, LLVMpipe and Zink with four compiler jobs.
Run it in that tools container with the verified extracted source mounted
read-only at `/source` and a new writable directory at `/build`. Installed
files are written under `/build/install/opt/shield/mesa`. No host driver is
replaced. The development tools image still resolves APT dependencies at build
time; record its immutable image ID and package versions for each qualification.

## Remaining work

1. Qualify broader application workloads, game/input/audio behavior and
   owner Moonlight acceptance after each image update. The deployed v13
   desktop has owner-verified usable streaming; isolated native API/rendering
   checks also pass. Keep the reference renderer as the trusted fallback.
2. Expand the protected operation coverage beyond the structurally proven
   bounded integer affine family. Existing public
   model policies do not authorize arbitrary scene coefficients or textures.
3. Validate numeric semantics and leakage for each accelerated operation;
   preserve trusted handling of private visibility, texture addressing,
   framebuffer composition and encoding where no protected lowering exists.
4. Compare complete frames against the reference, inject malformed worker
   replies, and measure frame times and masking costs on the target VM.
5. Exercise real applications and compatible games, run relevant API
   conformance tests, package a separate measured image, and complete the
   per-release production admission and owner acceptance. The native app
   is already published; a new candidate must not inherit old acceptance claims.

CPU rendering is a functional reference and fallback. It does not satisfy the
requested masked hardware acceleration by itself. The threat model and
unproven parts of the graphics design are recorded in
[the protected graphics note](../shield-compute/GRAPHICS.md).

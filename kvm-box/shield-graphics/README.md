# Standard graphics driver work

The requested endpoint is usable OpenGL/Vulkan in the native desktop, with
physical GPU work restricted to the masked channel and unsupported work kept
inside the VM. This directory starts the graphics reference and acceptance
path. A [protected OpenGL/Vulkan prepass](driver/README.md) now passes real
worker tests through a patched Mesa ICD in a host container and the actual
native KVM desktop. General masked rendering remains incomplete.

The desktop base contains Mesa's software OpenGL implementation and a Vulkan
loader, but lacks a Vulkan ICD. The separate development reference adds Mesa
Lavapipe and tests OpenGL over Vulkan using Zink. Keeping both APIs on one
renderer gives future protected compiler work a common reference path.
[Mesa documents Zink's Vulkan requirements](https://docs.mesa3d.org/drivers/zink.html).

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

1. Qualify Moonlight video streaming with the graphics image and broader
   application workloads. Native desktop API/rendering checks now pass; keep
   the reference renderer available as the trusted fallback.
2. Expand the protected operation coverage beyond the structurally proven
   fixed integer prepass. Existing public
   model policies do not authorize arbitrary scene coefficients or textures.
3. Validate numeric semantics and leakage for each accelerated operation;
   preserve trusted handling of private visibility, texture addressing,
   framebuffer composition and encoding where no protected lowering exists.
4. Compare complete frames against the reference, inject malformed worker
   replies, and measure frame times and masking costs on the target VM.
5. Exercise real applications and compatible games, run relevant API
   conformance tests, package a separate measured image, and complete the
   original desktop acceptance and publication work.

CPU rendering is a functional reference and fallback. It does not satisfy the
requested masked hardware acceleration by itself. The threat model and
unproven parts of the graphics design are recorded in
[the protected graphics note](../shield-compute/GRAPHICS.md).

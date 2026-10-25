# Standard graphics driver work

The requested endpoint is usable OpenGL/Vulkan in the native desktop, with
physical GPU work restricted to the masked channel and unsupported work kept
inside the VM. This directory starts the graphics reference and acceptance
path. It does **not** implement masked hardware rendering yet.

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

## Remaining work

1. Qualify the standard APIs inside the native VM, including its actual
   desktop display, application sandbox and Moonlight stream. Keep the
   reference renderer available as the trusted fallback.
2. Integrate an explicitly approved masked lowering into the graphics shader
   execution path. The compute channel's existing public Q8 model policies
   do not authorize arbitrary scene coefficients, textures or shaders.
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

# Protected graphics follow-up

Status: staged implementation, 2026-10-25. A bounded integer affine compiler for OpenGL/Vulkan
now executes through the existing masked channel in a patched Mesa ICD under
host-container and native KVM tests; see the [driver contract](../shield-graphics/driver/README.md).
The actual VM qualifies the software reference and both the original and generalized
protected compute-to-draw paths. General protected rendering remains open.

The user has now explicitly requested work through a usable OpenGL/Vulkan
driver. The separate [graphics reference](../shield-graphics/README.md) adds a
Vulkan software ICD to the desktop-derived development image and demonstrates
OpenGL through Zink plus real Vulkan/OpenGL frames in an isolated container
and on the actual desktop inside KVM under the native application policy.
A compute-to-draw fixture also passes with identical reference frames.
Firefox/WebGL and isolated Moonlight video acceptance also pass on the separate
native image, including restart and teardown. Broader protected lowering and
production admission/transport qualification remain outstanding. It does not
complete the full masked hardware graphics goal.

A hybrid renderer is a possible research direction under the existing
untrusted-GPU threat model. Selected linear work with explicitly public
coefficients could be transformed into a vetted masked operation. Private
visibility decisions, texture access, unsupported shaders, the framebuffer and
encoding would remain inside the VM. This is an architectural inference from
the existing compute boundary. The generalized compiler extracts approved bounded matrix products and keeps
modular bias/row terms local. It demonstrates a limited integer subset, with no
measured performance gain.

Encrypting a command stream or video connection does not hide plaintext from a
GPU that must decrypt and render it. Masking a matrix product also does not
provide a drop-in transformation for an arbitrary graphics pipeline. Vulkan
separates rasterization, shader invocation, coverage, tests and framebuffer
operations; their data-dependent behavior needs its own treatment. See the
[Khronos rasterization specification](https://docs.vulkan.org/spec/latest/chapters/primsrast.html)
and [fragment operations specification](https://docs.vulkan.org/spec/latest/chapters/fragops.html).

## What a compatible implementation would require

- A graphics frontend/driver in the VM with a software reference path. The
  existing OpenCL device is not an OpenGL or Vulkan graphics implementation.
- Compiler lowering only for operations with a proven masking and verification
  contract. The current Q8 public-tensor policies approve model weights, not
  arbitrary camera matrices, textures or graphics coefficients. New policies
  and numeric contracts would require separate validation.
- Explicit classification of public coefficients and dispatch metadata. A
  camera transform or scene asset is not automatically public. Private geometry
  or visibility must not determine externally visible work without an accepted
  leakage policy, padding or another data-oblivious protocol.
- Trusted handling of raster coverage, depth/stencil decisions, private texture
  lookups, composition and encoding wherever no vetted protected lowering
  exists. Ordinary hardware encoding of private frames on this untrusted GPU
  would violate the requested boundary.
- Fail-closed output publication, fresh masks, bounded resource use, rejection
  of malformed worker replies and local reference comparisons at shader,
  primitive and complete-frame levels.
- Actual frame-time measurements. Small transforms can cost more to mask and
  transfer than to compute locally. Neither faster games nor an accelerated
  Firefox desktop follows from the present compute tests.

An attested confidential GPU is a different architecture: it extends the
trusted boundary to GPU hardware and memory. NVIDIA describes that model in
its [confidential VM documentation](https://docs.nvidia.com/confidential-computing/virtual-machines/dev).
It must not be substituted for the user's current choice to treat the GPU as
untrusted. Hardware support and graphics API support also require separate,
version-specific validation; a confidential-compute label alone is insufficient.

Moonlight transport protection is a separate concern from protecting rendering
on the GPU. Existing stream functionality does not constitute evidence that
private frames can be processed securely by an untrusted hardware renderer or
encoder. No such claim is made for this release.

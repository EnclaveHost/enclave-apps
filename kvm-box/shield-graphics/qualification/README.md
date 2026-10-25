# Vulkan compute-to-draw qualification

This standard Vulkan application submits an integer compute shader, a memory
barrier, and a draw using the computed buffer as integer vertex attributes.
It reads back the rendered frame. It has no Shield service calls and never
opens a worker connection. The application requires a CPU Vulkan device so a
raw physical GPU cannot silently satisfy this reference test.

Eight dispatches check 2,048 scalar outputs against an independent integer
reference, including low-byte clamp extrema and nonzero high input bits.
Input contents and output guards must remain unchanged outside the specified
writes. The final 128×128 RGBA frame has 1,620 colored pixels.

Build in the Mesa tools image with `OUTPUT=/output/build sh /source/build.sh`.
The output directory must be new. Run `prepass SHADER_DIRECTORY NEW_RGBA_FILE`
with `VK_DRIVER_FILES` pinned to the Lavapipe ICD. The tool uses ordinary Vulkan
loader calls, SSBO descriptors, a compute pipeline, a graphics pipeline and a
fence; it does not select or implement a masked route.

The Debian Mesa 25.0.7 reference, an upstream Mesa 25.0.7 source build, and the
native KVM guest produced identical final RGBA bytes. This establishes a
reference for later protected lowering. Those initial reference runs demonstrate no GPU offload or speedup. The
[patched ICD](../driver/README.md) subsequently offloaded the prepass through
Shield with identical outputs. This is not a graphics API conformance suite.

The public synthetic transform is `(x+y, y-x, z, w)` after each source word is
masked to its low byte and shifted by −128. A future compiler must prove those
semantics and the runtime buffer layout before replacing it. Approval of this
published test matrix does not authorize private scene coefficients, camera
matrices or other shaders. Driver-side execution currently requires the exact qualified module.


## Regression fixtures

Run `python3 build-variants.py --output /output/variants` inside the pinned
Mesa tools image. The generated ordinary Vulkan applications test base and
device-generated dispatch, specialization, dynamic descriptors, wider ranges,
and pipeline-statistics queries. Failure variants use infinite fence/timeline
waits, a waiter started before submission, and queue/device idle waits.

`run-driver.py` runs each case in a disposable container with no physical GPU
nodes. Supply immutable `--image`, `--mesa` (installed prefix), `--sdk`, and
`--output` paths. The output directory must contain `build/`, `variants/`, and
`triangle.rgba` from the unmodified reference ICD. Each case creates new logs
and refuses to overwrite evidence. Only `masked` and `worker-corrupt` enable
host networking, solely to proxy to the dedicated loopback `--worker-port`;
the Vulkan app receives only the guest-local service socket. For example:

```sh
python3 run-driver.py --image sha256:TOOLS_HASH --mesa /work/mesa-install \
  --sdk /work/native-sdk --output /work/qualification masked
```

For `unmatched`, copy the compiled shaders into `build-unmatched/`, replace a
same-length `OpName` debug string in the compute module, and validate it with
`spirv-val --target-env vulkan1.1`. This case intentionally remains local.
The service and GPU policy are synthetic fixtures; successful tests do not
establish production admission or attestation.


## OpenGL compute reference

`prepass-gl.cpp` and `prepass-gl.comp` exercise ordinary OpenGL compute through
Zink on the pinned Vulkan ICD. Build with `OUTPUT=/output/gl sh /source/build-gl.sh`
in the tools image, then run `prepass-gl prepass-gl.comp` on the desktop display.
It checks 2,048 outputs across eight dispatches, SSBO alignment, untouched guards
and unchanged input using independent integer arithmetic. It requires a Zink
llvmpipe renderer and claims no GPU offload.

This fixture passes both in the isolated container and under the actual KVM
desktop role, together with the Vulkan protected prepass and worker-corruption
checks. The current exact-module driver keeps Zink's generated compute shader
local. Its generated SPIR-V uses a set-3 descriptor array and four scalar stores;
structural proof and runtime descriptor admission must support that representation
before OpenGL compute can use the masked route.

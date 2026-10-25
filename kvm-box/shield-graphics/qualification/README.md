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
masked to its low byte and shifted by −128. The structural compiler proves those
semantics, and each dispatch revalidates the runtime buffer layout before replacement. Approval of this
published test matrix does not authorize private scene coefficients, camera
matrices or other shaders. Equivalent compiled modules may qualify; unknown semantics stay local.


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
and refuses to overwrite evidence. Only the masked, equivalent-module and worker-corruption cases enable
host networking, solely to proxy to the dedicated loopback `--worker-port`;
the Vulkan app receives only the guest-local service socket. For example:

```sh
python3 run-driver.py --image sha256:TOOLS_HASH --mesa /work/mesa-install \
  --sdk /work/native-sdk --output /work/qualification masked
```

Use `build-admission-fixtures.py --output /output` in the tools image to create
`build-equivalent/` (renamed but equivalent shader) and `build-unmatched/`
(explicit bound-check control flow outside the supported proof). The equivalent
case uses the masked route; unmatched remains local. Both use validated SPIR-V.
The service and GPU policy are synthetic fixtures; successful tests do not
establish production admission or attestation.


## OpenGL compute reference

`prepass-gl.cpp` and `prepass-gl.comp` exercise ordinary OpenGL compute through
Zink on the pinned Vulkan ICD. Build with `OUTPUT=/output/gl sh /source/build-gl.sh`
in the tools image, then run `prepass-gl prepass-gl.comp` on the desktop display.
It checks 2,048 outputs across eight dispatches, SSBO alignment, untouched guards
and unchanged input using independent integer arithmetic. It requires a Zink
llvmpipe renderer and claims no GPU offload.

The structural driver also proves Zink's generated set-3 descriptor array and
four scalar stores. `gl-masked` checks actual worker/service counters; the app
itself makes no offload claim because standard OpenGL does not expose them.
Copy `prepass-gl` and `prepass-gl.comp` into the driver output directory. Run
`gl-local`, `gl-masked`, `gl-unavailable`, `gl-queries`, `gl-wide-range`,
`gl-descriptor-buffer`, and `gl-worker-corrupt` with `run-driver.py`.

`--expect-device-lost` creates a robust context, checks reset notification and
`GL_CONTEXT_LOST` on a subsequent command, and skips readback. `--queries` checks
64 compute invocations per dispatch; `--wide-range` exercises local fallback.
The harness selects the private Mesa libraries and lazy Zink descriptors;
the descriptor-buffer test overrides that selection and must stay local.

`test-nir.py` is the offline compiler admission suite. Prepare the source/tool
as described in the driver README. It compiles and validates every SPIR-V input,
then checks acceptance, named contract and rejection reason. These tests are
independent of the real-ICD/GPU execution checks and do not replace them.

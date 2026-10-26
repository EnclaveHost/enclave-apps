# Q8 OpenCL lowering: admission and execution evidence

Status as of 2026-10-24: experimental prerequisites implemented and tested.
Compiler plans alone remain non-dispatchable. Runtime capture and explicit
masked execution are now integrated experimentally; see `Q8_CAPTURE.md` and
`Q8_MASKED.md` for their separate evidence. The deployed 0.6.8 image
and default build recipes do not include these experimental modules.

## Compiler and admission boundary

`q8_kernel_compiler.py` recognizes the typed structure of the two pinned Qwen
Q8/float kernels. Function and variable renaming is allowed. Arithmetic,
indices, casts, signedness, vector selectors, subgroup size, stores and calls
remain part of the comparison. Unrecognized AST operations and attributes,
source function redeclarations, global storage, altered build flags and
unapproved preprocessing directives are refused. Comments are removed before
checking directives; alternate directive spellings, escapes, token-pasting,
stringizing and pragma operators are declined. This is conservative recognition,
not a general CUDA/OpenCL compiler or a formal equivalence proof.

The analysis result always has `dispatchable: false`. Structural recognition
does not authorize public disclosure of weights, approximate floating-point
arithmetic, or any GPU transaction. The existing integer compiler is unchanged
apart from an optional internal AST build-flags parameter with the same default.

`q8_admission.py` validates an initial contiguous, non-batched subset of both
19-argument ABIs. It checks scalar ranges, strides, broadcasting factors,
complete zero-offset grids, explicit local sizes, byte offsets, alignment,
allocation bounds and disjoint output storage. Root allocation identity is used
to catch aliases between different sub-buffer handles. Rounded tile indexing is
bounded before signed arithmetic can overflow. Matrix-vector launches require
eight complete output rows per workgroup because the original kernel loads
four rows per subgroup before testing its output bounds.

The experimental C++ caller now validates original scalar ABI widths, obtains
buffer metadata from the runtime, waits for dependencies and captures actual
contents; see `Q8_CAPTURE.md`. The Python descriptor API is not a client authorization
protocol and must never trust client-supplied root identities or sizes.

`PublicQ8Catalog` binds policy bytes to the service's configured policy, model
and calibration digests. Exact immutable code and scale snapshots are hashed
in original GGUF block order: two half-scale bytes followed by 32 code bytes.
No float conversion or requantization occurs during authentication. Tensor
names, dimensions, lengths and bytes must all match the approved public tensor.
Previous success does not authorize a later mutable buffer. The eventual
executor must consume these exact snapshots. Expected policy digests must come
from measured configuration, never from an application request; service startup
must still verify model and calibration file contents.

## Executed checks

The final compiler/admission regression run passes 23 tests. It includes changes
to arithmetic, stores, vector lanes, subgroup attributes, preprocessing, shapes,
strides, grids, aliases, alignment, integer limits, model bytes and policy
bindings. The Q8 byte reconstruction test spans multiple chunks, every half
bit pattern and every signed-byte code representation. This verifies exact byte
preservation, not that all those half encodings are suitable for numerical work.

`probe_q8_admission.py` independently parses the actual GGUF artifacts using
GGUFReader, verifies their configured model digests and authenticates snapshots
against the policies extracted from `enclave-kvm-box-desktop:0.6.8-prompts`:

| Model artifact | Authenticated Q8 tensors | Synthetic MM/MV layouts admitted | Declined layouts |
| --- | ---: | ---: | ---: |
| Qwen 0.5B Q8 | 170 | 340 | 0 |
| SD 1.5 Q8 | 955 | 1,212 | 698 |

Both artifacts reject mutated code bytes and mutated scale bytes after an
earlier successful authentication. The SD declines are matrix-vector row tails,
including 77-row position embeddings and one-row bias/normalization tensors.
These synthetic launches do not assert that those tensors are actually used as
matrix operands by SD. The initial SD probe stopped on the first declined
layout; its failed log is preserved. The revised probe reports admission and
declines separately. No model bytes or numerical settings were changed.

`test_q8_kernels.cpp` executes the exact original kernels through the corrected
CPU PoCL runtime in a networkless container without GPU devices. Six shapes,
each in strict and original fast-math modes, produce 18,694 exact outputs with
intact output guards. Cases include partial matrix tiles, 32/96/896-wide inner
dimensions, multiple input rows and nonzero input/output offsets. An independent
double scalar reference uses dyadic values for which intermediate arithmetic
is exact. This proves the tested indexing/results, not general float accuracy.

`probe_q8_kernels.py` requires the actual loaded ICD file's SHA-256, runs that
binary, then replays all 12 recorded launches through `validate_launch` and
checks output coverage. The tested PoCL library digest is
`90e4f6a7bbbe32d6170ca9575f3ad2630d31cd21fe3427e63dcb0faf881bb2ee`.
Mount the rebuilt library at `/opt/shield/pocl/lib/libpocl.so.2`; mounting only
the versioned `.so.2.13.0` filename does not replace this image's active ICD.

## Reproduction and remaining implementation

In the isolated clang-18 builder, run:

```sh
python3 -m unittest -v test_q8_kernel_compiler test_q8_admission test_kernel_compiler
c++ -std=c++17 -O2 test_q8_kernels.cpp -lOpenCL -o test-q8-kernels
```

Run the two `probe_q8_*.py` scripts with `--help` for their inputs. The model
probe needs the pinned llama.cpp `gguf-py` on `PYTHONPATH`, packaged service and
policy files, and read-only model mounts. The execution probe needs the
corrected PoCL library and half-math bitcode mounted into the desktop image.
Use no network/GPU devices; builds/tests were limited to 1–2 CPUs and 2 GiB RAM.

Runtime argument/storage capture is now tested separately in `Q8_CAPTURE.md`.
Next: connect authenticated snapshots to the verified masked executor and enforce an
explicit numerical contract for any approximate lowering. Native float kernels
must not be silently quantized. Once a protected transaction begins, failures
must retain the existing fail-closed/retirement behavior. End-to-end Qwen and SD
then need actual standard-API execution, fault tests, quality acceptance and
performance measurements. CPU execution and admission tests do not prove that.

Full API/compiler coverage, protected graphics, production identity binding,
publication assets, wallet/nucbox deployment and removal of the old compute
menus remain open. Protected graphics follows compute: eligible shader
arithmetic may use masking, while sensitive sampling, visibility, framebuffer
assembly and encoding remain trusted until separately implemented and verified.

# Pinned Q8 kernel references

Copied byte-for-byte from llama.cpp commit
`ddd4ec1428a6201e18975ea52b07c71e0f9aef26`, directory
`ggml/src/ggml-opencl/kernels/`. The upstream MIT license is in `LICENSE`.

| File | SHA-256 |
| --- | --- |
| `mul_mm_q8_0_f32_l4_lm.cl` | `e9dee2e88bf65fc8645b048e84a0e6bead25f99878d83989dbb07474f76b5c30` |
| `mul_mv_q8_0_f32_flat.cl` | `6e67804f54c5df29988e25980b47d696a09fd05371068482729f191253ee2abb` |

These are structural compiler references and execution-test inputs, not an
authorization list based on function names. See `../Q8_LOWERING.md`.

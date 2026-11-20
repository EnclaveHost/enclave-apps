# Pinned Q8 kernel references

Copied byte-for-byte from llama.cpp commit
`ddd4ec1428a6201e18975ea52b07c71e0f9aef26`, directory
`ggml/src/ggml-opencl/kernels/`. The upstream MIT license is in `LICENSE`.

| File | SHA-256 |
| --- | --- |
| `mul_mm_q8_0_f32_l4_lm.cl` | `e9dee2e88bf65fc8645b048e84a0e6bead25f99878d83989dbb07474f76b5c30` |
| `mul_mv_q8_0_f32_flat.cl` | `6e67804f54c5df29988e25980b47d696a09fd05371068482729f191253ee2abb` |
| `mul_mm_q4_k_f32_l4_lm.cl` | `12f6e45a94a64a9f085b0ac99dfd000a2351aa9861ae5ccbd5acf7dda47201d5` |
| `mul_mv_q4_k_f32_flat.cl` | `15a59243fabf94d4dc33a98fa70e61d0adee8338bfb60a827b2e63c95034351e` |

The two Q4_K files are recognised by the structural analysis only. Nothing
admits, captures or executes their launches yet.

These are structural compiler references and execution-test inputs, not an
authorization list based on function names. See `../Q8_LOWERING.md`.

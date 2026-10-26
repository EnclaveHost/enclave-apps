#!/bin/sh
set -eu
# Inside the HIP builder, with the installed pinned toolchain, source at /src
# and a private writable /build. The reference comes from audited CUDA source,
# never from the candidate application or its kernel name.
/opt/shield/hip/bin/cucc -c /src/gemm_i8_cuda.cu -o /build/gemm-reference.o
llvm-objcopy-18 --dump-section=.hip_fatbin=/build/gemm-reference.bundle /build/gemm-reference.o
clang-offload-bundler-18 --unbundle --type=bc --targets=hip-spirv64----generic \
    --input=/build/gemm-reference.bundle --output=/build/gemm-reference.spv
llvm-spirv-18 -r /build/gemm-reference.spv -o /build/gemm-reference.bc
sha256sum /src/gemm_i8_cuda.cu /build/gemm-reference.spv /build/gemm-reference.bc \
    > /build/gemm-reference.sha256

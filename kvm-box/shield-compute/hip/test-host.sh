#!/bin/sh
set -eu
# The caller supplies only the CPU ICD and Shield frontend. No host GPU device
# or worker endpoint is required: these tests exercise CPU execution, including
# the integer GEMM's explicit non-offloadable K=31 shape.
export CHIP_BE=opencl CHIP_DEVICE_TYPE=gpu CHIP_LOGLEVEL=crit
for device in /dev/dri /dev/kfd /dev/nvidia*; do
    if test -e "$device"; then
        printf 'Unexpected physical GPU device in CPU fallback test: %s\n' "$device" >&2
        exit 1
    fi
done
mkdir -p /tests
cd /tests
/opt/shield/hip/bin/hipcc /input/samples/0_MatrixTranspose/MatrixTranspose.cpp \
    -o /tests/matrix-transpose > /tests/matrix-transpose-build.log 2>&1
timeout 90 /tests/matrix-transpose > /tests/matrix-transpose-run.log 2>&1
printf '%s\n' 'HIP matrix transpose: passed'
for name in vectorAdd matrixMul asyncAPI; do
    /opt/shield/hip/bin/cucc -I/input/samples/cuda_samples/Common \
        "/input/samples/cuda_samples/0_Simple/$name/$name.cu" \
        -o "/tests/$name" > "/tests/$name-build.log" 2>&1
    timeout 90 "/tests/$name" > "/tests/$name-run.log" 2>&1
    printf 'CUDA %s: passed\n' "$name"
done
/opt/shield/hip/bin/cucc /frontend/test_cuda.cu -o /tests/cuda-gemm \
    > /tests/cuda-gemm-build.log 2>&1
timeout 90 /tests/cuda-gemm cpu-compat > /tests/cuda-gemm-run.log 2>&1
printf '%s\n' 'CUDA integer GEMM: passed'
/opt/shield/hip/bin/hipcc /frontend/test_math.cpp -o /tests/hip-math \
    > /tests/hip-math-build.log 2>&1
timeout 90 /tests/hip-math > /tests/hip-math-run.log 2>&1
printf '%s\n' 'HIP host/device pi-scaled math: passed'

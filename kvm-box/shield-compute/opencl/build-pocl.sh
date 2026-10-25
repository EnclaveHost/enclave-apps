#!/bin/sh
set -eu
# /input is a pristine export of PoCL v6.0 commit
# 952bc559f790e5deb5ae48692c4a19619b53fcdc. /build is private build space.
test -f /input/lib/CL/pocl_tracing.c
mkdir -p /build/source
cp -a /input/. /build/source/
cd /build/source
patch --forward -p1 < /frontend/pocl-callback-errors.patch
cmake -S /build/source -B /build/objects -G Ninja \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/shield/pocl \
    -DWITH_LLVM_CONFIG=/usr/bin/llvm-config-18 \
    -DENABLE_ICD=ON -DENABLE_TESTS=OFF -DENABLE_EXAMPLES=OFF \
    -DENABLE_HOST_CPU_DEVICES=ON -DENABLE_CUDA=OFF -DENABLE_VULKAN=OFF \
    -DENABLE_HSA=OFF -DENABLE_ALMAIF_DEVICE=OFF \
    -DENABLE_REMOTE_CLIENT=OFF -DENABLE_REMOTE_SERVER=OFF \
    -DENABLE_SPIRV=ON -DLLVM_SPIRV=/usr/bin/llvm-spirv-18 \
    -DKERNELLIB_HOST_CPU_VARIANTS=native -DLLC_HOST_CPU=x86-64
cmake --build /build/objects -j "${JOBS:-1}"
rm -rf /build/package
DESTDIR=/build/package cmake --install /build/objects
cp /package-versions.txt /build/package/opt/shield/pocl/build-package-versions.txt
cp /input/LICENSE /build/package/opt/shield/pocl/LICENSE

#!/bin/sh
set -eu
# Run in the HIP builder with the pristine export at /input, this directory at
# /frontend, and a new writable build directory at /build. Only OpenCL is built.
test ! -e /build/src
test ! -e /build/package
cp -a /input /build/src
cd /build/src
patch -p1 < /frontend/chipstar-glibc.patch
cmake -S /build/src -B /build/obj -G Ninja \
    -DLLVM_CONFIG_BIN=/usr/bin/llvm-config-18 \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/shield/hip \
    -DCHIP_BUILD_TESTS=OFF -DCHIP_BUILD_SAMPLES=OFF \
    -DCHIP_USE_INTEL_USM=OFF -DCHIP_ERROR_IF_NOT_IMPLEMENTED=ON
cmake --build /build/obj -j2
cmake --install /build/obj
mkdir -p /build/package/opt/shield
cp -a /opt/shield/hip /build/package/opt/shield/
package=/build/package/opt/shield/hip
# Upstream installs this generated utility with mode 0750; guest users need
# access to the compiler tools without root privileges.
chmod 0755 "$package/bin/spirv-extractor"
# chipStar embeds the build machine's OpenCL directory in libCHIP's RUNPATH.
# Prefer the packaged Shield frontend without per-process environment overrides.
patchelf --set-rpath /opt/shield/opencl/lib "$package/lib/libCHIP.so"
mkdir -p "$package/share/shield-build"
cp /input/shield-source.json /package-versions.txt "$package/share/shield-build/"
sha256sum /frontend/chipstar-glibc.patch > "$package/share/shield-build/patch.sha256"
sha256sum /frontend/build.sh > "$package/share/shield-build/build-script.sha256"
cp /build/src/LICENSE "$package/share/shield-build/chipStar-LICENSE"
cp /build/src/HIP/LICENSE.txt "$package/share/shield-build/HIP-LICENSE.txt"
cp /build/src/bitcode/ROCm-Device-Libs/LICENSE.TXT "$package/share/shield-build/ROCm-Device-Libs-LICENSE.TXT"

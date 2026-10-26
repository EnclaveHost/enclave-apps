#!/bin/sh
set -eu
export LLVM_CONFIG=/usr/bin/llvm-config-19
meson setup /build/mesa /source --prefix=/opt/shield/mesa --libdir=lib \
 --buildtype=release -Dgallium-drivers=llvmpipe,zink -Dvulkan-drivers=swrast \
 -Dplatforms=x11 -Dglx=dri -Degl=enabled -Dglvnd=enabled -Dgbm=enabled \
 -Dgallium-va=disabled -Dgallium-vdpau=disabled -Dgallium-xa=disabled \
 -Dllvm=enabled -Dshared-llvm=enabled -Dbuild-tests=false -Dvideo-codecs=
ninja -C /build/mesa -j4
DESTDIR=/build/install meson install -C /build/mesa

#!/bin/sh
# Build a RISC Box guest image set out of tree.
#
#   guest/build.sh <buildroot-tree> <build-dir> [defconfig]
#
# defconfig defaults to guest/buildroot.config (Xorg + twm + spincube). Pass
# guest/buildroot-xfce.config for the XFCE desktop instead.
#
# Leaves the two artifacts a deployment boots in <build-dir>/images:
#   fw_payload.elf   (OpenSBI v0.9 + Linux 5.15 Image payload, non-PIC —
#                     built by build-opensbi.sh, not Buildroot; see there)
#   rootfs.ext2      (ext2 root: the desktop the chosen defconfig describes)
set -eu
BR="${1:?usage: build.sh <buildroot-tree> <build-dir> [defconfig]}"
OUT="${2:?usage: build.sh <buildroot-tree> <build-dir> [defconfig]}"
GUEST="$(cd "$(dirname "$0")" && pwd)"
DEFCONFIG="${3:-$GUEST/buildroot.config}"
case "$DEFCONFIG" in /*) ;; *) DEFCONFIG="$(cd "$(dirname "$DEFCONFIG")" && pwd)/$(basename "$DEFCONFIG")" ;; esac

# HOST_CFLAGS: several host tools Buildroot builds (m4's bundled gnulib, most
# visibly) predate C23, where bool/true/false became keywords. GCC 15+ defaults
# to gnu23 and their own headers stop compiling, so pin the host side to gnu17.
# This only affects tools that run on the build machine, never target code.
HOSTFLAGS='-O2 -std=gnu17'

# XFCE is not carried by Buildroot, so those packages come from our own
# BR2_EXTERNAL tree. Harmless for the non-XFCE defconfig, which selects none
# of them.
EXTERNAL="$GUEST/br2-external"

# The defconfig references $(RISCBOX_GUEST) for the overlay/fragment/hooks so
# it works from any checkout location.
make -C "$BR" O="$OUT" BR2_DEFCONFIG="$DEFCONFIG" BR2_EXTERNAL="$EXTERNAL" \
    RISCBOX_GUEST="$GUEST" HOST_CFLAGS="$HOSTFLAGS" defconfig
make -C "$BR" O="$OUT" BR2_EXTERNAL="$EXTERNAL" RISCBOX_GUEST="$GUEST" \
    HOST_CFLAGS="$HOSTFLAGS" -j"$(nproc)"
exec "$GUEST/build-opensbi.sh" "$OUT"

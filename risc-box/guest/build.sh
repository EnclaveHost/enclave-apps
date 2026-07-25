#!/bin/sh
# Build the RISC Box xorg guest image set out of tree.
#
#   guest/build.sh <buildroot-tree> <build-dir>
#
# Leaves the two artifacts a deployment boots in <build-dir>/images:
#   fw_payload.elf   (OpenSBI v0.9 + Linux 5.15 Image payload, non-PIC —
#                     built by build-opensbi.sh, not Buildroot; see there)
#   rootfs.ext2      (ext2: Xorg fbdev + twm + dropbear + spincube)
set -eu
BR="${1:?usage: build.sh <buildroot-tree> <build-dir>}"
OUT="${2:?usage: build.sh <buildroot-tree> <build-dir>}"
GUEST="$(cd "$(dirname "$0")" && pwd)"

# The defconfig references $(RISCBOX_GUEST) for the overlay/fragment/hooks so
# it works from any checkout location.
make -C "$BR" O="$OUT" BR2_DEFCONFIG="$GUEST/buildroot.config" \
    RISCBOX_GUEST="$GUEST" defconfig
make -C "$BR" O="$OUT" RISCBOX_GUEST="$GUEST" -j"$(nproc)"
exec "$GUEST/build-opensbi.sh" "$OUT"

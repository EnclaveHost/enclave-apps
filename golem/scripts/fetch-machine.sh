#!/usr/bin/env bash
# fetch-machine.sh — assemble a golem machine directory from the prebuilt
# QEMU-wasm bundles published by the upstream qemu-wasm-demo (GitHub Pages).
# No Emscripten toolchain needed: the emulator, BIOS pack, kernel, initramfs
# and root disk are downloaded as-is and a machine.json manifest is written
# next to them. Push the result into your golem volume with the
# encrypted-volumes reference script:
#
#   ./fetch-machine.sh alpine-x86_64 ./machines
#   ENCVOL_WALLET_SIG=0x… ../../encrypted-volumes/scripts/enclave-encvol.sh \
#       push ./machines --endpoint https://… --bucket … --path vols/golem --name golem
#
# Machines:
#   alpine-x86_64   Alpine Linux, x86_64 PC (serial console; separate disk
#                   pack, so snapshots can be booted). ~145 MB download.
#   raspi3ap        Raspberry Pi 3A+, aarch64 (single pack; snapshots can be
#                   saved but not booted). ~85 MB download.
#
# Artifacts come from https://ktock.github.io/qemu-wasm-demo/ (QEMU is GPLv2;
# sources at https://github.com/ktock/qemu-wasm).
set -euo pipefail

BASE="https://ktock.github.io/qemu-wasm-demo/images"
MACHINE="${1:-alpine-x86_64}"
DEST="${2:-./machines}/${MACHINE}"

fetch() { # fetch <relpath>
  echo "  $1"
  curl -fsSL --create-dirs -o "${DEST}/$1" "${BASE}/${MACHINE}/$1"
}

mkdir -p "${DEST}"
echo "fetching ${MACHINE} into ${DEST}:"

case "${MACHINE}" in
alpine-x86_64)
  for f in out.js qemu-system-x86_64.wasm qemu-system-x86_64.worker.js \
           load-rom.js load-rom.data load-kernel.js load-kernel.data \
           load-initramfs.js load-initramfs.data load-rootfs.js load-rootfs.data; do
    fetch "$f"
  done
  cat > "${DEST}/machine.json" <<'EOF'
{
  "title": "Alpine Linux (x86_64)",
  "main": "out.js",
  "loaders": ["load-rom.js", "load-kernel.js", "load-initramfs.js", "load-rootfs.js"],
  "diskLoader": "load-rootfs.js",
  "disk": "/pack-rootfs/disk-rootfs.img",
  "args": ["-nographic", "-M", "pc", "-m", "512M", "-accel", "tcg,tb-size=500",
           "-L", "/pack-rom/", "-nic", "none",
           "-kernel", "/pack-kernel/vmlinuz-virt",
           "-initrd", "/pack-initramfs/initramfs-virt",
           "-append", "console=ttyS0 root=/dev/vda noautodetect hostname=golem",
           "-drive", "id=root,file=/pack-rootfs/disk-rootfs.img,format=raw,if=none",
           "-device", "virtio-blk-pci,drive=root"]
}
EOF
  ;;
raspi3ap)
  for f in out.js qemu-system-aarch64.wasm qemu-system-aarch64.worker.js \
           qemu-system-aarch64.data load.js; do
    fetch "$f"
  done
  cat > "${DEST}/machine.json" <<'EOF'
{
  "title": "Raspberry Pi 3A+ (aarch64)",
  "main": "out.js",
  "loaders": ["load.js"],
  "disk": "/pack/rootfs.bin",
  "args": ["-nic", "none", "-M", "raspi3ap", "-nographic", "-m", "512M",
           "-accel", "tcg,tb-size=500", "-smp", "4",
           "-dtb", "/pack/bcm2710-rpi-3-b-plus.dtb",
           "-kernel", "/pack/kernel8.img",
           "-drive", "file=/pack/rootfs.bin,format=raw,if=sd",
           "-append", "earlycon=pl011,0x3f201000 console=ttyAMA0,115200 loglevel=6 initcall_blacklist=bcm2835_pm_driver_init root=/dev/mmcblk0 rootfstype=ext4 rootwait no_console_suspend"]
}
EOF
  ;;
*)
  echo "unknown machine '${MACHINE}' (known: alpine-x86_64, raspi3ap)" >&2
  exit 1
  ;;
esac

echo "done: $(du -sh "${DEST}" | cut -f1) in ${DEST}"
echo "next: push the parent directory into your golem volume, e.g."
echo "  scripts/enclave-encvol.sh push $(dirname "${DEST}") --endpoint https://… --bucket … --name golem"

#!/bin/sh
# Build the boot firmware for the RISC Box xorg guest: OpenSBI v0.9 with the
# Buildroot-built Linux Image embedded as FW_PAYLOAD, using the Buildroot
# output's own cross-toolchain. Run AFTER the Buildroot make; build.sh does.
#
#   guest/build-opensbi.sh <buildroot-output-dir> [opensbi-src-dir]
#
# Why not Buildroot's own OpenSBI package: 2024.02 carries OpenSBI 1.2, which
# links position-independent (FW_PIC=y) firmware; the vendored emulator's ELF
# loader cannot relocate it and the machine never reaches the SBI banner.
# v0.9 links at 0x80000000 the plain way (the known-good sample image shipped
# v0.8-era firmware). Two build quirks, both handled below:
#   - the Bootlin glibc toolchain enables -fstack-protector by default, and
#     freestanding OpenSBI has no __stack_chk_* to link: shim gcc to force
#     -fno-stack-protector.
#   - GCC 12 needs the ISA string spelled with _zifencei (fence.i is no
#     longer implied by rv64imafdc; F implies zicsr).
set -eu
OUT="${1:?usage: build-opensbi.sh <buildroot-output-dir> [opensbi-src-dir]}"
SRC="${2:-$OUT/riscbox-opensbi}"

if [ ! -d "$SRC" ]; then
    git clone --depth 1 --branch v0.9 \
        https://github.com/riscv-software-src/opensbi "$SRC"
fi

SHIM="$OUT/riscbox-sbishim"
mkdir -p "$SHIM"
cat > "$SHIM/riscv64-linux-gcc" <<EOF
#!/bin/sh
exec "$OUT/host/bin/riscv64-linux-gcc" -fno-stack-protector "\$@"
EOF
chmod +x "$SHIM/riscv64-linux-gcc"

PATH="$SHIM:$OUT/host/bin:$PATH" make -C "$SRC" \
    CROSS_COMPILE=riscv64-linux- \
    PLATFORM=generic \
    PLATFORM_RISCV_ISA=rv64imafdc_zifencei \
    FW_PIC=n \
    FW_PAYLOAD_PATH="$OUT/images/Image" \
    -j"$(nproc)"

cp "$SRC/build/platform/generic/firmware/fw_payload.elf" "$OUT/images/fw_payload.elf"
echo "build-opensbi: $OUT/images/fw_payload.elf"

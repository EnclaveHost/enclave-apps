#!/bin/sh
# Buildroot post-build hook for the RISC Box xorg guest: compile the spincube
# demo with the image's own toolchain straight into the target (no package
# scaffolding for one C file), and tighten the ssh bits the overlay laid down.
set -eu
GUEST="$(dirname "$0")"

# The cross-gcc name differs between an internal toolchain
# (riscv64-buildroot-linux-gnu-gcc) and an external one (riscv64-linux-gcc),
# so discover whichever wrapper this build produced.
CC=$(ls "$HOST_DIR"/bin/riscv64-*-gcc "$HOST_DIR"/bin/riscv64-linux-gcc 2>/dev/null | head -1)
[ -x "$CC" ] || { echo "post-build: no cross gcc in $HOST_DIR/bin"; exit 1; }
"$CC" -O2 -o "$TARGET_DIR/usr/bin/spincube" "$GUEST/cube.c" \
    --sysroot "$STAGING_DIR" -lX11 -lm

chmod 700 "$TARGET_DIR/root/.ssh"
chmod 600 "$TARGET_DIR/root/.ssh/authorized_keys"

# Build fontconfig's cache here rather than making the guest do it.
#
# The first process to use Xft scans every font on the system and writes the
# result to /var/cache/fontconfig. That scan is ~400 files (Xorg ships hundreds
# of gzipped PCF bitmaps, each of which must be decompressed and parsed), and
# on an emulated core in the tens of MIPS it is minutes of a desktop's startup
# — paid before the first window can draw, and paid again by every process that
# starts before the cache lands.
#
# It has to be the TARGET's own fc-cache: Buildroot configures fontconfig with
# --with-arch=$(GNU_TARGET_NAME), so caches are tagged riscv64-buildroot-linux-gnu
# and one built by the host's fontconfig carries the host triplet, which the
# guest silently ignores — a cache that is present, wrong, and rebuilt anyway.
# qemu user-mode runs the real riscv64 binary on the build machine in seconds.
QEMU=$(command -v qemu-riscv64 || command -v qemu-riscv64-static || true)
if [ -x "$TARGET_DIR/usr/bin/fc-cache" ]; then
	if [ -n "$QEMU" ]; then
		if "$QEMU" -L "$TARGET_DIR" "$TARGET_DIR/usr/bin/fc-cache" \
			--sysroot="$TARGET_DIR" -f >/dev/null 2>&1; then
			echo "post-build: fontconfig cache prebuilt ($(ls "$TARGET_DIR/var/cache/fontconfig" | wc -l) files)"
		else
			echo "post-build: WARNING fc-cache failed; the guest will rebuild it on first Xft use (minutes)"
		fi
	else
		echo "post-build: WARNING qemu-riscv64 not installed; shipping without a fontconfig cache."
		echo "post-build:          The guest will scan every font on first Xft use, which costs"
		echo "post-build:          minutes of desktop startup at emulated speed. Install qemu-user."
	fi
fi

# Buildroot's xserver package installs its own X starter (S40xorg) that races
# our S90xdesktop for display :0 ("Server is already active for display 0").
# Remove it so xdesktop.sh is the sole owner of X (it starts X with the
# LD_PRELOAD the fbdev driver needs, then runs twm + the cube).
rm -f "$TARGET_DIR/etc/init.d/S40xorg"

# Shim xkbcomp: the emulator can't run X's forked keymap compile (see the
# wrapper's own comment). Move the real binary aside and drop in the wrapper,
# which returns the prebuilt keymap the overlay ships.
if [ -f "$TARGET_DIR/usr/bin/xkbcomp" ] && [ ! -f "$TARGET_DIR/usr/bin/xkbcomp.real" ]; then
	mv "$TARGET_DIR/usr/bin/xkbcomp" "$TARGET_DIR/usr/bin/xkbcomp.real"
fi
install -m 0755 "$GUEST/xkbcomp-wrapper.sh" "$TARGET_DIR/usr/bin/xkbcomp"

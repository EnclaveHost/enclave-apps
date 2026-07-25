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

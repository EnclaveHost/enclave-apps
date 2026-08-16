#!/bin/sh
# The DOOM machine's whole session: no X, no window manager, no compositor.
#
# Every layer between the game and the framebuffer is paid for in emulated
# instructions, and on this machine that is the entire frame budget: the X path
# (SDL scale -> XPutImage -> Xorg blit) costs about four times as much per frame
# as DOOM's own renderer. fbdoom writes the palette-mapped 320x200 image
# straight into /dev/fb0 instead.
# Output goes to the console on purpose: the frame-rate line the game prints is
# the measurement, and inittab gives a respawned service /dev/console.
echo 0 > /proc/sys/kernel/printk 2>/dev/null

# fbcon paints the text console onto the same framebuffer; the game asks for
# KD_GRAPHICS itself, but make sure the VT it takes over is the visible one.
chvt 1 2>/dev/null

export HOME=/root
cd /root

# Wait for udev to have made the virtio-input keyboard, or the first seconds of
# play would swallow every keystroke. Bounded: no input is worse than no game,
# but a missing device must not stop DOOM from starting.
n=0
while [ ! -e /dev/input/event0 ] && [ "$n" -lt 30 ]; do
    sleep 1
    n=$((n + 1))
done

# Respawned by inittab, so a quit or a crash brings the game straight back.
exec /usr/bin/fbdoom -iwad /usr/share/games/doom/freedoom1.wad "$@"

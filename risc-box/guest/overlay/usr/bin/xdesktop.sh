#!/bin/sh
# The RISC Box desktop: Xorg on /dev/fb0, twm, and the spincube demo.
# Started by S90xdesktop when the kernel has a framebuffer.
#
# LD_PRELOAD note: xf86-video-fbdev's fbdev_drv.so has undefined symbols that
# live in helper modules — the fbdevHW* family in libfbdevhw.so and the
# shadow* family in libshadow.so. Xorg's module loader opens the driver with
# eager binding before the driver's own submodule loader runs, so those
# symbols must already be in the global namespace; preloading both modules
# puts them there and the driver resolves. (xorg.conf also Loads them as
# belt-and-suspenders.)
export DISPLAY=:0
XMOD=/usr/lib/xorg/modules
PRELOAD="$XMOD/libfbdevhw.so $XMOD/libshadow.so"

start_x() {
    # clear any stale lock/socket from a prior server before starting
    pkill -9 Xorg 2>/dev/null
    rm -f /tmp/.X0-lock /tmp/.X11-unix/X0
    LD_PRELOAD="$PRELOAD" /usr/bin/X :0 -nolisten tcp vt2 &
    X_PID=$!
    # X init is heavy at emulated speed; wait up to ~90s for the socket
    n=0
    while [ ! -S /tmp/.X11-unix/X0 ] && [ $n -lt 90 ]; do
        kill -0 "$X_PID" 2>/dev/null || return 1
        sleep 1; n=$((n+1))
    done
    [ -S /tmp/.X11-unix/X0 ]
}

# bring X up (retry, but never tight-loop and thrash the emulated CPU)
until start_x; do
    echo "xdesktop: X failed to start, retrying in 10s" >&2
    pkill X 2>/dev/null; sleep 10
done

twm &
xsetroot -solid "#204060" 2>/dev/null

# the spinning cube; if it ever exits, pause before relaunch so a crash loop
# can't starve the emulated CPU
while :; do
    /usr/bin/spincube
    sleep 5
done

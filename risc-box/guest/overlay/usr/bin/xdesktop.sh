#!/bin/sh
# The RISC Box desktop: Xorg on /dev/fb0, then a session on top of it.
# Started by S90xdesktop when the kernel has a framebuffer.
#
# Two sessions are supported. If the image carries XFCE that is what runs;
# otherwise it falls back to twm plus the spincube demo, which is what the
# minimal image ships. Both paths share the X startup below.
#
# LD_PRELOAD note: xf86-video-fbdev's fbdev_drv.so has undefined symbols that
# live in helper modules — the fbdevHW* family in libfbdevhw.so and the
# shadow* family in libshadow.so. Xorg's module loader opens the driver with
# eager binding before the driver's own submodule loader runs, so those
# symbols must already be in the global namespace; preloading both modules
# puts them there and the driver resolves. (xorg.conf also Loads them as
# belt-and-suspenders.)
export DISPLAY=:0
export HOME=/root
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

# Something must own the root window before a session starts, or the first
# repaint is whatever the framebuffer happened to contain.
xsetroot -solid "#204060" 2>/dev/null

run_twm_session() {
    echo "xdesktop: starting twm + spincube" >&2
    twm &
    # the spinning cube; if it ever exits, pause before relaunch so a crash
    # loop can't starve the emulated CPU
    while :; do
        /usr/bin/spincube
        sleep 5
    done
}

if [ -x /usr/bin/xfce4-session ]; then
    # XFCE reaches xfconfd and the session manager over a session bus, so one
    # has to exist. dbus-run-session creates it, runs the session under it,
    # and tears it down when the session exits.
    #
    # On expectations: this is a full GTK3 desktop on an emulated CPU in the
    # tens of MIPS. Startup is minutes rather than seconds, and that time is
    # real work (icon cache, fontconfig, gsettings), not a hang.
    echo "xdesktop: starting XFCE session" >&2
    tries=0
    while [ $tries -lt 3 ]; do
        dbus-run-session -- /usr/bin/xfce4-session
        tries=$((tries+1))
        echo "xdesktop: XFCE session exited (attempt $tries), restarting in 10s" >&2
        sleep 10
    done
    # Three straight exits means XFCE is not going to come up on this image.
    # A machine reachable only through a framebuffer is miserable to debug, so
    # leave something driveable on screen rather than an empty root window.
    echo "xdesktop: XFCE failed three times, falling back to twm" >&2
    run_twm_session
else
    echo "xdesktop: no XFCE in this image" >&2
    run_twm_session
fi

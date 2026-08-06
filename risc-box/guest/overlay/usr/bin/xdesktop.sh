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

run_matchbox_session() {
    # Matchbox: a desktop environment built for handhelds, which is the same
    # problem this machine has — a slow CPU, a framebuffer, and no GPU. There
    # is no toolkit to initialise and no icon theme to index, so unlike XFCE it
    # reaches first paint without minutes of one-time work.
    #
    # Started component by component rather than through matchbox-session for
    # the same reason XFCE is: the panel and desktop both want a window manager
    # already owning the screen, and on hardware this slow they will win that
    # race against the WM and come up unmanaged.
    echo "xdesktop: starting matchbox session" >&2
    tries=0
    while [ $tries -lt 3 ]; do
        matchbox-window-manager -use_titlebar yes &
        WM_PID=$!
        sleep 8
        # --no-session, not --no-menu: matchbox-panel rejects an unknown flag
        # and exits, which looks exactly like a desktop that never painted.
        # There is nowhere to save a session to on a machine whose disk is
        # thrown away at stop, so turning it off is right anyway.
        [ -x /usr/bin/matchbox-panel ] && matchbox-panel --no-session &
        # matchbox-desktop is deliberately NOT started. It segfaults inside
        # libmb on this target ("unhandled signal 11 ... in libmb.so.1.0.9"),
        # and what it contributes — the icon folder and a backdrop — is worth
        # less than the black screen it paints on its way down. The same
        # judgement as xfdesktop on the XFCE image, for a blunter reason.
        #
        # Something must own the root once the window manager has taken the
        # screen, or the desktop is a black void with a panel floating on it.
        (sleep 12; xsetroot -solid "#204060" 2>/dev/null) &
        # Keep the session alive as long as the window manager is.
        wait $WM_PID
        tries=$((tries+1))
        echo "xdesktop: matchbox session exited (attempt $tries), restarting in 10s" >&2
        pkill matchbox-panel 2>/dev/null
        pkill matchbox-desktop 2>/dev/null
        sleep 10
    done
    echo "xdesktop: matchbox failed three times, falling back to twm" >&2
    run_twm_session
}

run_fluxbox_session() {
    # fluxbox carries the launcher and the taskbar in the same binary as the
    # window manager: right-click the root for the menu, and the toolbar lists
    # windows and workspaces. That matters more than it sounds — matchbox is
    # lighter, but its launcher is a separate binary that crashes here, which
    # leaves a session you cannot start anything from.
    echo "xdesktop: starting fluxbox session" >&2
    # Point fluxbox at the menu the image ships. Generating one at first start
    # probes the filesystem for minutes at emulated speed to rediscover a
    # package list that was known at build time.
    mkdir -p /root/.fluxbox
    : > /root/.fluxbox/init
    # Point at our menu only if it is really there. fluxbox ships a usable
    # default at /usr/share/fluxbox/menu, and naming a menuFile that does not
    # exist produces an EMPTY right-click menu — a desktop you cannot launch
    # anything from, which is exactly the failure this session is fixing.
    if [ -f /etc/fluxbox/menu ]; then
        cp /etc/fluxbox/menu /root/.fluxbox/menu
        echo "session.menuFile: /root/.fluxbox/menu" >> /root/.fluxbox/init
    else
        echo "xdesktop: no /etc/fluxbox/menu; using fluxbox's default menu" >&2
    fi
    echo "session.screen0.toolbar.visible: true" >> /root/.fluxbox/init
    # Rendering every window move/resize live means a full-screen software
    # repaint per motion event, which this machine cannot afford.
    echo "session.screen0.opaqueMove: false" >> /root/.fluxbox/init
    echo "session.screen0.fullMaximization: true" >> /root/.fluxbox/init
    tries=0
    while [ $tries -lt 3 ]; do
        fluxbox
        tries=$((tries+1))
        echo "xdesktop: fluxbox exited (attempt $tries), restarting in 10s" >&2
        sleep 10
    done
    echo "xdesktop: fluxbox failed three times, falling back to twm" >&2
    run_twm_session
}

if [ -x /usr/bin/fluxbox ]; then
    run_fluxbox_session
elif [ -x /usr/bin/matchbox-window-manager ]; then
    run_matchbox_session
elif [ -x /usr/bin/xfce4-session ]; then
    # XFCE reaches xfconfd and the session manager over a session bus, so one
    # has to exist. dbus-run-session creates it, runs the session under it,
    # and tears it down when the session exits.
    #
    # On expectations: this is a full GTK3 desktop on an emulated CPU in the
    # tens of MIPS. Startup is minutes rather than seconds, and that time is
    # real work (icon cache, fontconfig, gsettings), not a hang.
    # Start the components in order rather than letting xfce4-session race
    # them. On hardware this slow xfwm4 needs tens of seconds to claim the
    # screen, and xfce4-panel and xfdesktop check for a registered window
    # manager the moment they start — losing that race, they print "No window
    # manager registered on screen 0" and never draw. Starting the WM first
    # and passing --disable-wm-check to the other two removes the race
    # entirely instead of hoping the timing works out.
    echo "xdesktop: starting XFCE session" >&2
    tries=0
    while [ $tries -lt 3 ]; do
        # xfdesktop is deliberately not started. Its visible contributions are
        # the backdrop, desktop icons and the root menu; the backdrop is
        # already painted above by xsetroot, and the other two are not worth
        # what they cost here. Left running it paints a black backdrop over
        # that root colour and then competes with the panel for a CPU that has
        # none to spare — the observable result was a black screen with an
        # unpainted panel. Dropping it gives back a visible desktop and lets
        # the panel reach first paint sooner.
        dbus-run-session -- sh -c '
            xfsettingsd &
            xfwm4 &
            # Give the window manager time to take the screen before anything
            # asks whether it has.
            sleep 25
            xfce4-panel --disable-wm-check &
            # Repaint the root once the panel is up. Without xfdesktop nothing
            # owns the backdrop, and the colour set before the session started
            # does not survive the session taking the screen — the desktop
            # would otherwise be a black void with panels floating on it.
            (sleep 90; xsetroot -solid "#204060" 2>/dev/null) &
            # Keep the session alive as long as the window manager is.
            wait %2
        '
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

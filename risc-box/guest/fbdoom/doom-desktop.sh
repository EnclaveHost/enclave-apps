#!/bin/sh
# DOOM launcher for the RISC Box Alpine desktop (baked as /usr/bin/doom).
#
# Launches the SAME game the boot session does: xdoom, the raw-X build with the
# uncapped renderer, the direct-to-framebuffer overlay, and sound. A relaunch
# from the fluxbox menu is then identical to the game that came up at boot,
# audio included. The old wrapper ran `chocolate-doom -nosound` because the
# guest had no audio device and SDL's probing wasted emulated time; the guest
# has one now (virtio-snd + the host OPL synth), so sound is on by default and
# the SDL build is not used at all.
#
# Retry ONLY quick deaths. The WM focus/reparent race kills the game within a
# couple of seconds, and the retry runs against a settled WM. A later exit is
# the player quitting (window close -> "press y" -> quit), whatever status the
# teardown reports, so it must NOT respawn — that made the game unkillable from
# the desktop.
export DISPLAY="${DISPLAY:-:0}" HOME="${HOME:-/root}" XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run}"
n=0
while :; do
    t0=$(date +%s)
    xdoom -uncapped -overlay -scaling 2 "$@" && exit 0
    dt=$(( $(date +%s) - t0 ))
    [ "$dt" -ge 5 ] && exit 0
    n=$((n+1))
    [ "$n" -ge 10 ] && exit 1
    sleep 1
done

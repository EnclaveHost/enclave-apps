#!/bin/sh
# DOOM launcher for the RISC Box Alpine desktop (baked as /usr/bin/doom).
#
# chocolate-doom's default fullscreen path always dies with X BadMatch on this
# fbdev server, and even windowed creation loses a focus/reparent race with
# the window manager about half the time (the error lands in the terminal it
# was typed into, so it just looks like nothing happened). Retrying converges:
# the retry runs against a settled WM. Sound is stubbed off because the guest
# has no audio device and SDL's probing wastes seconds of emulated time.
#
# Retry ONLY quick deaths. The startup races this loop exists for kill the
# game within a couple of seconds; an exit after a real run is the PLAYER
# quitting (window close -> "press y" -> quit), whatever exit status the
# teardown reports — chocolate-doom leaves nonzero on this server even for a
# clean quit, and respawning on it made the game unkillable from the desktop.
export DISPLAY="${DISPLAY:-:0}" HOME="${HOME:-/root}" XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run}"
export SDL_RENDER_DRIVER=software
n=0
while :; do
    t0=$(date +%s)
    chocolate-doom -nomusic -nosfx -nosound -window -width 640 -height 480 "$@" && exit 0
    dt=$(( $(date +%s) - t0 ))
    [ "$dt" -ge 5 ] && exit 0
    n=$((n+1))
    [ "$n" -ge 10 ] && exit 1
    sleep 1
done

#!/bin/sh
# The vendored emulator mishandles X's fork+pipe+wait of a child xkbcomp during
# keyboard init: standalone xkbcomp (and even X's exact command line, run by
# hand) compiles fine, but when the X server itself spawns it the compile is
# reported failed and X aborts ("Failed to activate virtual core keyboard").
# This headless X has no real keyboard anyway — input reaches the guest over the
# console UART, not X — so any valid keymap suffices. When X asks to compile a
# keymap to a .xkm file, hand back a prebuilt one instead of compiling; defer
# everything else to the real xkbcomp.
last=""
for a in "$@"; do last="$a"; done
case "$last" in
	*.xkm)
		cat >/dev/null 2>&1   # drain the piped keymap so X doesn't SIGPIPE
		cp /usr/share/X11/xkb/precompiled.xkm "$last" 2>/dev/null && exit 0
		exit 1 ;;
esac
exec /usr/bin/xkbcomp.real "$@"

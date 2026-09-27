#!/bin/sh
# Fullscreen desktop launcher. Use the completed-frame/AOT-matched xdoom build
# described in docs/doom-aot.md; RISC_DOOM_BIN can select an installed copy.
# Keep the desktop mode and sound, and let the renderer use its direct overlay.
export DISPLAY="${DISPLAY:-:0}" HOME="${HOME:-/root}"
game=${RISC_DOOM_BIN:-xdoom}
log=${RISC_DOOM_LOG:-/var/log/xdoom.log}
wm_config="$HOME/.fluxbox/init"
toolbar=$(sed -n 's/.*toolbar.visible:[[:space:]]*//p' "$wm_config" 2>/dev/null | head -1)
case "$toolbar" in true|false) ;; *) toolbar=true ;; esac

restore_desktop() {
    if [ -n "${child:-}" ]; then
        kill -TERM "$child" 2>/dev/null
        wait "$child" 2>/dev/null
    fi
    sed -i "s/toolbar.visible:.*/toolbar.visible: $toolbar/" "$wm_config" 2>/dev/null
    xsetroot -cursor_name left_ptr 2>/dev/null
    fluxbox-remote reconfigure 2>/dev/null
}
trap restore_desktop EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

sed -i 's/toolbar.visible:.*/toolbar.visible: false/' "$wm_config" 2>/dev/null
xsetroot -cursor "$HOME/.blank.xbm" "$HOME/.blank.xbm" 2>/dev/null
fluxbox-remote reconfigure 2>/dev/null
size=$(xrandr 2>/dev/null | sed -n 's/.*current \([0-9]*\) x \([0-9]*\).*/\1 \2/p')
width=${size% *}
height=${size#* }
case "$width:$height" in *[!0-9:]*|:*) width=960; height=600 ;; esac
scale=$((width / 320))
[ $((height / 200)) -lt "$scale" ] && scale=$((height / 200))
[ "$scale" -ge 1 ] || scale=1

"$game" -mb 64 -uncapped -overlay -scaling "$scale" \
    -iwad /usr/share/games/doom/freedoom1.wad "$@" >"$log" 2>&1 &
child=$!
wait "$child"
status=$?
child=
exit "$status"

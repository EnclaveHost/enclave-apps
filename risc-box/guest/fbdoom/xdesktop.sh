#!/bin/sh
export DISPLAY=:0 HOME=/root XDG_RUNTIME_DIR=/run
export SDL_AUDIODRIVER=dummy SDL_VIDEODRIVER=x11
exec >/var/log/xdesktop.log 2>&1

# --- framebuffer console ------------------------------------------------------
# Do NOT unbind fbcon. When Xorg grabs vt2 it puts that VT in KD_GRAPHICS mode,
# which suppresses fbcon on the framebuffer for as long as X holds the VT -- so a
# STABLE X owns fb0 cleanly with fbcon bound (verified). Unbinding it instead
# left fb0 owned by nobody and X rendered to a dead buffer (black screen). The
# console-repaint that looked like an fbcon fight was really X *crashing* on the
# C.FLDSP SIGILL and releasing vt2 back to the text console; that's fixed in the
# emulator now. Silence kernel printk so a stray dmesg line can't repaint either.
echo 0 > /proc/sys/kernel/printk 2>/dev/null

# Determinism for the baked-region JIT: with mmap ASLR off, Xorg and the
# shared libraries land at the same virtual addresses every boot, so the
# regions profiled at build time keep matching in production. The game
# binary is ET_EXEC and never moved anyway. This machine runs one appliance
# workload; address randomization defends nothing here.
echo 0 > /proc/sys/kernel/randomize_va_space 2>/dev/null


# NOTE: do NOT LD_PRELOAD the fbdev/shadow modules. On musl (no lazy binding)
# the preload cannot resolve xf86*/Damage* against a /bin/sh wrapper, and it
# also poisons every X client. Xorg's own loader resolves them fine. Also exec
# the real server, not /usr/bin/X (a shell wrapper).
start_x() {
    killall -9 Xorg 2>/dev/null
    rm -f /tmp/.X0-lock /tmp/.X11-unix/X0
    /usr/libexec/Xorg :0 -nolisten tcp vt2 &
    X_PID=$!
    n=0
    while [ ! -S /tmp/.X11-unix/X0 ] && [ $n -lt 90 ]; do
        kill -0 "$X_PID" 2>/dev/null || return 1
        sleep 1; n=$((n+1))
    done
    [ -S /tmp/.X11-unix/X0 ]
}
until start_x; do
    echo "X failed to start; Xorg.0.log tail:"; tail -n 25 /var/log/Xorg.0.log 2>/dev/null
    echo "retry 10s"; sleep 10
done
echo "=== X is up (uptime $(cut -d. -f1 /proc/uptime)s) ==="

# X grabs vt2 but on this emulated console the VT switch to vt2 does not reliably
# become the ACTIVE console. A simple-framebuffer only ever shows the active VT,
# so if vt2 is not active X renders to an off-screen VT buffer and fb0 keeps
# showing vt1 (black). Force vt2 active so X's output actually reaches fb0.
for i in 1 2 3 4 5; do
    chvt 2 2>/dev/null
    [ "$(cat /sys/class/tty/tty0/active 2>/dev/null)" = "tty2" ] && break
    sleep 1
done
echo "active vt now: $(cat /sys/class/tty/tty0/active 2>/dev/null)"

# never blank/DPMS-off: on this simple-framebuffer a blank clears fb0 to black
# and there is no hardware to unblank it cleanly.
xset s off 2>/dev/null
xset s noblank 2>/dev/null
xset -dpms 2>/dev/null

xsetroot -solid "#1a1a2e" 2>/dev/null
fluxbox >/var/log/fluxbox.log 2>&1 &
# ~25-70 MIPS core: DO NOT stampede it. Launching fluxbox+xterm+doom within
# seconds serializes their inits into minutes of black screen (measured: 16G
# instructions of contention before first paint). Let the WM finish, then one
# terminal; DOOM and Firefox launch on demand from the fluxbox menu.
# Wait for the KEYBOARD X device, not a fixed time. The 45s sleep was
# tuned for a ~25 MIPS emulator; the 2026-08 interpreter is 2.4x faster
# and guest time races during idle, so the terminal could map before the
# keyboard subdevice existed (~45 guest-seconds after the pointer) and
# typing into a fresh desktop went nowhere. Gating on the Xorg log makes
# the desktop appear exactly when typing works. Bounded so a logging
# surprise cannot hold the terminal hostage.
n=0
until grep -q 'type: KEYBOARD' /var/log/Xorg.0.log 2>/dev/null; do
    sleep 1; n=$((n+1)); [ "$n" -ge 120 ] && break
done
# Bottom-right, out of the game's way. Placement is not cosmetics here: the
# game's fast path writes its window's pixels straight into the framebuffer,
# and that is only sound while the window is wholly unobscured — one corner of
# an overlapping terminal and every frame falls back to a full copy through the
# server. fluxbox honours a user-specified geometry.
xterm -fn fixed -geometry 60x18+660+430 -bg "#101020" -fg "#c8d0ff" \
    -T "RISC Box" >/var/log/xterm.log 2>&1 &

# DOOM. The desktop above is exactly as shipped; this adds the game to it.
# xdoom talks the X protocol directly and fuses the palette lookup into the
# scale, because the SDL path spends ~20 emulated instructions per presented
# pixel (a converting blit through a generic scaler, then a copy through the
# socket) against ~6 here -- 16 fps versus 37 for the same window on the same
# screen. -scaling 2 is a 640x400 window, the size this image's own launcher
# always used.
( sleep 5
  # NO -overlay on the virtio-gpu image. The overlay mmaps /dev/fb0 and
  # paints DOOM straight into the simple-framebuffer, which was a 1.33x
  # win while fb0 WAS the screen. With Xorg on /dev/dri/card0 nothing
  # scans fb0 out any more, so those writes are invisible -- measured at
  # 13 MB every 2 s of emulated memory traffic for a buffer nobody reads.
  # The frames reach the screen through X either way (verified by
  # snapshot: live game in the scanout), so dropping it RECLAIMS work.
  # -overlay writes the game straight into the (invisible) simple-framebuffer;
  # the APP composites that rectangle over the GPU scanout natively, so the
  # whole X copy chain drops out of the guest's frame budget.
  DISPLAY=:0 /usr/bin/xdoom -uncapped -overlay -scaling 2 -iwad /usr/share/games/doom/freedoom1.wad \
    >/var/log/xdoom.log 2>&1 ) &

# On a ~25 MIPS core the WM can sit for minutes after startup without ever
# painting the root, leaving the framebuffer holding X's initial black clear --
# which looks exactly like a hung machine. Re-assert the background once the
# clients are up so there is always something on screen.
sleep 20
xsetroot -solid "#1a1a2e" 2>/dev/null

# console beacon so we can see client liveness without X logs
( while :; do
    sleep 10
    # keep vt2 the active console so X keeps owning fb0 (simplefb has no real
    # VT switching; if anything steals the console, X's output stops reaching fb0)
    [ "$(cat /sys/class/tty/tty0/active 2>/dev/null)" = "tty2" ] || chvt 2 2>/dev/null
    b=""
    for p in Xorg fluxbox xterm; do
        pgrep -x "$p" >/dev/null 2>&1 && b="$b $p=up" || b="$b $p=DEAD"
    done
    echo "[beacon]$b"
done ) &
while :; do sleep 3600; done

# DOOM on the RISC Box desktop

What turns the Alpine image into a machine that plays Freedoom at the engine's
own 35 fps. Two files, and neither of them is emulator work: the frame budget
was never going to the game.

## Why not just run chocolate-doom

The desktop image already has chocolate-doom and `freedoom1.wad`, and it plays.
It plays at **14 frames per real second**, and 12.6M guest instructions go into
each one. DOOM draws 320x200; what the X path does with that is:

    render 320x200 -> SDL scales to 640x480 -> 8bpp to 32bpp -> XPutImage
      -> Xorg blits into a 1024x768 framebuffer -> the app scans 3 MiB of it

The game's own renderer is the cheapest link in that chain. Everything after it
is pixels being copied, converted and copied again, and on an emulated CPU every
one of those copies is interpreted instructions.

Straight to the framebuffer, at DOOM's native size, the same frame costs **1.6M
instructions** — and the machine hits the engine's 35 fps ceiling with room to
spare.

## i_video_rbfb.c

A replacement video/input backend for
[fbDOOM](https://github.com/maximevince/fbDOOM), whose own fbdev backend builds
a whole-screen intermediate image and pushes it with a `write()` syscall per
frame. This one:

- **mmaps `/dev/fb0`** and writes only the rectangle DOOM drew;
- **fuses the palette lookup into the scale** — one 256-entry XRGB table, and
  at scale 2 a doubled pixel pair is one 64-bit store, so a presented frame is
  `w*h*4` bytes of stores and no other per-pixel work;
- **reads the keyboard off evdev**, because the machine's keyboard is a
  virtio-input device the host feeds from the browser and there is no X here to
  do it;
- **takes the VT into KD_GRAPHICS**, so fbcon stops painting the console over
  the game, and puts it back on the way out;
- **prints a wall-clock frame rate** every 100 frames. That number is only
  honest when the machine runs with `realtime: true` — see the README's *What
  time it is inside*; on the instruction-driven clock the guest's own seconds
  stretch with the emulator and the game reports a rate that has nothing to do
  with the one you can see.

Build it against the fbDOOM tree with any riscv64 toolchain, static so it does
not care that the image is musl:

    # in fbDOOM/fbdoom, with i_video_rbfb.o replacing i_video_fbdev.o and
    # i_input_tty.o in OBJS
    make CROSS_COMPILE=riscv64-linux- NOSDL=1 \
         CFLAGS="-O2 -static -DNORMALUNIX -DLINUX" LDFLAGS="-static"

(Upstream hardcodes `homedir = "/mnt"` in `m_config.c` for its own appliance;
put `getenv("HOME")` back or DOOM writes its config where this image has
nothing.)

## doom-run.sh

The whole session: no X, no window manager, no compositor. Installed as
`/usr/bin/doom-run.sh` with inittab's respawn line pointed at it instead of
`xdesktop.sh`, so a quit or a crash brings the game straight back, and its
stdout — including that frame-rate line — lands on the serial console, where
the app's `/console` stream carries it to the browser.

## Installing into an image

`debugfs` writes into an ext2 image without mounting it or needing root:

    debugfs -w -f - rootfs.ext2 <<'EOF'
    write fbdoom /usr/bin/fbdoom
    write doom-run.sh /usr/bin/doom-run.sh
    sif /usr/bin/fbdoom mode 0100755
    sif /usr/bin/doom-run.sh mode 0100755
    EOF

The WAD is already at `/usr/share/games/doom/freedoom1.wad` in the Alpine
desktop image.

## i_video_x11raw.c — the one that runs on the desktop

`i_video_rbfb.c` above owns the whole screen. This one is an ordinary X client
on the shipped Alpine desktop, in a window, with the screen left at 1024x768 —
and it is the interesting measurement, because it isolates how much of DOOM's
cost on this machine was never DOOM.

Measured in the app's wasm build, same guest, same screen, same window:

| what draws the window | instructions/frame | fps |
|---|---|---|
| chocolate-doom 640x480 (SDL) | 8.5M | 16 |
| xdoom 640x400, PutImage | 4.2M | 28 |
| xdoom 640x400, MIT-SHM | 2.9M | **45** |
| xdoom 960x600, MIT-SHM | 4.8M | 25 |

Three things account for the difference, none of them emulator work:

1. **The palette lookup is fused into the scale**, and a doubled pixel pair is
   one 64-bit store. SDL converts 8-bit to ARGB into a surface, then blits that
   through a generic scaler into a differently-formatted surface: about twenty
   emulated instructions per presented pixel against about two.
2. **MIT-SHM.** Without it the frame is copied into the socket by the client
   and out of it by the server — two copies of 1 MB, per frame, worth 1.3M
   instructions. With it the scale writes where the server can already see, and
   a frame is a 40-byte request. Two buffers alternate, with ShmCompletion
   telling us when one is free again.
3. **No Xlib.** Speaking the protocol directly means no per-frame library
   buffering or format negotiation, and a static binary that does not care that
   the guest is musl and the toolchain is glibc.

What is left per frame is DOOM's own renderer (~1.2M) plus the server's blit
out of shared memory into the framebuffer (~4 instructions per pixel, which is
Xorg's to own).

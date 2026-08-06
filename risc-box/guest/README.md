# RISC Box guest images

Two images, both built out of tree with Buildroot 2024.02.x LTS and both
booting the same way (OpenSBI v0.9 + Linux 5.15 as one `fw_payload.elf`, plus
an ext2 root the emulator attaches as `/dev/vda`):

| defconfig | what you get | rootfs |
|---|---|---|
| `buildroot.config` | Xorg (fbdev) + twm + dropbear + the spincube demo | 96 MiB |
| `buildroot-matchbox.config` | the above plus the matchbox desktop | 128 MiB |
| `buildroot-xfce.config` | the above plus XFCE 4.18 | 320 MiB |

```sh
guest/build.sh <buildroot-tree> <build-dir>                                  # twm
guest/build.sh <buildroot-tree> <build-dir> guest/buildroot-matchbox.config  # matchbox
guest/build.sh <buildroot-tree> <build-dir> guest/buildroot-xfce.config      # XFCE
```

## What a desktop costs

Measured in **retired guest instructions**, not seconds. Instructions are a
property of the guest, so they hold still while the emulator underneath gets
faster and while the host does other things; seconds do neither. The number
that matters is where the instret curve flattens — that is the desktop
finishing its startup and parking in WFI, and it is unambiguous in a way that
"looks painted" is not.

| | XFCE | matchbox |
|---|---|---|
| X server up | — | 1.6G |
| panel painted | — | 4.9G |
| **guest settles idle** | **~22G** | **~4.9G** |
| installed tree | 163 MiB | 72 MiB |
| gzipped image | 53 MiB | 24.6 MiB |

**About 4.5x fewer instructions**, and less than half the image. At the ~21 MIPS
a deployment gets, that is roughly four minutes to a usable desktop instead of
seventeen.

The difference is structural rather than tuning. XFCE is GTK3, which
hard-depends on an OpenGL provider (so Mesa comes too) and wants an icon theme
— Adwaita is 42 MiB and ~6000 files. Every XFCE process pays GTK3 startup:
linking an ~8 MiB library, parsing the theme CSS, building icon caches.
Matchbox's window manager and panel were written for handhelds and depend on
matchbox-lib, Xext, Xpm, expat and zlib. There is no toolkit, so none of that
work exists to be optimised away.

Matchbox is a window manager, a panel and xterm — sparse next to XFCE's file
manager, settings dialogs and applications menu. Pick it when you want a
desktop that is *there*; pick XFCE when you want one that is *furnished*, and
budget the seventeen minutes.

Both leave `images/fw_payload.elf` and `images/rootfs.ext2` in the build dir.
Seed them into the deployment's S3 bucket with `scripts/seed-machine.py put`.

**Size the rootfs to its content.** The app holds the whole disk image in its
wasm32 linear memory alongside the guest's 512 MiB of DRAM. An earlier 768 MiB
XFCE image tipped that over — the read grew a buffer to 1 GiB, the allocation
failed, and the module trapped before the machine booted (`memory allocation
of 1073741824 bytes failed`). The XFCE tree installs to about 143 MiB, so
320 MiB is generous; treat roughly half a gigabyte as the ceiling.

## Which one to use

The twm image is the one to reach for by default. It boots in seconds, leaves
the emulated CPU almost entirely to whatever you are actually running, and the
spincube demo makes it obvious at a glance that the display path works.

The XFCE image is a real desktop environment — panel, window manager, desktop
icons, file manager, settings — and it is correspondingly heavy. Be clear-eyed
about the hardware: an emulated RV64GC core in the tens of MIPS, 512 MiB of
RAM, a software framebuffer, and no GPU.

Measured, so you can decide for yourself. It does work: the image boots, X
comes up, xfwm4 takes the screen, and the panel paints — Applications menu,
window buttons, the dock along the bottom. The pointer works, and a Moonlight
client streams it (1158 frames / 20 IDR in a 25 s session, cursor tracking
injected input exactly).

What it costs is time. The guest sits at **0% idle with a load average above
7 on a single emulated core** through startup, and the panel's first paint
arrives on the order of ten minutes after boot, not seconds. While that is
happening the machine is saturated enough that `ssh` cannot complete a banner
exchange. RAM is not the constraint (about 127 MiB of 482 MiB in use) — CPU
is, entirely.

So: XFCE genuinely runs and is driveable, and it is a fair demonstration that
this is a real Linux desktop. It is not what you would pick for interactive
work. **Use the twm image unless you specifically want XFCE**; it boots in
seconds and leaves the CPU to whatever you are actually doing.

The guest tuning that makes XFCE bearable is in the overlay and is worth
keeping if you change things: xfwm4's compositor is off, GTK animations are
off, and window move/resize draw outlines rather than live content
(`overlay/etc/xdg/`). Each of those removes full-screen software repaints,
which cost twice here — once in the guest's CPU and again in the encoder that
has to carry every changed pixel to a streaming client.

`xdesktop.sh` also starts the session's parts in order rather than handing the
job to `xfce4-session`, and that is not stylistic. xfce4-panel and xfdesktop
check for a registered window manager the moment they start; on hardware this
slow, xfwm4 has not yet claimed the screen, so both lose the race, log "No
window manager registered on screen 0", and never draw — while continuing to
run, so `ps` makes everything look healthy. Starting xfwm4 first, giving it a
real head start, and passing `--disable-wm-check` to the other two removes the
race rather than hoping the timing lands.

## Why XFCE lives in `br2-external/`

Buildroot has never carried XFCE (checked through master), so the twelve
packages it needs are ours: `libwnck3` plus XFCE's own `libxfce4util`,
`xfconf`, `libxfce4ui`, `garcon`, `exo`, `xfwm4`, `xfce4-panel`, `xfdesktop`,
`xfce4-session`, `xfce4-settings` and `thunar`. They live in
`guest/br2-external`, which `build.sh` points `BR2_EXTERNAL` at.

Two things about that dependency graph are easy to get wrong, because they run
opposite to how the names read: **`exo` and `garcon` both depend on
`libxfce4ui`**, not the other way around, and `xfce4-session` needs
`libwnck3` even though nothing in its name suggests a window-list library.
Both were taken from the packages' own `configure.ac` rather than guessed.

Buildroot's GTK3 also hard-depends on an OpenGL provider even though nothing
in this guest draws with GL, so the XFCE config pulls in Mesa's Gallium swrast
behind GLX — deliberately without `BR2_PACKAGE_MESA3D_LLVM`, since llvmpipe
would drag LLVM into a target that will never use it.

## Host build notes

Buildroot builds a handful of tools with the *host* compiler, and several of
them (m4's bundled gnulib most visibly) predate C23, where `bool`, `true` and
`false` became keywords. GCC 15+ defaults to `gnu23` and those headers stop
compiling, so `build.sh` pins `HOST_CFLAGS` to `-std=gnu17`. This affects only
tools that run on the build machine; target code is built by the Bootlin
cross-toolchain and is unaffected.

`rsync` is a hard Buildroot dependency and is not always installed. If the
build stops with "You must install 'rsync'", that is what it means.

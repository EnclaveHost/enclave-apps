# RISC Box guest images

Two images, both built out of tree with Buildroot 2024.02.x LTS and both
booting the same way (OpenSBI v0.9 + Linux 5.15 as one `fw_payload.elf`, plus
an ext2 root the emulator attaches as `/dev/vda`):

| defconfig | what you get | rootfs |
|---|---|---|
| `buildroot.config` | Xorg (fbdev) + twm + dropbear + the spincube demo | 96 MiB |
| `buildroot-xfce.config` | the above plus XFCE 4.18 | 768 MiB |

```sh
guest/build.sh <buildroot-tree> <build-dir>                              # twm
guest/build.sh <buildroot-tree> <build-dir> guest/buildroot-xfce.config  # XFCE
```

Both leave `images/fw_payload.elf` and `images/rootfs.ext2` in the build dir.
Seed them into the deployment's S3 bucket with `scripts/seed-machine.py put`.

## Which one to use

The twm image is the one to reach for by default. It boots in seconds, leaves
the emulated CPU almost entirely to whatever you are actually running, and the
spincube demo makes it obvious at a glance that the display path works.

The XFCE image is a real desktop environment — panel, window manager, desktop
icons, file manager, settings — and it is correspondingly heavy. Be clear-eyed
about the hardware: an emulated RV64GC core in the tens of MIPS, 512 MiB of
RAM, a software framebuffer, and no GPU. Session startup is minutes rather
than seconds, and interaction is deliberate. It is genuinely driveable, and it
is genuinely not fast. If the goal is responsiveness, use twm.

The guest tuning that makes XFCE bearable is in the overlay and is worth
keeping if you change things: xfwm4's compositor is off, GTK animations are
off, and window move/resize draw outlines rather than live content
(`overlay/etc/xdg/`). Each of those removes full-screen software repaints,
which cost twice here — once in the guest's CPU and again in the encoder that
has to carry every changed pixel to a streaming client.

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

# DOOM at 1024x768 on the shipped desktop: every measurement

Goal: Freedoom at ≥24 fps (ideally 30) on the **shipped Alpine desktop image at
1024x768**, on a live enclave deployment. An earlier answer that reached the
number by shrinking the workload — a DOOM-only session on a 320x200 screen —
was rejected, correctly: the machine stays the desktop at its real resolution
and the frame rate has to come from making the stack faster.

Everything below is measured. The negative results are the more useful half:
they are the paths that look obviously right and are not.

## Where it started and where it is

| | instructions/frame | fps |
|---|---|---|
| chocolate-doom 640x480 window, as the image ships | 8.5M | 16 |
| xdoom (raw X11), PutImage | 4.2M | 28 |
| xdoom, MIT-SHM | 2.9M | 45 (this workstation) |
| same, on kryptos | 2.9M | mean 26.9, median 25.8, floor 19.7 |
| + device tick 32, dirty-row upload, game at -O3 (0.7.5) | — | **mean 35.5, median 34.5, floor 25.7** |

Final, on the live deployment (`0x458a63b9…`, kryptos), by the game's own
wall-clock counter over 53 consecutive samples of 100 frames — about 2.6
minutes of unbroken gameplay, every frame through X (`0 direct / N via X`):

    min 25.7    p10 29.9    median 34.5    mean 35.5    max 66.4
    below 24 fps: 0 of 53

A later run with the desktop's terminal moved clear of the game window (fluxbox
was placing it across the lower-right corner, which also gave Xorg's blit a
multi-rectangle clip) — 129 samples, ~6.5 min:

    min 11.5    p10 28.5    median 33.1    mean 35.2    max 69.5
    below 24: 4 of 129

and the series says exactly what those four are:

    54.1  68.7  69.5 | 15.6  11.5 | 28.1  32.9  30.8  31.5 ...
       title screen  |  demo load |        gameplay

Every sub-24 sample is the three-to-six seconds where one demo ends and the
next loads (level load plus DOOM's melt wipe). **In gameplay the floor is
25.4.** Quote it that way; a single number hides which is which.

Method caveat: reading the game's log through the /console SSE can double-count,
because the stream replays scrollback before live output. Sample counts above
are therefore approximate; the distribution is not.

The goal is met on the shipped desktop at its real resolution: the floor
clears 24 and the median clears the 30 stretch. Of the three changes that took
it there from a 19.7 floor, the dirty-row upload was much the largest —
larger than its estimate, because DOOM redraws its whole 320x200 buffer every
frame and in most scenes a good part of it has not changed, not just the
status bar.

MIPS is flat at 76-79 across every fleet sample, so the 2x swing is DOOM's own
geometry (1.97M-3.94M instructions a frame), not the machine. The floor is a
genuinely heavy gameplay scene — screenshotted at the dip to be sure it was not
a menu or the demo-loop wipe.

## What paid

1. **The guest's clock was made of retired instructions.** `mtime` advanced one
   tick per instruction against a DTB promising 10 MHz, so a guest at 130 MIPS
   lived 13 seconds per real second: DOOM ran at 13x speed, its game logic did
   13x the work per frame, and every in-guest number was measured with a ruler
   that stretches. `realtime: true` (`Clint::set_wall_clock`, resampled every
   4096 instructions) sources it from the host clock. Also cuts the kernel's
   timer interrupt rate 13x. Costs determinism, so it is a knob and boot-bench
   grew `--realtime` to match.
2. **SDL's pixel path cost ~20 emulated instructions per presented pixel** — a
   converting blit through a generic scaler, then a copy through the X socket.
   Not the scaler: the same per-pixel cost was measured at 320x200 with no
   scaling at all. `guest/fbdoom/i_video_x11raw.c` speaks the X protocol
   directly (no Xlib), fuses the palette lookup into the scale (a doubled pixel
   pair is one 64-bit store), and uses MIT-SHM with two alternating buffers.
   8.5M -> 2.9M instructions a frame.
3. **The app's event loop slept 1 ms on a busy guest.** `busy` meant "something
   reached a client", not "the guest worked", so a guest that prints nothing —
   a game, a build, most of a boot — paid 1 ms on every ~6 ms batch. Fix is
   `busy = !parked`. App wasm 48 -> 66 MIPS, matching the emulator measured
   alone (boot-bench.wasm, 66).
4. **Display bands were full-width strips, and a slow watcher was disconnected
   rather than paced.** Bands now carry x/w (changed column range found by
   comparing frames 8 bytes at a time — row hashes can only say THAT a row
   changed), the row hash reads u64 instead of u8, and the scan gates on
   `Server::sse_backlog` so frames are produced at the rate the link takes
   instead of filling `MAX_WBUF` and closing the connection.
5. **`DEVICE_TICK_INTERVAL` 16 -> 32**: **+1.8%**, measured as four
   INTERLEAVED A/B pairs on two saved binaries (tick16: 87.4 88.5 87.7 87.9;
   tick32: 89.6 89.3 89.7 89.3 — consistent, the two sets do not overlap).
   Two earlier non-interleaved runs put this at +4.3-4.6% (87.2-87.4 -> 91.2);
   that was wrong. The same code measures 91.2 in one session and 88.5 in
   another, so a baseline taken minutes apart drifts about as much as the
   effect. **Interleave, or do not claim a single-digit percentage.** The
   README kept the interval at 16 because the UART drains on a 16-cycle
   cadence and 64 visibly corrupts the console; 32 was checked against a full
   boot's console output and is clean.
6. **xdoom uploads only the source rows DOOM redrew.** It redraws its whole
   320x200 buffer every frame including the status bar, which changes when the
   player's health does and not otherwise. Comparing 320 bytes a row is nothing
   against scaling and shipping 2560 of them.

## What did not pay — measured, do not repeat

- **Direct-to-framebuffer overlay** (write the scaled frame into `/dev/fb0`,
  skipping the server's copy out of shared memory). It works — the game reports
  "100 direct / 0 via X" — and it is not faster: same binary, same boot,
  24-40 fps with it against 35-40 without. It also owes correctness the copy
  gets for free: it paints outside the window whenever the window has moved and
  the origin has not been re-resolved, visible as a ghost on the desktop. Kept
  behind `-overlay`, off by default.
  *Caveat worth knowing:* a separate single observation on this workstation
  read 65-70 fps with the overlay on, but it was not a controlled A/B and was
  taken right after boot when the demo is in light scenes. The A/B is the
  better evidence; the discrepancy is unexplained and would need same-scene
  repetition to settle.
- **Unchecked memory accessors in the emulator** — the accessors already do an
  explicit `fits()` test and then index a slice, which checks the same bound
  again. Replacing the second check with an unaligned raw read: 88.6-90.8 vs
  91.2 MIPS. LLVM was already eliding it.
- **Packing the software TLB** into one array of `{tag, ppn, meta}` so a probe
  touches one cache line instead of three: 89.8 vs 91.2.
- **An 8x larger TLB** (512 -> 4096 sets): 87.1 vs 91.2. It is not thrashing;
  the bigger table just costs locality.
- **64-op superblocks** (`BLOCK_MAX` 32 -> 64): 88.0 vs 91.2.
- **More cpuShare** (0.13 -> 0.30, one transaction, measured): no change. The
  SET worker is running and doing its job — with a watcher attached the guest
  keeps rendering, where an inline scan would cost ~82% — but the emulator is
  one hart on one thread by construction, so extra cores have nothing to run.
- **`SDL_RENDER_SCALE_QUALITY=nearest`**: no change. The cost is the converting
  blit, not the filter.

## Traps that will fool the measurement

- `/status` MIPS is `instret / uptime` — a **lifetime average**, useless for
  A/B. Differentiate instret instead (`scratchpad/meter.py`).
- The app's `fps` divides framebuffer bytes by the **screen** frame, so a
  640x400 window on a 1024x768 screen reads 3.07x low — and with dirty-row
  uploads it reads lower still. The game's own counter (printed every 100
  frames, honest once `realtime` is on) is the number to quote.
- The serial console **drops the first byte after an idle line**; send a bare
  newline first.
- DOOM's demo loop varies 2x between scenes. Six to eight samples, or the
  number means nothing.
- boot-bench's fixed-instruction run is the only deterministic A/B rig for the
  emulator: `boot-bench.wasm <kernel> <rootfs> --ram-mib 1792 --insns 3_000_000_000`.
  It is deterministic in INSTRUCTIONS, not in wall time: the same binary
  measured 91.2 MIPS in one session and 88.5 in another. Build both arms,
  save them as separate .wasm files, and run them A/B/A/B — a control taken
  before a rebuild is worth nothing at this effect size.

## What is left

The gap is single-core interpretation speed. kryptos gives ~0.58x this
workstation on the identical build, and the interpreter is at its local optimum
(five separate attempts above, all neutral or worse). The only lever that
changes the category rather than the margin is the JIT in `PLATFORM-JIT.md` —
5.6x measured in the proto with per-access TLB probes — which needs the
platform `codegen` verb.

Separately, what a remote viewer sees is capped well below what the machine
renders: see `app-bandwidth-handoff.md`.

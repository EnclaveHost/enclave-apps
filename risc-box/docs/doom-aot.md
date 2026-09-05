# AOT profile for the fullscreen Doom workload

The default `emu/aot/regions.dump` contains 149 regions recorded from the
fullscreen 960×600 Doom overlay workload, with music and stereo sound enabled.
This changes emulator execution; it does not reduce display resolution.
Use the `aot` and application `set` features with wasm64 memory and shared-everything threads. All three are required for this workload.

The profile was collected with a maximum of 16 blocks per region. Keeping
regions small limits generated code size and gives the verifier alternatives
when a larger region includes code from several guest address spaces.
The guest used `alpine/fw_payload-gpu.elf`, `alpine/rootfs-uncap4.ext2.gz`,
ASLR disabled, and the completed-frame overlay frontend. Its executable SHA256
was `b96ad681efc98357b6987fa857ebf778e6ac21fd3602b32b9a93d47e3f17ca00`.
The profile SHA256 is
`15f1d4d674f877797839b87dc67047606e3ba716718ae3096fccdcb1197c93d9`.

A region runs only after its instructions and current address mappings verify
against guest memory. Other binaries and changed code fall back to the
interpreter. Failed verification must never count as executed AOT coverage;
`python3 emu/tests/aot_verifier.py` exercises mapping changes and that fallback.

The profile matches the palette-corrected frontend. Palette changes invalidate
unchanged-pixel reuse, so gamma changes and damage flashes repaint the world.

The completed-overlay baseline without AOT averaged 21.0 game FPS. The previous
148-region AOT build with duplicate-frame suppression averaged 29.6 game FPS
and 30.2 stream generation updates/sec. With this 149-region profile and the
device queue optimization, a complete demo1 run measured 31.46 game FPS and
31.55 stream generation updates/sec at 960×600, with sound/music and native
NVENC/Moonlight attached. Game FPS is the harmonic mean of 100-frame windows
with at least 98 direct frames. Stream generation counts can include band
updates; a nominal 60 FPS encoder does not imply 60 complete game frames.

The palette-only control measured 28.49 game FPS and 28.75 stream updates/sec,
though a compiler occupied another core during that control. These are single
runs, not a statistical estimate. Subjective smoothness still needs user review.

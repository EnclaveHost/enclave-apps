# AOT profile for the fullscreen Doom workload

The default `emu/aot/regions.dump` contains 148 regions recorded from the
fullscreen 960×600 Doom overlay workload, with music and stereo sound enabled.
This changes emulator execution; it does not reduce display resolution.
Use the `aot` feature together with the wasm64 shared-everything-threads build.

The profile was collected with a maximum of 16 blocks per region. Keeping
regions small limits generated code size and gives the verifier alternatives
when a larger region includes code from several guest address spaces.
The guest used `alpine/fw_payload-gpu.elf`, `alpine/rootfs-uncap4.ext2.gz`,
ASLR disabled, and the completed-frame overlay frontend. Its executable SHA256
was `66dcff8cf281ec5d99d9558f40e8d37cfdcdd8fb06fbb0f53f1de170629b909b`.
The profile SHA256 is
`df171ea805b80feaa4ba37c221e374d96462b10a09d4f4cb7e7412eae72e2181`.

A region runs only after its instructions and current address mappings verify
against guest memory. Other binaries and changed code fall back to the
interpreter. Failed verification must never count as executed AOT coverage;
`python3 emu/tests/aot_verifier.py` exercises mapping changes and that fallback.

In one local full demo1 comparison with NVENC/Moonlight attached, the completed
overlay baseline averaged 21.0 game FPS and this AOT profile averaged 30.5 FPS.
These are harmonic means of 100-frame windows with at least 98 direct frames,
excluding startup windows using X and mostly static windows. The native stream
still delivered about 24 distinct frames per second in the AOT test, so game
rendering speed alone does not establish streaming smoothness. Both tests used
960×600, the same guest snapshot, wasm64, SET, sound, and music.

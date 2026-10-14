# Moonlight on a single-vCPU isolated RISC Box

Measurements from 2026-09-27, using the published 0.6.54 SET64/AOT artifact,
a per-app SEV-SNP guest on EPYC 9115, a 960×600 desktop, and an authorized
local Moonlight viewer with an RTX 3070. These results **do not meet a smooth
30+ game-FPS target**. A 60 FPS encoder can duplicate a 20 FPS game image.

## Transport

The public app route took an unnecessary remote relay round trip even though
the isolated guest and viewer were on the same computer. A local ciphertext
splice retained the app's original TLS hostname, normal certificate trust,
and the SPKI verified against fresh AMD attestation. It did not terminate TLS
on the hosting side.

Twenty-five warm `/ping` samples on each path:

| Path | Median | p95 |
| --- | ---: | ---: |
| Public relay | 388.0 ms | 396.6 ms |
| Pinned local splice | 11.4 ms | 17.6 ms |

This is request latency, **not input-to-photon latency**. A separate 20-event
test injected input through the app API and observed the guest Linux input
device over authenticated SSH: median 60.9 ms, p95 71.8 ms. That includes the
SSH telemetry return and excludes Moonlight's input path and presentation.

Configure `GS_APP_CONNECT_ADDR` and `GS_APP_SPKI_SHA256` as documented in the
bridge README. A changed guest key requires fresh verification. Never learn
the expected pin from an unverified certificate. The TLS integration test
checks that bad pins, hostnames, or certificate trust send no credentials.

## Rendering and audio

The original desktop launcher selected an older executable and the X copy
path. The profile-matched completed-frame executable described in
`doom-aot.md`, with `-overlay -scaling 3`, avoids that copy. Keep the desktop at
960×600 and preserve sound/music. `guest/fbdoom/doom-fullscreen.sh` supplies
the full-screen launch and restores the desktop toolbar/cursor on exit;
`RISC_DOOM_BIN` selects the installed, verified executable.

For this local viewer, the bridge uses `--frames bands --fb 960x600` and client
NVENC. Pixels reach the authorized viewer through attested TLS before encoding.
This is not protected provider-GPU encoding. App-side CPU H.264 remains an
alternative for remote links; here it competes with the emulator for the
guest's single vCPU.

Three complete `-timedemo demo1` runs, with the same optimized executable,
sound/music, full desktop resolution, and Moonlight attached:

| Experiment | Aggregate game FPS |
| --- | ---: |
| High detail, normal CPU scheduling | 20.06 |
| Doom low-detail setting | 20.66 |
| High detail, vCPU pinned to one physical core | 20.15 |

These are single runs, not confidence intervals. Neither experiment gave a
convincing benefit, so high detail and normal CPU affinity were retained.
Earlier tests that switched modes mid-demo, or ran while another paused game
held the audio device, are excluded. Their faster numbers did not establish
full-audio gameplay performance.

Audio telemetry exposed a separate latency bug: samples collected during
connection setup remained queued after playback started, even beyond the
configured priming interval. The bridge now primes from the newest window.
Steady delivery gaps were usually 37–42 ms. A 60 ms priming experiment reduced
queued audio from roughly 150 ms to 60 ms, but an input/SSH stress test caused
one underrun with a 72 ms gap. The local setting was increased to 80 ms for
margin; the generic default remains 100 ms. Check underruns and trimmed audio
alongside latency before lowering this setting elsewhere.

## Remaining work

The isolated resource policy fixes this catalog version at one vCPU. Its SET
worker, emulation, TLS serving, and audio synthesis therefore share one core.
The next substantial experiment should compare properly measured isolated
guests with additional vCPUs. Increasing the advertised share alone does not
change this fixed guest shape.

Do not silently change an existing measured policy or skip verification to
get a faster result. A larger shape needs an explicit versioned policy, a new
derived identity and expected measurement, and verification before releasing
app configuration or secrets. Benchmark it with the same game, sound, desktop
resolution, and input load; measure actual frame delivery and not just encoded
packet counts. Keep a separate recovery snapshot before replacing gameplay.

# gs-bridge: a Moonlight/GameStream host for the RISC Box desktop

This is the native bridge that lets a real **Moonlight** client stream the RISC
Box desktop over NVIDIA's GameStream protocol. It speaks the whole protocol
itself — discovery, pairing, the HTTPS control surface, RTSP negotiation, RTP
video with Reed-Solomon FEC, and the encrypted ENet control channel — and wires
those to the app's two endpoints: `GET /fb.rgb` for frames and `POST /hid` for
input, which lands on the emulated virtio-input device.

The native adapter provides GameStream's TCP/UDP endpoints. It can either
repacketize H.264 encoded inside the isolated app (`--frames h264`), or encode
authenticated framebuffer updates on the **authorized viewing computer**
(`--frames bands` or `pull`). The latter receives plaintext pixels after TLS
verification, just as the browser viewer does. It must not run on an untrusted
provider machine: ordinary GPU encoding there would expose the desktop.

## Status: streaming works end to end

A real Moonlight client pairs, connects, and **decodes a live H.264 stream of
the emulated machine's desktop**, with input flowing back into the guest.
Verified against the actual RISC Box app (RISC-V Linux booted under wasmtime
from a minio-backed S3, running Xorg on its 1024x768 framebuffer):

```
[client] decoder setup: H.264 1280x720 @ 60 fps
[client] FIRST FRAME: 34236 bytes, type=IDR
frames_decoded: 485
idr_frames: 9
terminated: no (code 0)
```

Worth knowing how easy it is to fool yourself here: an earlier run of this
same test reported 826 frames and a clean teardown while the guest's
framebuffer was **entirely black** — the sample rootfs boots to a serial
console and never draws. A blank screen encodes, packetizes and streams
exactly as well as a desktop does, so frame counts alone prove the transport
and nothing about the picture. The tell is the frame size: under 1 KB for
black, tens of KB once there is a desktop on it. Build the guest from
`../guest/` if you want something on screen.

Input was confirmed against the running X server rather than assumed: driving
the pointer to two different positions puts the cursor at each one (pixels
appear in a previously-black region at the requested spot and vanish when the
pointer moves away).

The client is **moonlight-common-c** itself — the same protocol library
moonlight-qt links — driven headlessly so a decode can be counted rather than
merely rendered. Pairing was verified separately with stock **moonlight-qt
6.1.0**, which completes all four handshake phases plus the HTTPS
`pairchallenge` and then lists apps over TLS.

Every input class was verified reaching the guest: absolute and relative
pointer motion, the three mouse buttons, keyboard, and scroll — each accepted
by the app's `/hid` (`{"ok":true,"events":1}`).

**GPU encode**: the video is hardware-encoded on the GPU's NVENC engine
(`h264_nvenc` errors out rather than falling back to CPU, so a running stream is
itself proof), with the encoder engine measurably active during a session
(`nvidia-smi` encoder utilization non-zero throughout). Verified on the viewing
computer's RTX 3070. This is client-side encoding, not Enclave Shield GPU
offload and not evidence that a provider GPU can safely see plaintext.

## What it implements

| Port | Transport | Role |
|---|---|---|
| 47989 | TCP | discovery + the 4-phase pairing handshake |
| 47984 | TLS | `/serverinfo`, `/applist`, `/launch`, `/resume`, `/cancel` |
| 48010 | TCP | RTSP: OPTIONS, DESCRIBE, SETUP x3, ANNOUNCE, PLAY |
| 47998 | UDP | RTP video: NV_VIDEO_PACKET framing + Reed-Solomon FEC |
| 47999 | UDP | ENet control, AES-128-GCM both directions; input, IDR requests |
| 48000 | UDP | guest stereo audio, Opus with GameStream FEC |

The wire formats mirror Sunshine and moonlight-common-c exactly. Notable points
the protocol is unforgiving about, all learned the hard way:

- **`appversion` must end in a negative component** (`7.1.431.-1`). That is the
  only thing that makes the client's `IS_SUNSHINE()` true, which in turn enables
  the encrypted control stream, multi-block FEC, and the `control/13/0` stream id.
- **RTSP is plain TCP** at this version, one connection per request, and every
  response must be followed by a half-close — the client reads until EOF. Every
  response also needs a `CSeq` header, because the client's parser cannot
  terminate a message that has no headers at all.
- **An IDR frame is recognized by its access unit starting with an SPS**, not by
  containing an IDR slice. So SPS/PPS must be repeated ahead of every keyframe
  (`dump_extra=freq=keyframe`), the parameter sets must lead the IDR's access
  unit rather than trailing the previous frame, and **filler-data NALs must be
  stripped** — NVENC's CBR padding otherwise sits in front of the SPS and hides
  it, and the client silently drops every keyframe.
- **Client certificates are the authorization model**: pairing stores the
  client's cert, and the TLS listener admits only those, answering everyone else
  with the 401 XML body.

## Build and run

```
cargo build --release
./target/release/gs-bridge --app 127.0.0.1:8000            # a local RISC Box
./target/release/gs-bridge --app https://<id>.app.enclave.host   # one on the fleet
```

Options: `--app <url>` (host:port, `http://…`, `https://…`, or a path-prefixed
`https://<enclave-host>/x/<deployment-id>` for an enclave's DIRECT endpoint),
`--api-key <token>` or `RISCBOX_API_KEY` (if the app config sets `api_key`),
`--app-cookie <token>` or `GS_APP_COOKIE` (the deployment-scoped app token a
PRIVATE deployment's data path requires — minted by
`POST /v1/deployments/<id>/app-token`, sent as the gateway's `enclave_app`
cookie, the only owner proof that survives the relay), `--fb <WxH>`
(framebuffer size, default 1024x768), `--codec <name>` (default `h264_nvenc`),
`--state <dir>` (server identity and paired certs),
`--frames auto|bands|pull|h264|raw`, `--probe`.

**`--frames h264` avoids sending lossless pixels across a remote network**: the
APP encodes H.264 inside the enclave (minih264 on the SET worker; see
`../docs/moonlight-30fps-handoff.md`) and this bridge only repacketizes into
RTP — no NVENC, no re-encode, no mirror. Encoded frames may repeat an unchanged
game image, so the client's decode rate is not the game's render rate. Pixels
cross the wire as ~3 Mbps of video instead of 6-10 MiB/s of lossless bands.
Client IDR requests are forwarded to the app's `POST /video-key`. Measured
in an earlier fleet test: min 38 / median 40 encoded fps, 0 seconds below 30,
under continuous gameplay input. That is a transport measurement, not a
guarantee for a single-vCPU isolated deployment. `GSB_APP_KBPS` controls the app
encoder's bitrate (default 6400, bounded to 250–50000 kbit/s).

**Check the connection first.** `--probe` fetches one frame, says whether it
matches `--fb`, and reports whether anything is actually drawn on it; add
`--frames bands` to prove the mirror rather than the connection, and set
`GS_PROBE_PPM=/tmp/f.ppm` to write the frame out and look at it.

```
$ gs-bridge --app https://<id>.app.enclave.host --frames bands --probe
[screen] mirroring 1024x768 from /display
[probe] mirrored 2359296 bytes after 1 bands
[probe] mirror has 16 distinct colours in a sample
```

### Where frames come from, and why it matters remotely

`GET /fb.rgb` hands over a whole framebuffer. Beside the app that is the right
answer — no state, no protocol. Across a network it is hopeless: the frame is
2.25 MiB and one measured **2.9 seconds** from a deployment on the fleet, about
a third of a frame per second.

So a remote bridge mirrors the app's **`/display` band stream** instead. The app
already scans its framebuffer, finds the rows that changed and ships them
deflated; gs-bridge holds that stream open, applies each band to a local copy,
and the encoder reads that copy as a memcpy. Traffic becomes proportional to
what moved rather than to frame rate:

| source | mostly-idle desktop | at 30 fps |
|---|---|---|
| `/fb.rgb` per frame | 68 MiB/s | 68 MiB/s |
| `/display` bands | **479 bytes/s** | proportional to change |

`--frames` defaults to `auto`: bands for an `https://` app, raw for a local one.

Bands contain the guest's pixels, compressed rather than protected from the
recipient. Send them only over authenticated, attested TLS to an authorized
viewer. Encoding inside the isolated CPU keeps that work within its boundary;
the SET build has a worker thread, but on a one-vCPU guest the worker still
competes with emulation and audio for the same core.

#### A ceiling worth knowing about before you tune anything

When the bridge runs on your own machine, **Moonlight cannot be more responsive
than the browser tab**, and it is worth being blunt about why: both are fed by
the same `/display` band stream. The browser inflates a band and blits it. The
bridge inflates the same band, then adds an H.264 encode, a packetize, a UDP
hop, a decode and a present. Same source, strictly more work — so a local
bridge buys the GameStream input path and client ecosystem, not lower latency.

The arrangement where Moonlight wins is the one where the encoder sits next to
the framebuffer, inside the enclave, and the band stream is never in the loop.
That is the `nvenc` verb, and this ceiling is the strongest argument for it.

Two settings do matter while the bridge is local:

* **Stream at the framebuffer's own size, 1024x768.** Anything else makes
  ffmpeg resample every frame on the CPU, which is the most expensive thing
  this process does, to produce a picture strictly worse than the original.
  The bridge logs a line when it catches itself doing this.
* **Frame rate is set upstream, not here.** The guest can only repaint so fast,
  and the app only scans when the picture moves; the encoder's `new frames/s of
  encoded/s` line every ten seconds says which of the two is the limit.

Pairing with a real client, with the PIN pre-seeded so it can run unattended:

```
curl 'http://127.0.0.1:47989/pin?uniqueid=0123456789ABCDEF&pin=1234'
moonlight pair <host-ip> --pin 1234        # -> *** PAIRED ***
```

(The `/pin` endpoint is a headless-test convenience for delivering the PIN that
Moonlight would normally show in its UI; a real deployment would surface it to
the operator.)

`vendor/enet/` is Moonlight's ENet fork (MIT, commit `aca8784`), vendored and
linked so the control channel is wire-compatible by construction rather than by
reimplementation.

## Architecture

```
  Moonlight client
        │  GameStream (pair/HTTPS/RTSP/RTP/ENet)
        ▼
   gs-bridge  ──GET /fb.rgb──▶  RISC Box app (WebAssembly)
        │                              │
        │  NVENC encode on the GPU     │ emulated RISC-V machine
        └──POST /hid───────────────────▶ virtio-input HID
```

Frames are pulled from the app, hardware-encoded, split into access units,
packetized into RTP shards with parity, and paced onto the wire. Input arrives
on the encrypted control channel and is translated into the app's `/hid` schema
— which means mapping Moonlight's **Windows virtual-key codes onto Linux
keycodes**, and integrating relative mouse motion into an absolute position,
since the emulated pointer is absolute-only.

Audio streams from the guest's virtio sound device through `/audio?stream=1`.
The bridge resamples it to 48 kHz stereo and encodes 5 ms Opus packets at a
constant 128 kbit/s. Silence uses the same packet size and duration. Audio RTP
timestamps count milliseconds, and recovery uses the fixed GameStream audio
matrix, which differs from the video parity matrix. Variable packet sizes
cause Moonlight to disable audio recovery.

`cargo test` checks codec duration, decoding, encrypted packet sizes, and
resampler continuity. To verify loss recovery against the actual client, use a
Moonlight common C checkout with its submodules:

```sh
python3 tests/audio_fec_interop.py /path/to/moonlight-common-c
```

This test drops every pair of data packets and checks the recovered bytes and
timestamps, including sequence number wrap. It was verified against
`moonlight-common-c` commit `874ac9548f1bd6f095ef2b435c42cdde460e7821`.

## What is not done

- **Provider GPU encoding.** This bridge does not implement protected GPU
  offload. Keep a framebuffer-encoding bridge on the authorized viewer.
- **HEVC/AV1.** DESCRIBE deliberately advertises H.264 only. The codec markers
  the client greps for are understood, so adding them is mostly encoder work.
- **Gamepad, touch, and pen** input is parsed and dropped — the emulated HID has
  no equivalent device.

### Local Moonlight endpoint

Set `GS_BIND_ADDR=127.0.0.1` to bind every HTTP, HTTPS, RTSP, video,
audio, and ENet control socket to loopback. Invalid IPv4 values fail at
startup. Without this setting the historical LAN binding is retained.
For a remote app that already encodes H.264, use `--frames h264`; this
repacketizes the app stream without local re-encoding. Supply app credentials
through `RISCBOX_API_KEY`, rather than command-line arguments.

For a local ciphertext splice to the guest, `GS_APP_CONNECT_ADDR=127.0.0.1:PORT`
changes only the TCP destination. It requires an HTTPS app URL and
`GS_APP_SPKI_SHA256` set to the transport key hash verified by the enclave
attestation client. TLS still validates the original URL's hostname and CA
chain; the bridge also checks the pinned SPKI on every connection before
sending credentials. A replaced guest/key requires fresh attestation and a
new pin; there is no unverified fallback.

The video source log includes 10-second arrival-gap statistics, separate from
game render FPS. Client-side `--no-vsync --no-frame-pacing --fps 60` avoids
intentional presentation buffering. Input pointer cadence and minimum key
hold are tunable with `GSB_CURSOR_MS` and `GSB_KEY_MIN_HOLD_MS` respectively;
the latter should stay long enough for the guest game to register short taps.

`GSB_AUDIO_BUFFER_MS` sets audio priming between 20 and 200 ms (default 100).
The audio log reports chunk gaps, queued duration, discarded backlog, and
sound/silence counts. Reduce buffering only after measuring the actual path;
a buffer shorter than delivery gaps introduces underruns instead of lower
usable latency. Startup and underrun recovery discard old handshake backlog
and prime from the newest audio window before playback resumes.

Run `python3 tests/attested_route.py /path/to/gs-bridge` to exercise the pinned
route against a local TLS peer. It checks that wrong keys, wrong hostnames,
untrusted certificates, missing pins, and plaintext overrides send no HTTP
request or credentials. This complements attestation verification; it does
not itself verify an AMD report.

# gs-bridge: a Moonlight/GameStream host for the RISC Box desktop

This is the native bridge that lets a real **Moonlight** client stream the RISC
Box desktop over NVIDIA's GameStream protocol. It speaks the whole protocol
itself — discovery, pairing, the HTTPS control surface, RTSP negotiation, RTP
video with Reed-Solomon FEC, and the encrypted ENet control channel — and wires
those to the app's two endpoints: `GET /fb.rgb` for frames and `POST /hid` for
input, which lands on the emulated virtio-input device.

Why native (not in the wasm app): GameStream needs plain-HTTP + HTTPS control
surfaces, UDP RTP/ENet transports, and hardware video encode. That belongs in a
native process running where a GPU is reachable — the same place the H200 NVENC
path lives (see `../docs/encode-path-handoff.md`) — not inside the
`wasm32-wasip2` sandbox.

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
(`nvidia-smi` encoder utilization non-zero throughout). Verified on an RTX 3070,
the local test GPU; production encode is the fleet's **H200**, where the NVENC
API is identical.

## What it implements

| Port | Transport | Role |
|---|---|---|
| 47989 | TCP | discovery + the 4-phase pairing handshake |
| 47984 | TLS | `/serverinfo`, `/applist`, `/launch`, `/resume`, `/cancel` |
| 48010 | TCP | RTSP: OPTIONS, DESCRIBE, SETUP x3, ANNOUNCE, PLAY |
| 47998 | UDP | RTP video: NV_VIDEO_PACKET framing + Reed-Solomon FEC |
| 47999 | UDP | ENet control, AES-128-GCM both directions; input, IDR requests |
| 48000 | UDP | RTP audio (silent Opus; the guest has no sound device) |

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

Options: `--app <url>` (host:port, `http://…` or `https://…`), `--api-key
<token>` or `RISCBOX_API_KEY` (if the app config sets `api_key`), `--fb <WxH>`
(framebuffer size, default 1024x768), `--codec <name>` (default `h264_nvenc`),
`--state <dir>` (server identity and paired certs), `--frames auto|bands|raw`,
`--probe`.

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

One thing this does NOT fix: raw frames still leave the enclave. The bands are
the guest's own pixels, just compressed. Encoding *inside* the enclave — the
`nvenc` verb specced in `../PLATFORM.md` — is what would keep pixels in and emit
H.264 directly, and it has a second argument in its favour now: the app is
single-threaded (wasip2 cannot spawn one, on p2 or p3), so anything encoded in
there competes with the emulator for the only core it has.

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
   gs-bridge  ──GET /fb.rgb──▶  RISC Box app (wasm32-wasip2)
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

## What is not done

- **Production H200 deploy.** The encode is GPU-agnostic NVENC, but placing this
  service on the fleet GPU node next to the RISC Box CVM is an operational step
  that needs access to that node.
- **Audio is silence.** The emulated machine has no sound device; the stream
  exists so the client's audio path stays healthy.
- **HEVC/AV1.** DESCRIBE deliberately advertises H.264 only. The codec markers
  the client greps for are understood, so adding them is mostly encoder work.
- **Gamepad, touch, and pen** input is parsed and dropped — the emulated HID has
  no equivalent device.

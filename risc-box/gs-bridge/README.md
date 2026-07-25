# gs-bridge: a Moonlight/GameStream host for the RISC Box desktop

This is the native bridge that lets a real **Moonlight** client stream the RISC
Box desktop over NVIDIA's GameStream protocol. It's the counterpart to the
in-app pieces: the app already produces an efficient **AV1** video stream
(`GET /video`) and accepts input (`POST /hid`, backed by the emulated
virtio-input HID). gs-bridge speaks the GameStream protocol to Moonlight and
wires those two together.

Why native (not in the wasm app): GameStream needs a plain-HTTP + HTTPS control
surface, UDP RTP/ENet transports, and (for real speed) hardware video encode.
That belongs in a native process running where a GPU is reachable, the same
place the H200 NVENC path would live (see `../docs/encode-path-handoff.md`),
not inside the `wasm32-wasip2` sandbox. The bridge pulls frames from the app's
`/video` and posts input back to `/hid`.

## Status

**Pairing works and is verified against a real client.** A stock
**moonlight-qt 6.1.0** discovers this host, runs all four phases of the
GameStream pairing handshake, and both cryptographic checks pass:

```
[gshost] phase2 clientchallenge ok
[gshost] phase3 serverchallengeresp ok
[gshost] *** PAIRED *** (hash_ok=true sig_ok=true)
```

After pairing, Moonlight advances to `/applist` (a paired-only request) and the
host reports `PairStatus=1`. The pairing crypto mirrors Sunshine's
`src/nvhttp.cpp` + `crypto.cpp` exactly:

- `getservercert`: `aes_key = SHA256(salt ‖ pin)[:16]`; return the server cert.
- `clientchallenge`: `resp = AES128-ECB( SHA256(clientChal ‖ serverCertSig ‖
  serverSecret) ‖ serverChallenge )`.
- `serverchallengeresp`: return `serverSecret ‖ RSA-SHA256-sign(serverSecret)`.
- `clientpairingsecret`: verify `SHA256(serverChal ‖ clientCertSig ‖ secret) ==
  clientHash` **and** RSA-verify(clientCert, secret, sign).

## GPU compute: NVENC hardware encode on the H200

The RISC Box app's GPU compute is the video **encode**, and it runs on the GPU's
NVENC engine, off the emulated CPU and off the wasm app. In production this
runs on the fleet GPU node's **H200** (co-located with the RISC Box CVM); the
NVENC API is identical on a dev GPU, so a pipeline verified locally is the H200
path. The frame source is the app's `GET /fb.rgb` (raw 800×600 RGB); the native
bridge pulls it and NVENC-encodes it (`encode-nvenc.sh`, the encoder gs-bridge
feeds into the video stream).

**Verified on an RTX 3070** (the local test GPU; production is the H200): pulling
the RISC Box desktop from `/fb.rgb` and encoding with `h264_nvenc` drove the
GPU's **NVENC engine to 100% utilization**, producing valid yuv420p H.264 that
decodes to the desktop. `h264_nvenc` errors out if NVENC is unavailable (it
never falls back to CPU), so a successful encode is itself proof the GPU did the
work. The 3070 also exposes `av1_nvenc` and `hevc_nvenc`.

## What's implemented vs. remaining

Implemented: the GameStream HTTP control surface for discovery + pairing,
`/serverinfo` (so Moonlight lists the host) and `/pair` (the 4-phase handshake),
on HTTP :47989. Session state, self-signed server cert, and the exact crypto.
Plus the GPU encode (NVENC) of the desktop, the GPU compute, verified above.

Remaining for actual video streaming (the larger piece):

- **HTTPS :47984** for post-pair requests (`/applist`, `/launch`, `/resume`),
  using the paired cert. Moonlight moves here right after pairing.
- **RTSP handshake** (:48010) negotiating the streams.
- **RTP video** (:47998): packetize the app's AV1 frames in Moonlight's video
  packet format with Reed-Solomon FEC. Modern Moonlight/Sunshine support AV1,
  so the app's existing `/video` output is the source; no H.264 needed for a
  browser-grade client. (H.264/HEVC via H200 NVENC remains the path for maximum
  compatibility/speed; see `../docs/encode-path-handoff.md`.)
- **ENet control** (:47999, AES-GCM): input + keepalives. Input maps to the
  app's `POST /hid`.
- **Audio** (:48000): Opus over RTP (optional).

## Run / reproduce the pairing test

```
cargo build --release
./target/release/gs-bridge          # GameStream host on :47989

# with a real Moonlight client (moonlight-qt), pin fixed so both sides agree:
curl 'http://127.0.0.1:47989/pin?uniqueid=0123456789ABCDEF&pin=1234'   # pre-seed the pin
moonlight pair <host-ip> --pin 1234                                    # pairs -> "*** PAIRED ***"
```

(The `/pin` endpoint is a headless-test convenience for delivering the PIN that
Moonlight would normally show in its UI; a real deployment would surface the PIN
to the operator.)

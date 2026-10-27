# Owner-local native desktop transport

`native-local-desktop.mjs CONFIG.json` connects an owner workstation to an
already admitted native desktop through its existing SSH host access. It
requires the guest's valid WebPKI certificate, hostname and exact instance SPKI
SHA-256 before sending the existing owner credential. It does not perform a
new hardware attestation or replace platform admission. Configure its pin only
from an independently verified deployment; a VM replacement must be reverified.
No certificate error or changed key is accepted automatically.

The config has `hostname`, `spkiSha256`, `sshTarget`, `guestPort`,
`credentialsFile`, `stateFile`, optional `listenPort` and optional
`moonlightAddress` (127.0.0.2 through 127.0.0.9). The credential file contains
`KVMBOX_OWNER_PASSWORD`. The private state file contains the launch URL; do not
publish it. The HTTP viewer is loopback-only, requires a random HttpOnly
SameSite cookie, checks the exact Host/Origin and rejects cross-site requests.
It only proxies desktop GET assets and the noVNC WebSocket, never `/exec` or
control APIs. HTTP loopback traffic never crosses the workstation's network.
Browser policy may prohibit opening this local viewer; do not bypass it.

The optional Moonlight listener exposes only the existing fixed GameStream
ports on the chosen loopback address. All host-crossing bytes go through SSH
and the validated, instance-pinned guest TLS channel. The native route has the
`/desktop` prefix. Sunshine's own media encryption remains enabled. Pair the
local client through the authenticated guest `/desktop/api/moonlight/pair`
endpoint, then verify pairing with `moonlight list ADDRESS`.

A supervised connector can recover an SSH connection loss. Its configuration
is intentionally bound to one admitted instance and fails closed if the
instance key changes. No remote firewall or public ingress changes are needed.

For a 960x600 desktop use:

```
moonlight stream ADDRESS 'KVM Box Desktop' --resolution 960x600 --fps 30 \
  --bitrate 8000 --video-codec H.264 --display-mode windowed \
  --absolute-mouse --no-vsync --performance-overlay --no-quit-after
```

30 fps is a requested target, not a measured delivery claim. Inspect the client
performance overlay during moving video. The guest currently encodes through
software libx264. This transport does not expose an unmasked host GPU.

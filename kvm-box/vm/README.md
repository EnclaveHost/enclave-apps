# KVM Box as a whole VM

One SEV-SNP guest that runs the KVM Box desktop on a host with the platform's whole-VM path (catalog runtime
`sev-snp-vm`, the path Enclave RPC runs on: `rpc/snp/vm-manager.mjs` and `rpc/snp/launch.py` in the platform
repository). It is a third way to run the desktop, beside the native NucBox profile (`../guest/native`) and the
original two-guest broker (`../host`).

Status, 2026-11-25: the image builds and was booted on metal0 under SEV-SNP outside any lease (below). No catalog
version names it and no deployment runs it.

## What is in the guest

| Part | From | Covered by |
| --- | --- | --- |
| kernel | Ubuntu 6.8.0-147, the RPC guest image's kernel, unchanged | launch measurement |
| `initrd.img`: `vm-init` and the dm-verity and overlay modules | `vm-init.c` | launch measurement |
| kernel command line: root hash and geometry, model volume root hash | `build_image.py` | launch measurement |
| `root.img`: the desktop root, the rest of the modules, `/vm` | the native desktop root, `start`, `app.ports` | dm-verity, root hash on the command line |
| model volume (optional) | `metal/volumes.mjs` | dm-verity, root hash on the command line |

`vm-init` finds its disks by virtio serial, opens them through dm-verity, mounts the root read-only under a tmpfs
overlay and runs `/vm/start`. Every write is guest memory and is gone at power-off: nothing persists between boots.

`/vm/start` starts the same desktop services as the original single-guest script (`../guest/desktop-start.sh`: Xorg
on the dummy driver, fluxbox, PulseAudio, Sunshine with software encoding, x11vnc and noVNC, the helper that serves
the owner page) and ends in the platform's whole-VM front (`isolation/m2/vmfront`, platform main 2a2da5faa), which holds the TLS key, serves
the attestation and the certificate exchange, passes HTTP to the helper and serves the ports in `app.ports` as
protected-port tunnels (GameStream).

## What it does not have

- No GPU. Sunshine encodes in software and the models run on the guest's CPUs. The Shield service and its worker
  channel are not started: a whole VM has no route to a host's GPU workers yet.
- The native profile's parts are in the root but unused: the Wasm controller and its owner page, the gateway and
  inner services, the automation API, the supervisor's start/stop of the desktop. The owner page is the helper's.
- The owner password is fixed at build: its SHA-256 is in the root (`/access.sha256`), as in the original
  single-guest build. The image is public, so the password must be long and random; the builder makes one when the
  file it is pointed at does not exist. Sealed delivery of an owner-chosen secret, which the native profile has, is
  not built for whole VMs.

## Build

```
python3 -I kvm-box/vm/build_image.py SPEC.json
```

The docstring lists the fields. The inputs used on 2026-11-25: the SVN26 native root unpacked with the pinned
`unsquashfs`, the kernel and `root.img` of the RPC image r17, the pinned `mksquashfs`, and the model volume
`kvm-box-public-models-v5` (the 27B).

## Tried on metal0 (2026-11-25 08:09Z)

Launched with `rpc/snp/launch.py` (8 vCPUs, 16 GiB, the model volume as a read-only disk, HOST_DATA a test label):

- The launch measurement `sev-snp-measure` predicts from OVMF, kernel, initramfs and command line was the one in
  the AMD-signed report.
- The platform's attested fetch (`rpc/snp/attested-fetch.mjs`) accepted the front: measurement, HOST_DATA, the
  served key. A fresh report for a caller's nonce and the certificate request for the app name were served.
- The owner page answered 401 without a password. A TCP tunnel to port 47989 returned Sunshine's `serverinfo`.

Not tried: a Moonlight stream, the 27B from inside the guest, a certificate from the platform, a launch by the
host's VM manager under a lease, more than a few minutes of uptime.

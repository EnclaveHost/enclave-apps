# KVM Box as a whole VM

The KVM Box desktop as one app image for the platform's whole-VM path: a root filesystem, its dm-verity tree and
`APP.json`. The image names no kernel and no platform. The platform wraps it for a host type (kernel, init, kernel
modules, launch identity: `isolation/vmboot` in the platform repository), so the same root hash is to run on
SEV-SNP hosts such as metal0 and on Enclave Shield hosts such as the NucBox. It is a third way to run the desktop,
beside the native NucBox profile (`../guest/native`) and the original two-guest broker (`../host`), and is meant to
replace the native profile once it has run on both host types.

Status, 2026-11-25: the image builds; wrapped for SEV-SNP it booted on metal0 outside any lease (below). It has
never run on a Shield host: the platform has no wrapper for one yet. No catalog version names it and no deployment
runs it.

## What is in the image

| Part | From |
| --- | --- |
| the desktop root | the native desktop's root image, unpacked |
| `/vm/start` | `start`: the services of the original single-guest script (`../guest/desktop-start.sh`: Xorg on the dummy driver, fluxbox, PulseAudio, Sunshine with software encoding, x11vnc and noVNC, the helper that serves the owner page), ending in the front |
| `/vm/vmfront` | the platform's whole-VM front (`isolation/vmboot/front`): TLS key, attestation from whichever report source the boot offers, certificate exchange, HTTP to the helper, the ports in `app.ports` as protected-port tunnels (GameStream) |
| `/helper`, `/access.sha256` | built from `../guest`; the owner password's hash |
| `APP.json` | root hash and geometry, the model volume's, the kernel modules the desktop needs (`uhid`, `hid-generic`, `nf_tables`), the ports |

The platform's init mounts the root read-only under a tmpfs overlay: every write is guest memory and is gone at
power-off. `start` brings up a network card when the platform gives the guest one and runs without when it does
not.

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
`unsquashfs`, the pinned `mksquashfs`, and the model volume `kvm-box-public-models-v5` (the 27B). Then, in the
platform repository, `python3 -I isolation/vmboot/build_boot.py SPEC.json` wraps the result for a platform (used:
`sev-snp` with the kernel of the RPC image r17).

## Tried on metal0 (2026-11-25, platform main a1a8a0415)

Wrapped for SEV-SNP and launched with `rpc/snp/launch.py` (8 vCPUs, 16 GiB, the model volume as a read-only disk,
HOST_DATA a test label):

- The launch measurement `sev-snp-measure` predicts from OVMF, kernel, initramfs and command line was the one in
  the AMD-signed report.
- The platform's attested fetch (`rpc/snp/attested-fetch.mjs`) accepted the front: measurement, HOST_DATA, the
  served key. A fresh report for a caller's nonce and the certificate request for the app name were served.
- The owner page answered 401 without a password. A TCP tunnel to port 47989 returned Sunshine's `serverinfo`.

Under plain KVM, with the root and its hash tree presented the way a Hyper-V host presents disks (labelled ISO
containers on SCSI DVD drives): the desktop started from the same image.

Not tried: anything on a Shield host, a Moonlight stream, the 27B from inside the guest, a certificate from the
platform, a launch by a host's VM manager under a lease, more than a few minutes of uptime.

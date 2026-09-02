# Instant boot from a snapshot — handoff (2026-09-01)

RISC Box 0.6.46 can serialize a running machine and resume it. On the
e64f7cba deployment (Alpine DOOM desktop, 1792 MiB, 960x600, realtime) the
guest's own boot is a couple of minutes of emulated time; a resume is the
snapshot download plus an inflate.

## What shipped

- `emu/src/snapshot.rs` — the format: tagged sections, `Ser`/`De` byte codec,
  sparse+deflated RAM, disk delta (only blocks the guest wrote), `FORMAT`
  constant. **Bump `FORMAT` whenever a device's `snapshot()`/`restore()` pair
  changes what it writes**; every reader checks section lengths, so unbumped
  drift fails loudly instead of resuming a subtly wrong machine.
- every device (`clint`, `plic`, `uart`, `virtio_*`, `opl`) has
  `snapshot()`/`restore()`; the CLINT re-bases its wall clock so `mtime`
  continues from the snapshot's value (timer deadlines are absolute);
  `virtio_block_disk` now tracks dirty blocks at 4 KiB.
- `Emulator::snapshot(identity, level)`, `Emulator::restore(bytes, identity)`,
  `Emulator::snapshot_meta(bytes)`; `Emulator::seed_rng` gives every cold
  boot a fresh DTB `rng-seed` (the blob shipped one fixed seed).
- app: config keys `snapshot`, `snapshotSaveKey`, `snapshotLevel`,
  `restoreExec`, `restoreExecTimeoutS`; `POST /snapshot`; `/start
  {"snapshot":false}`; `/status.snapshot`; the snapshot is fetched with the
  images (404 = cold boot, other errors retry with them) and cached beside
  them so a `/stop`+`/start` resumes without a download.
- `boot-bench --snapshot-on MARKER:FILE` / `--restore FILE` for measuring
  without S3.

A snapshot is bound to an identity string: sha256 of the kernel and fs
objects as fetched, the dtb, `ramMiB`, display geometry and `realtime`. A
mismatch is logged and the machine boots cold. Overwriting the base image in
R2 (as was done for the xdoom fire fix) therefore invalidates every snapshot
taken against it — take a new one afterwards.

## Measured

Sample Buildroot image, wasm under wasmtime, local minio (`scratchpad/snaptest.py`):

| step | time |
|---|---|
| cold boot to console prompt | 3.1 s |
| `/snapshot` (256 MiB guest, 6785 non-zero pages) | 13.5 MB in 0.29 s |
| `/stop` + `/start` resume (cached) | 0.74 s |
| fresh process: fetch snapshot from S3 + resume | 0.77 s |

Alpine DOOM desktop (the e64f7cba images), wasm under the SET engine locally
against R2: see the numbers section at the end (filled from the run).

## Deploying to e64f7cba (owner wallet 0x0b2d…, Steven)

The object `alpine/desktop.snap` in the `machines` bucket was taken from the
exact images and settings the deployment runs (kernel `alpine/fw_payload-gpu.elf`,
fs `alpine/rootfs-uncap4.ext2.gz`, 1792 MiB, 960x600, realtime), so it matches
the deployment's identity as long as those objects are not overwritten.

1. Publish 0.6.46 (the SET build, `set/risc-box-set.wasm`), with the current
   version config plus the two new keys:

   ```json
   "snapshot": "alpine/desktop.snap",
   "restoreExec": "date -s @{epoch} >/dev/null; echo {entropy} > /dev/urandom"
   ```

   `enclave publish set/risc-box-set.wasm --slug risc-box --version 0.6.46 --config "$(cat <config>)"`

2. `enclave upgrade e64f7cba 0.6.46`. The next start fetches
   kernel + fs (as before) + the snapshot and resumes. `/status.snapshot.restored`
   says whether it did; the log line is `machine RESUMED from snapshot`.

3. To re-take the snapshot from the live box (e.g. after a rootfs change):
   `POST /snapshot` with the api key. It uploads with the deployment's S3
   credentials, so those need write access to the key (the temp R2 creds used
   for the 2026-09-01 upload should be rotated).

If the config override path is preferred over a version config edit,
`enclave config set e64f7cba` with the two keys works the same way — the app
reads them from `ENCLAVE_CONFIG` at process start.

## Instances (2026-09-02): many machines in one process

Beyond resuming one machine, the app now hosts N of them, forked from a root
snapshot — the "each chat gets its own VM from a root image" shape.

- `emu/src/memory.rs`: guest RAM is 64 KiB chunks in three states (untouched
  = reads zero and costs nothing; Shared = an `Arc` from an image, copied on
  first write; Owned). Flat read/write pointer tables keep the hot path at
  one pointer load + one unaligned load (measured ~3% slower than the old
  contiguous Vec on the CPU-bound boot phase). `RamImage` is the shareable
  page set a snapshot inflates to; `Memory::adopt` is O(chunks) refcount bumps.
- `emu/src/device/virtio_block_disk.rs`: the disk is a shared `Arc<Vec<u8>>`
  base plus a per-machine overlay of written 4 KiB blocks (the overlay IS the
  snapshot delta).
- `Emulator::load_image` (inflate once), `restore_image` (fork), `image()`
  (publish a LIVE machine as a fork root in milliseconds, no bucket),
  `setup_filesystem_shared`, `footprint()`.
- app: `Machine` struct + `App.machines` (index 0 = `main`), `/i/<id>/…`
  routes, `GET|POST /instances`, `DELETE /i/<id>`, config `instances.{max,
  maxBytes}`, round-robin scheduler (busy machines split TICK_BATCH, floor
  MIN_BATCH; parked ones take IDLE_BATCH), per-instance console topics
  `console:<id>`, per-instance NetStack (outbound only), the restoreExec hook
  on every fork. Streams/GameStream/forwards/`/save` stay on `main`.
- Snapshot identity no longer includes `ram:` (an instance's RAM size is the
  image's; the emulator checks it) — the `alpine/desktop.snap` object was
  re-taken with the new identity string.

Measured: INSTANCES-NUMBERS-TBD

## Caveats

- A resumed guest's wall clock and random pool are the snapshot's; the
  `restoreExec` hook above fixes both, but only if a shell answers on
  `ttyS0` (it does on the Alpine image). Every restore of one snapshot shares
  the kernel CRNG state until that hook (or interrupt entropy) diverges it —
  the same class of exposure the fixed DTB `rng-seed` gave every cold boot
  before 0.6.46.
- TCP connections open in the guest at snapshot time are dead on resume
  (they were host sockets). Take the snapshot when the machine is quiet; the
  NIC, its lease and the forwards carry over.
- The base fs object is still downloaded on every start: the snapshot holds
  only changed blocks, which keeps it small. A self-contained snapshot
  (full disk inside) would trade that for a larger object.
- `readOnly: true` disables `/snapshot` as it disables `/save`.
- Fixed on the way (2026-09-02): the UART receive interrupt was
  EDGE-triggered (upstream), and the PLIC pending bit is cleared by the
  guest's claim-complete while the CPU's SEIP is cleared on delivery — so a
  byte landing between the serial ISR's last LSR read and its completion was
  never signalled again: it sat in RBR, blocked every byte behind it, and a
  guest parked in WFI slept forever. It hit whenever a pasted line's cadence
  aligned with the ISR (2 of 6 native timings; every app resume that ran the
  restoreExec hook). The line is level-triggered now, like the virtio ones
  (`uart.rs` tick, `plic.rs` tick).
- Fixed on the way: `/exec` answered by connection INDEX while its own
  console pumping flushed the server, and a flush that reaped any earlier
  connection compacted the list under it — the response was dropped as
  "stale key" (seen as a hung exec). It now parks the request by ticket
  (`hold`/`release`, the long-poll mechanism) and held connections are
  exempt from the idle reaper.

## Numbers from the Alpine run

Local run against R2 (`wasmtime-set` 49 engine, this workstation, ~80 guest
MIPS; the deployment's exact config: 1792 MiB, 960x600, realtime, gpu kernel +
rootfs-uncap4):

| step | measured |
|---|---|
| cold boot until DOOM was rendering (fps >= 2) | ~2.5 min (10.9 G instructions), plus the 215 MB fs download |
| `/snapshot` | 54.5 MB in 1.8 s: 37066 of 458752 RAM pages non-zero (145 MB), 128 disk blocks changed |
| upload to R2 | 4.7 s |
| `/stop` + `/start` resume (cached) | restored in 0.46 s |
| `restoreExec` hook (date + entropy) | 1.0 s |
| fresh process: fetch kernel + fs + snapshot from R2, gunzip the fs (215 -> 671 MB), resume, run the hook | 23.9 s end to end (restore itself 0.86 s, hook 1.0 s); `/exec` then reports `Linux alpine-riscbox 5.15.164` with the clock corrected |

So on the fleet a start is dominated by the downloads (215 MB fs + 55 MB
snapshot) rather than the guest's boot. Roughly a quarter of the resident
guest RAM is page cache of files that also sit in the rootfs; de-duplicating
those pages against the disk (reference a block instead of storing the page)
is the obvious next cut in snapshot size if it matters.

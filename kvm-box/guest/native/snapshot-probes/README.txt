Disposable Linux guest snapshot qualification probes

These are PID-1-only diagnostic init programs, not native app endpoints. Never
run them in an existing application guest. They require a separately created
synthetic guest and an independent host watchdog with exact-owned-ID cleanup.

Availability: tested with kernel f401ccc30759c48c29a63bdb36e9cd7b09b840e863016080716b644939870da8
and OpenHCL initrd 71a91101f8f9b870199946076db2b220c68167e029539d4619a6b7600432124e.
/dev/snapshot opened and closed successfully. /sys/power/state included disk;
/sys/bus/vmbus/hibernation was 0. No freeze/restore ioctl was issued.
The disposable VM and its scheduled watchdog/runner tasks were removed;
preexisting VM states and the master VMGS hash were verified unchanged.

Capture/discard: source candidate under qualification. It requests a 128 MiB
preferred image, freezes, captures, reads/discards at most 512 MiB, then frees
and thaws. It never persists the image or invokes atomic restore. Acceptance
requires original=1, valid size, exact read count, successful FREE/UNFREEZE/CLOSE
and RESULT PASS. COMPLETE alone is insufficient. FNV is diagnostic only.

Neither probe establishes native resumable private snapshots. TPM page rebinding,
VMBus reconnect, fresh attestation/lease and full desktop restore remain required.

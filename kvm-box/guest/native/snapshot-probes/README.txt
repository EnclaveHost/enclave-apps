Disposable Linux guest snapshot qualification probes

These PID1 diagnostic programs are never installed in application guests.
Every VM operation requires a separately created synthetic guest and an
independent host watchdog with exact-owned-ID cleanup.

Availability passed with kernel f401ccc30759c48c29a63bdb36e9cd7b09b840e863016080716b644939870da8
and OpenHCL initrd 71a91101f8f9b870199946076db2b220c68167e029539d4619a6b7600432124e.
Capture/read/discard subsequently passed: 92,291,072 bytes, original=1,
FREE/UNFREEZE/CLOSE success. Evidence in evidence/ includes unchanged before/
after VM inventory and master/module pins, watchdog receipt and independent
VM/task absence confirmation. No snapshot was retained by that probe.

accept_capture.py requires all ordered operation successes, exact bounded read
size, independent cleanup, unchanged inventory and pins. COMPLETE alone never
passes. test_accept_capture.py covers missing/duplicate/out-of-order steps,
failed cleanup, corruption of inventory/pins and mismatched read lengths.

roundtrip/ is a new two-boot encrypted synthetic diagnostic, NOT qualified.
It needs a separately reviewed lifecycle and freshly created blank 1 GiB VHD.
It must not run against existing disks or with real credentials/user state.
Its PUBLIC test key and placeholder binding digests establish no privacy or
attestation claim. It checks continuation and TPM transport, not TPM custody,
exclusive admission, GPU recovery, full desktop or writable-filesystem restore.
The kernel explicitly forbids cancelling capture then restoring it in place.

Native application snapshot endpoints remain disabled. See ../SNAPSHOTS.md.

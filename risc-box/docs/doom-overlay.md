# Fullscreen Doom overlay

The X11 Doom frontend's opt-in `-overlay` path scales into `/dev/fb0` at the
window's actual screen coordinates. The host composites the completed image
over the virtio-GPU desktop. This removes X's copies from the emulated CPU.
The guest still renders a 960×600 frame when the display is 960×600.

The guest uses the mailbox at physical address `0x10007000` through
`/dev/mem`, alongside the existing OPL mailbox. Its byte registers are:

| Offset | Meaning |
| --- | --- |
| 0–3 | Read-only signature `RBXO` |
| 4 | Command: 0 hides, 1 publishes a completed frame, 2 renews a static frame |
| 8, 12, 16, 20 | Little-endian u32 x, y, width, height |

Geometry is staged until a command arrives. Publishing copies the pixels
before guest execution resumes, so capture cannot see a partially painted
frame. A static renewal avoids the copy unless the rectangle changed or the
host has no previous image, as after a snapshot restore. Copies and allocations
are bounded by the reserved 8 MiB framebuffer window; composition also clips
to the current display size.

The lease expires after two seconds without a presentation or renewal. Hiding
or expiry suppresses the legacy write-extent heuristic, so old framebuffer
writes cannot resurrect a departed window. Ownership and completed pixels are
per machine and transient across restore. The guest republishes geometry on
every renewal, repaints fully when switching destinations, and hides the layer
on visibility changes, movement, and orderly exit. An abrupt process exit is
covered by the lease.

Hosts without the signature retain the previous overlay behavior. Those hosts
cannot reliably composite fullscreen overlays; leave `-overlay` off there.

Keep the mailbox's frame-copy path out of line from normal emulated stores.
Inlining it into that path caused a large performance regression despite the
copy itself taking less than 0.1 ms in the measured wasm64 SET workload.

Validation includes the display unit test for fullscreen edges, clipping,
hidden layers, and preservation of completed pixels while the next frame is
painted. The mailbox test checks publication, lease expiry, and renewal.
Live performance should be judged by Doom's dynamic frame reports and distinct
stream frames, with sound/music enabled, rather than the encoder's 60 FPS rate.

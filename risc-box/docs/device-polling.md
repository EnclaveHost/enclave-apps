# Device queue polling

The emulator services devices after each short CPU burst. GPU command queues and the sound control queue now drain after QueueNotify or QueueReady, instead of re-reading empty available rings on every service. Both devices leave used-ring notification flags at zero and do not offer EVENT_IDX, so drivers notify newly posted work. Any queue notification safely rechecks the command queues.

PCM playback still accrues the same credit from mtime on every service. The front buffer remains posted until enough playback time has elapsed. Its descriptor length is cached during that wait and the head and length are rechecked before spending credit. Completion, queue reconfiguration, flush, and reset invalidate the cache.

These caches add no serialized fields. Restore invalidates the PCM price and schedules a command-queue drain, so requests posted before a snapshot are processed without another guest notification.

Validation: full emulator library suite, 42 passed and 12 previously ignored. Three integration tests use real DMA descriptors and available/used rings to check batching, spurious notifications, pending work after restore, exact PCM bytes, fine-grained playback timing, a snapshot halfway through a period, and a subsequent shorter period.

An isolated native benchmark measured median device service cost of 17.9624 ns before and 2.2055 ns after the change (five runs of two million services). The full wasm64 + share-everything threads + application SET demo1 run measured 31.46 game FPS and 31.55 stream updates/sec at 960×600 with stereo sound and music. See doom-aot.md for comparison and measurement limits. The new-viewer, overlay expiry, resume, keyboard, and palette integration checks also passed.

Protocol basis: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html, sections 2.7.10 and 2.7.13.

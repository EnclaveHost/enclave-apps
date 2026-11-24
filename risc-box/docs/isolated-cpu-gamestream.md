# GameStream in the isolated CPU app

The published 0.6.54 build declines to bind its GameStream ports without NVENC.
The protected TCP/UDP tunnel now works in production, and SSH answers, but this
app gate still prevents a direct Moonlight connection.

This change enables the existing app-side H.264 CPU encoder without NVENC,
drives capture from a running GameStream session without needing a browser
video subscriber, and forwards frames in both SET and inline execution.
Encoding runs inside the app isolation boundary, not on the physical host or
the emulated RISC-V core. An active stream selects H.264; an AV1 browser join
is refused while GameStream is running.

Before enabling that previously dormant listener, its TLS control endpoint now
requires a completed pairing certificate and cryptographic proof of the private
key. Public `/pin` and `/unpair` are refused. Submit pairing consent through the
app's existing API-key authentication: `POST /gamestream/pin` with JSON
`{"uniqueid":"<client-id>","pin":"<four-digit-code>"}`. A configured app API key
is required. TLS or control-port setup failure refuses the whole host.

Validation: `cargo test --manifest-path risc-box/Cargo.toml --lib gamestream -- --test-threads=1`.
The tests include CPU-only listener creation, a real CPU H.264 frame, stream
watcher lifecycle, required paired certificates, and rejection of public PIN
approval/revocation. This is not an end-to-end Moonlight pairing or playback test.

Remaining release work: rebuild with the published SET64/AOT toolchain so the
existing emulator optimizations are preserved, exercise the resulting artifact
in the isolated guest, publish it, and update the owner's deployment. This
source change has NOT replaced 0.6.54 in production. The existing in-app audio
implementation still emits silence; it does not encode the guest's audio.

# Private VM snapshot encryption

This crate wraps an app's existing serialized VM state. It is not a VM
snapshot implementation, Sia client or S3 extension. RISC Box owns the machine
serializer and uses this before standard S3 PUT and after GET. Native full-VM
capture/resume still requires an outer-runtime implementation.

`EVMSNAP1` consists of an 8-byte magic/version, 32 random salt bytes, then
records: big-endian u32 plaintext length, encrypted bytes, 16-byte GCM tag.
Records contain at most 1 MiB, ending with a mandatory authenticated zero-length
record. Extra bytes after the final record are rejected. Each save derives a
fresh AES-256 key with HMAC-SHA256(master, context || header); the random salt
is public. The nonce is four zero bytes followed by a big-endian u64 record
index. AAD binds context, header, index and length. Context hashes separately
length-prefixed app/owner/deployment/bucket/object identifiers. Do not change
this layout without a version bump.

Decryption authenticates every record before writing it, but callers MUST
withhold all output from consumers until the final tag and EOF succeed.
`open_bytes` stages in memory; the CLI stages in a private file. Snapshot
length/object names remain visible to storage. An authentic old object can
still be replayed: encryption does not implement a monotonic revision service.

## Offline migration

Build with `cargo build --manifest-path snapshot-crypto/Cargo.toml --release`.
Use a trusted downloaded legacy snapshot and a config containing
`snapshotEncryption.masterKey` (an environment reference), `owner` and
`deployment`. Use the exact destination S3 bucket and object key, since they
are authenticated. For example, from a shell with the deployment secret
already provisioned in its environment:

```sh
snapshot-crypto seal risc-box machines private/session.snap legacy.snap private.enc < snapshot-config.json
snapshot-crypto open risc-box machines private/session.snap private.enc verified.snap < snapshot-config.json
cmp legacy.snap verified.snap
```

Upload `private.enc` using normal S3 tooling, download it to a fresh file, open
and compare again, then select the new object in the app. Verify an actual VM
resume before retiring the legacy copy. The tool makes no network calls and
never overwrites a file. It writes 0600 staging files and publishes with an
atomic no-clobber hard link only after completion. Unix only. Keep migration
files on private storage; interruption by SIGKILL can leave a 0600 staging
file requiring cleanup. Do not place real secrets in command arguments or
public config. The tool supports opaque bytes; successful encryption alone
does not establish that those bytes are a valid VM snapshot.

## Verification

`cargo test --manifest-path snapshot-crypto/Cargo.toml` covers empty and
multi-record streams, fresh salts, wrong owner/app/deployment/key/location,
every-byte corruption and every prefix truncation of a small envelope,
record splicing/reordering, size limits, public-template pinning and offline
atomic/no-overwrite/failure cleanup. The RISC Box library adds a test that
resumes CPU execution plus saved RAM and disk delta after encryption/decryption.

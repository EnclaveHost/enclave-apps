# Native desktop automation service

This is the desktop-side implementation for migrating RISC Box's `/exec` and
`/computer` clients. The controller and both gateways are wired in source, but these
changes are **not yet in the measured/released image**. Image integration and
a real confined-VM acceptance run remain required before changing EyesOff AI
or the API MCP Adapter configuration.

The service must run as desktop UID/EUID 1000 inside the existing confined
desktop namespace. It accepts HTTP over `/run/native-automation/desktop.sock`
only from Unix peer UID 1003 (the inner gateway). The measured launcher must
create `/run/native-automation` as 1000:1000 mode 0750. The service creates its
socket mode 0660 and refuses an existing socket. No TCP listener is opened.
Connection and request concurrency, headers, bodies, time and output are bounded.

The Wasm controller must authenticate requests before forwarding them. This
service does not authenticate an owner, accept a credential, or authorize a
desktop process to act as the inner gateway. Commands run as the desktop user,
never the UI or compute-service identities. Child environments are constructed
explicitly and contain no controller credential. This is owner-authorized
arbitrary **desktop-user** command execution, not a command sandbox inside the
desktop. The existing measured namespace, device, networking and resource
boundaries must continue to apply to all commands and their descendants.

## Implemented interfaces

`POST /exec` accepts `cmd`, optional `timeout_s` (1–120, default 30), and
`max_bytes` (1024–1048576, default 65536). It returns the RISC response shape:
`ok`, `exitCode` when completed, combined `output`, `truncated`, and `ms`.
A nonzero command exit is a completed execution and retains its exit code.
Cancellation/timeouts kill the command process group. Output continues draining
after the cap; descendants holding a completed command's output pipe trigger
a bounded failure and group cleanup. A deliberately detached program is still
ordinary desktop workload subject to the VM's existing resource limits.

`POST /computer` supports screenshot/look/observe, move, clicks, mouse down/up,
drag, scroll, type, key chords, and wait, including the RISC aliases. Coordinates
can be pixels or a supplied `grid`; numeric strings and coordinate arrays are
accepted. Input is validated before any event is sent. Xdotool receives fixed
commands and separate argument strings, never shell interpolation. A failed
drag attempts to release its held button. Computer actions are serialized.

Responses include `screen`, `coordinates`, `cursor`, `settled`, `changed`,
`waited_ms`, `mime: image/jpeg` and base64 `image`. Settling means unchanged
sampled pixels for 500 ms after at least 600 ms reaction time; it does not
assert CPU idleness. Text is limited to 800 characters; waits to 20 seconds.
Screenshots are limited to 3840×2160 and encoded JPEG to 4 MiB. Actual images
come from `desktop-capture.c`, installed at the fixed path
`/usr/local/libexec/kvm-box/capture`, using the desktop's authenticated X11
connection. The helper accepts no arguments and requires UID/EUID 1000.

## Verification

From `kvm-box/guest/native/automation`:

```sh
GOWORK=off GO111MODULE=off go test -race -count=1 .
GOWORK=off GO111MODULE=off go vet .
```

The tests execute real shell commands and Unix socket connections. They cover
nonzero exits, bounded output, timeout/cancellation, inherited credential
exclusion, invalid requests without side effects, peer rejection, role refusal,
RGB framing, grid mapping, invalid input without injection, JPEG decoding,
failed-drag cleanup, pixel settling and concurrent action refusal.

From the repository root, with an available Xvfb binary, a writable scratch
directory and the desktop UID (1000):

```sh
cc -O2 -Wall -Wextra -Werror kvm-box/guest/native/desktop-capture.c -lX11 -o /path/to/scratch/capture
python3 kvm-box/tests/check_native_automation_x11.py \
  --xvfb /path/to/Xvfb --capture /path/to/scratch/capture \
  --output /path/to/scratch/new-test-directory
```

This creates an authenticated, TCP-disabled test X server on an unused display,
sets known pixels, checks exact screenshot bytes and pointer coordinates, tests
option-like typed text, and terminates only that test server. It never connects
to the user's display or a production VM. Its JSON report pins tested binaries.

## Required release work

1. Source complete: optional `KVMBOX_AUTOMATION_KEY` is loaded only by the
   Wasm controller, retained as a digest, and permits only the exact two POST
   routes. Bearer or `X-Api-Key` is accepted; both are accepted only when each
   independently matches the same key. Duplicate or conflicting credentials
   fail. Owner Basic authentication remains available. The measured release
   policy still needs to deliver the separate automation secret.
2. Source complete: both gateways allow the exact routes. The inner gateway
   verifies socket metadata and server UID1000 before writing; bounds the
   deadline, headers and body; validates strict JSON; and emits safe headers.
   It rejects redirects, chunked replies, oversized replies and malformed JSON.
   Rejected bodies are closed without draining. Source tests cover these
   boundaries, but confined-VM qualification is still required.
3. Add the two binaries and service launch/readiness to a new measured image;
   preserve the UID separation and the masked-only GPU path. Rebuild, sign and
   qualify that image before enabling its profile.
4. Verify authenticated endpoints, wrong keys/roles, timeout cleanup, real
   screenshot/input, native compute and graphics from the actual VM. Then
   publish the new version and migrate the two running tool configurations.

No existing catalog version, deployed configuration, owner credential, or
running service is modified by building or testing these sources.

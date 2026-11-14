# Fixed inner desktop gateway

Claude contributed the implementation and initial socket tests from copied
public source context. The main agent reviewed the source, added noVNC MP3/OGA
MIME support, corrected a truncated-frame test to signal EOF, and added pairing
deadline/cancellation coverage. This component is implemented and tested; it
is not deployed or admitted as a native desktop profile.

Build with `CGO_ENABLED=0 go build -trimpath -o inner inner.go`. The Linux Go
standard library is the only dependency; os.OpenRoot requires a sufficiently
recent Go toolchain. Verification used Go 1.27.2-X:nodwarf5 on linux/amd64.
Run `go test -race inner.go inner_test.go` and `go vet inner.go inner_test.go`.

## Boundary

Production uses UID/EUID 1003, no arguments or environment endpoint overrides,
and TCP 127.0.0.1:18080. Inherited owner password/hash variables are refused.
The measured nftables policy permits only UID 1002 to reach this listener and
binds upstream source ports to the prescribed roles. TCP addresses and headers
do not authenticate a process UID. The privileged launcher must install these
rules first, drop all capabilities, apply NoNewPrivs and the role seccomp
policy, close inherited descriptors and provide a clean environment.

Only root-owned, non-group/world-writable static files on a read-only filesystem
are served from /usr/share/kvm-box/native-ui. The build tool flattens noVNC
assets. Runtime confined opens, ancestor/component checks, inode verification,
path/extension allowlists and size limits prevent serving desktop-home content.
The two compute pages use the single fixed native.html. Responses have fixed
MIME, nosniff, no-store and same-origin framing headers. Unknown routes are 404.

Native compute reaches only /run/shield/compute.sock. Before sending, it checks
the parent/socket owner, group and modes and requires server SO_PEERCRED UID
1001. Requests/responses use bounded u32 big-endian JSON framing, EOF checking,
strict request parsing and absolute status/work deadlines. Model output is
JSON, never HTML. The measured service configuration must explicitly authorize
client UID 1003; filesystem group membership alone does not authorize it.

WebSocket traffic reaches only 127.0.0.1:6080. Handshakes and the upstream accept
value are checked; owner authorization, cookies and caller forwarding headers
are not propagated. Moonlight tunnels permit only the fixed Sunshine ports,
with raw TCP or bounded u16 big-endian UDP framing. The Sunshine administration
port cannot be reached by a tunnel. All streams count toward the 32-connection
limit, preserve buffered early bytes, propagate TCP half-close and close on
errors, timeouts or cancellation.

Pairing reads a private generated credential owned 1003:1003 mode 0600 from
/run/native-ui/sunshine-password; its parent must be 1003:1003 mode 0700.
This is a per-boot Sunshine credential, not the owner's password or wallet.
The fixed HTTPS endpoint uses a self-signed certificate and relies on measured
nft rules for endpoint identity (UID 1000 on port 47990). Environment proxies
and redirects are disabled. Only one pending pairing is accepted, and only a
sanitized boolean or fixed error is returned. PINs, credentials, prompts and
backend bodies are not logged. Root platform code remains trusted.

## Verification and limits

Twenty-five test groups pass with the Go race detector, covering strict HTTP,
static integrity, framed compute, peer rejection, cancellation and deadlines,
WebSocket handshakes, stream capacity/half-close, UDP boundaries and pairing.
The initial UDP test sent a valid 65500-byte length followed by only 102 bytes
without EOF; expecting immediate closure was incorrect. The corrected test
signals EOF, while a separate test verifies immediate rejection above 65507.

Five additional tests run the actual static binary as UID 1003 under the
production capability/NoNewPrivs/seccomp policy in disposable mapped Linux
namespaces with actual nft rules. They verify root-owned read-only assets
(including refusal after a test remount to writable), reciprocal Unix peer
credentials, wrong-role network rejection, WebSocket early bytes/half-close
and Moonlight TCP/UDP framing. Backend processes are synthetic fixtures. The
first fixture run passed its assertions but failed cleanup ordering; the
corrected harness stops processes before unmounting and exits successfully.

This is not full nsinit, real Sunshine pairing, Moonlight playback, GPU
acceleration, confidential-VM attestation or platform admission. The launcher
still must compose/reap services, generate the pairing credential and initialize
Sunshine, mount authenticated models, reserve the authenticated worker, enforce
cgroups, and signal readiness only after the required services are ready.
The existing control-chain test still uses a fixture inner server; this new
boundary test does not by itself replace that whole-chain acceptance.

## Pairing the same client again (source, not released image)

Sunshine 2026.914 appends a record for every completed pairing and accepts a
client certificate only while exactly one record carries it. Moonlight keeps
its certificate, so a Moonlight that paired a second time was refused on every
HTTPS request ("The client is not authorized. Certificate verification
failed. (Error 401)"), and every further try added a record. Only a desktop
restart cleared it.

After Sunshine accepts a PIN the gateway now leaves one record per
certificate (`clients.go`): it lists the records through Sunshine's admin API,
asks the desktop automation service which certificate each record carries
(`/paired`, digests only), removes every record that a later record repeats,
and lists again. The page hears `{"status":true}` only when that list holds
none of the removed records; otherwise 502. Rules:

- The newest record of a certificate always stays, so the list never becomes
  empty here (Sunshine ends the running application when it does).
- Without digests for every listed record (three tries, 200 ms apart) only the
  newest record stays: the client that just paired works, other devices pair
  again.
- The cleanup runs on the gateway's own context (10 s), not the page's
  request, once Sunshine has accepted the PIN.
- New tunnel connections to port 47984 wait from before the PIN is sent until
  the list is settled (at most the pairing time plus the cleanup time, then
  503). Sunshine stores the record before it answers the PIN, and the client's
  next request is its first with the certificate; held back, it never meets a
  list in which its certificate appears twice. Connections already open are
  not touched.
- `/paired` is not a web route: the page and the controller get 404.

Limit: when the page's request ends while the PIN is still with Sunshine (30 s
pairing time), a pairing that completes afterwards is not cleaned up. The next
pairing through the page cleans it up.

Unit tests (`clients_test.go`) use a Sunshine fixture that keeps records the
way Sunshine does. A separate run uses Sunshine itself: the image's base root
read-only in a sandbox with its own loopback, a virtual display, Sunshine
started as nsinit starts it, automation's `TestLiveServePaired` answering
`/paired` from the real state file, and `TestLiveSunshinePairAgain` pairing as
Moonlight does (one certificate kept, the PIN through this gateway's route,
the first certificate request sent at once through the tunnel). Both tests
are behind the `sunshine_live` build tag and skip without their environment:

```sh
go test -c -tags sunshine_live -o "$BIN/inner.test" .          # in inner/
go test -c -tags sunshine_live -o "$BIN/automation.test" .     # in automation/
bwrap --ro-bind "$BASE_ROOT" / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run \
  --bind "$HOME_DIR" /home/desktop --ro-bind "$BIN" /mnt --bind "$OUT" /media \
  --unshare-net --unshare-pid --unshare-ipc --die-with-parent /bin/sh /mnt/inside.sh
```

`inside.sh` is `sunshine-live-inside.sh` copied into `$BIN`. It sets a random credential with `sunshine /sunshine.conf --creds`,
starts `Xvfb :0` and `sunshine /sunshine.conf`, then runs
`automation.test -test.run 'TestLiveServePaired$'` (with
`KVMBOX_LIVE_SOCKET_DIR`, `KVMBOX_LIVE_STATE`, `KVMBOX_LIVE_STOP`) beside
`inner.test -test.run 'TestLiveSunshinePairAgain$'` (with
`KVMBOX_LIVE_CREDENTIAL`, `KVMBOX_LIVE_SOCKET_DIR`).

Result on 2026-11-14 with Sunshine 2026.914.233613: one client paired three
times and a second client beside it all stay authorized, from the first
certificate request on, with one record each. The same binary shows the
defect when the PIN is entered at Sunshine alone (two records, status 401 for
that certificate, the other client unaffected) and clears it with one more
pairing through the route. This run has no nsinit, no confined identities, no
nft rules and no video stream; it is not a test of the image.

## Desktop automation integration (source, not released image)

The exact POST routes `/exec` and `/computer` now forward the controller's
already-authenticated automation requests. The separate automation key is
owned only by the Wasm controller and is never forwarded. Both gateway startup
checks refuse an inherited `KVMBOX_AUTOMATION_KEY`.

The inner gateway requires `/run/native-automation/desktop.sock` with directory
1000:1000 mode 0750, socket 1000:1000 mode 0660 and connected peer UID 1000. It
validates a strict JSON request up to 64 KiB, uses a 125-second absolute exchange
deadline, and admits only bounded JSON replies with explicit length. Desktop
headers, cookies and redirects are not relayed. The outer response-header
limit is 130 seconds to accommodate the 120-second maximum command deadline and
bounded cleanup. No TCP desktop automation listener or direct GPU route is
introduced. Packaging and confined-VM acceptance remain required.

# KVM Box

KVM Box combines a portable Wasm controller with a native x86 desktop. This
source checkpoint contains two deployment paths:

- The original Linux KVM/SNP broker path described below, with separate
  controller and desktop guests.
- The native desktop profile in `guest/native/` and `src/bin/shield-desktop.rs`,
  paired with the Enclave platform's dedicated Windows VBS launcher. Its image,
  authenticated volumes, monitor startup and cleanup have been qualified on
  NucBox. The real manager has launched the signed v9 image with the installed
  CPU/RAM pool sizes and enforced guest CPU quota; fresh app/host attestation
  and scoped cleanup passed. Owner release and final desktop acceptance remain.
  This path retains the stated operator/hypervisor
  trust boundary; it has no verified host-OS-exclusion claim.

The complete app has not yet been published or deployed through the user's
wallet. The v9 candidate removes the Shield Compute and Native Shield Compute
menu entries. Its standard OpenGL/Vulkan interface uses guest CPU Mesa for
general rendering and supports bounded masked GPU operations; it is not a
universal GPU renderer. Selected CTS results do not establish conformance or
Steam game compatibility. The final owner's Moonlight pairing/stream must still
be verified after provisioning this image.
Build outputs and owner credentials are kept outside this source checkpoint.

## Original Linux KVM/SNP path

The following broker, build, credential and connector instructions describe the
original Linux path, not activation of the native Windows profile above.

A separate Wasm app that controls a native x86 Linux desktop through a host-side VM broker. Both the controller and desktop run in separate SEV-SNP guests. The desktop boots directly on KVM virtual CPUs; no RISC-V interpreter or nested KVM is involved.

The first implementation is a dedicated local deployment on the tested AMD EPYC host. It is not registered in the public Enclave catalog and is not a drop-in app for ordinary Wasmtime nodes. Fleet scheduling, billing, wallet authorization, remote enrollment and durable desktop storage are not implemented here.

## Components

- `src/`: portable `wasm32-wasip2` controller with start, status and stop actions. Its only management connection is to a fixed loopback bridge.
- `guest/helper.go`: controller TCP-to-vsock bridge, or desktop authentication gateway. It never executes a caller-provided command.
- `host/broker.py`: admits only the controller guest's vsock CID, launches only the two fixed digest-checked release images, limits resources, and verifies SNP attestation. It has no ordinary-VM fallback.
- `guest/`: independent Debian x86 desktop with Firefox ESR, Fluxbox, custom xdoom/FreeDoom and noVNC. The desktop and its credentials are not accessible through raw VNC on the host.
- `host/connect.mjs`: owner-side console connector. Independently verifies AMD attestation against the local release manifest, checks the supplied TCB floor, and pins the measured guest's TLS public key before transmitting the owner's credential or application data.
- `host/build.py`: builds separately measured controller and desktop initramfs images. `release.json` pins their hashes and expected launch measurements, the kernel, firmware and app identities. The desktop's identity is a native image descriptor, not a claim that Firefox is Wasm.

The host broker is not a confidentiality trust anchor. It may observe VM lifecycle and resource use, and can deny service. The owner must obtain the release manifest from a trusted source. The owner connector belongs on the owner's trusted machine; running it on an untrusted hosting provider would give that provider the credential and plaintext console.

## Build

Requires this app beside the existing `enclave` repository, Docker, Rust with `wasm32-wasip2`, Go, a static C toolchain, cpio, gzip, the existing verifying AmdSev firmware, matching guest kernel/modules and `sev-snp-measure`. The current release profile is x86_64 EPYC Turin (family 26/model 2/stepping 1). Other CPU/firmware combinations require a separately predicted and verified release profile.

The CUDA/HIP development desktop also supports `host/build.py --compression xz`
to reduce the initramfs allocation during firmware boot. This requires `xz` on
the build host and `CONFIG_RD_XZ=y` in the exact guest kernel. The builder uses
CRC32 and one compression thread, records the `.cpio.xz` image in the manifest,
and predicts its own launch measurement. Never substitute compressed bytes in
an existing measured release. The default remains gzip for existing profiles.

```sh
docker build -t enclave-kvm-box-desktop:0.3.0 guest
python3 host/build.py --enclave ../../enclave --work /absolute/private/work --output /absolute/release --compute-config /absolute/compute-policy
```

The Debian base OCI digest is pinned. Package versions are recorded in the desktop image; apt repositories are not a historical snapshot, so a future rebuild is not promised to reproduce today's desktop bytes. Distribute and verify the exact hashed release images. The builder must run with the deployment stopped; it replaces files only in its explicitly supplied output/work directories.

The private work directory contains a randomly generated owner password, mode 0600. Only its SHA-256 hash is baked into the guest images. Rebuilding retains the password. Do not publish the private work directory.

## Run

```sh
host/start.sh /absolute/release /absolute/private/work ../../enclave /path/vcek.der /path/Turin-cert_chain.pem /path/min-tcb.json --development-unbound
```

The connector writes its loopback URL to `WORK/connector.json`. Open it, select Start desktop, then Open desktop. The controller launches with 1 vCPU / 768 MiB and the desktop with 2 vCPUs / 4 GiB. The control service listens only on vsock. The local browser connector is bound to 127.0.0.1 and refuses unexpected Host and Origin headers.

Binding is now explicit at startup. For a deployment selected through Enclave's
trusted account/deployment flow, replace `--development-unbound` with
`--deployment-id 0x…` using its full nonzero 32-byte deployment ID. The broker
passes those exact bytes as SNP HOST_DATA for both guests and requires them in
its attestation check. The owner connector independently checks the ID supplied
on its own command line, never a value learned from the broker's state file.
Missing, zero or malformed IDs fail closed; an unbound run requires the explicit
development flag. The current running development release remains unbound.

This is instance selection, not wallet authorization or protection against a
host cloning a genuine guest with the same label. The existing wallet/ledger
authorization, catalog capability routing and lease lifecycle still need
integration before publication. No random local ID is a substitute for that
protocol. See [deployment binding](host/DEPLOYMENT_BINDING.md).
The standalone connector also accepts `--no-moonlight` for browser-only access.

The desktop has outbound user-mode networking. It has no mounted host directory, host display, GPU passthrough or inbound host-network port. Firefox runs as the unprivileged `desktop` user with its normal sandbox. The two TLS endpoints are encrypted all the way into their respective guests.

```sh
host/stop.sh
```

Stopping the broker stops both guests. These development services are not installed for automatic startup at login. Existing Enclave services and RISC Box are not changed.

## Storage

The base image is `desktop.cpio.gz`, independent of RISC Box. The controller is `controller.cpio.gz`, and the new app artifact is `kvm-box.wasm` plus its canonical bundle. Desktop writes live in guest RAM and disappear on stop. There is no disk-save or suspend/resume implementation in this version. Do not use it for files that need to survive a restart.

## Validation

```sh
cargo test
python3 -m unittest discover -s tests -v
```

Hardware checks must additionally cover both guest attestations, the authenticated console, actual Firefox execution, start/stop/restart, rejection of modified measurements and TLS-key substitution. Successful unit tests alone do not establish these properties.

## Deployment boundary

This is a new app and dedicated runtime profile, not a replacement for the fleet's manager. Publishing its Wasm alone would send it to nodes without the broker, which cannot run it correctly. Catalog publication needs explicit capability routing and account-bound provisioning of the child VM before this can be advertised as a normal fleet app. Those checks must not be replaced with a plain-KVM fallback.

## Shield compute (0.2)

The native desktop includes a local client at `http://127.0.0.1:8765/`. The owner console also exposes it through `/desktop/compute/`. Native programs can POST to `http://127.0.0.1:8765/services/images/v1/images/generations` using the image-generator API. Prompts and results stay inside the two confidential guests and their TLS connection; the desktop does not hand plaintext prompts to the GPU worker.

The client uses Enclave's existing SNP judge inside the desktop. Before sending a request, it checks a fresh nonce, AMD certificate chain, minimum TCB, measured image, app identity, runtime identity, deployment host-data, and the service's TLS key. Subsequent connections pin that key. A verification failure stops the request; there is no unverified or CPU service fallback. Attestation results are cached for at most one minute, with TLS key pinning on every connection.

The measured policy directory supplied to the builder contains `services.json`, `vcek.der`, `amd-chain.pem`, and `min-tcb.json`. `services.json` has a `services` mapping; each entry specifies `label`, HTTPS `endpoint`, `measurement`, `appId`, `hostData`, full `runtime` identity, and descriptive `capabilities`. The initial integration enables the existing Z-Image and EyesOff Shield deployments. Chat clients use `http://127.0.0.1:8765/services/chat/v1`, with their existing EyesOff API key in `Authorization: Bearer`. The desktop does not bypass service authentication or bundle user API keys. Through the owner console proxy, use `X-Compute-Authorization` instead; the gateway keeps it separate from the desktop owner credential. Speech is not enabled by this release. Upscaling is usable only when that service has an upscaler attached. This is service access, not a general CUDA/OpenGL device.

Endpoint changes and identity updates require a new measured desktop policy/image. Never obtain expected identities from the same untrusted endpoint being verified. The supplied VCEK is host-specific; migrating the service requires trusted policy maintenance.

## Moonlight (0.2)

Sunshine `v2026.914.233613` runs inside the desktop, captured from a software Xorg display and encoded with the CPU. PulseAudio supplies a guest-only audio sink, and guest-only virtual HID devices feed Xorg's libinput driver. The Sunshine package SHA-256 is pinned in the Dockerfile. No host display, host input device, or GPU is attached.

1. Start the desktop from the owner console.
2. On the owner's machine, add PC `127.0.0.2` in Moonlight.
3. Enter Moonlight's four-digit PIN in the KVM Box console's pairing form.
4. Select **KVM Box Desktop**. Start with 1280×800, H.264, 30 FPS, 8 Mbps.

For a local CLI workflow, use `python3 host/pair-local.py --work PRIVATE_WORK`.
After booting a new measured image, stop the existing Moonlight stream and add
`--replace-host-record` to replace only this VM's saved record. Other PCs and
the client's identity are preserved, with a private backup. Close any existing
pairing dialog first: concurrent pairing clients can create duplicate certificate
registrations that Sunshine correctly rejects. The helper verifies an
authenticated app listing, then closes its own pairing dialog; a GUI process
exiting or remaining open is not evidence of pairing success.

All Moonlight ports bind only to `127.0.0.2` on the owner's machine, separate from RISC Box's `127.0.0.1`. Each TCP stream and framed UDP flow crosses the host in an authenticated, SNP-verified TLS tunnel to fixed Sunshine ports inside the guest. Sunshine also requires stream encryption. Its administration credential is generated inside the guest and never returned to the owner connector. Only authenticated PIN submission is exposed; the administration port is not tunnelable.

This release supports Moonlight on the machine running the owner connector. It does not expose a LAN or Internet GameStream endpoint. UDP is carried through TLS/TCP and can suffer head-of-line blocking on lossy links; no WAN performance target is claimed. Video encoding is software, not Shield GPU encoding. Pairing keys and desktop data are ephemeral, so remove the old PC entry and pair again after restarting the desktop. Browser/noVNC access remains available.

Additional validation: `CGO_ENABLED=0 go test ./...` in `guest/` checks fixed tunnel destinations and explicit action requirements. Hardware validation must include real Moonlight pairing/streaming, audio/input, a real service request from inside the desktop, and rejection of invalid service identities.

## RISC Box desktop parity (0.3)

The native image carries the recovered RISC Box Fluxbox `bloe` style, purple background, terminal colors, keyboard bindings, tiny RiscBox GTK theme and animation CSS. Right-click the desktop for **Web Browser - Firefox**, **Terminal**, or **DOOM**. Firefox and Doom start on demand, leaving no browser process idle after boot.

FreeDoom Phase 1 uses the exact WAD recovered from the tuned RISC base. Its custom xdoom engine is rebuilt for x86 with uncapped interpolation, fused palette scaling, MIT-SHM double buffering, palette invalidation and the wide timer fix. The fullscreen launcher hides and restores the toolbar/cursor, retains sound and music, and caps rendering near 60 FPS. The previous RISC MMIO OPL synthesis is replaced by the same native OPL implementation inside this guest; PCM goes through PulseAudio to Sunshine. No host audio or framebuffer device is exposed.

Firefox's custom RV64 ESR 140.14 build was recovered as a binary, with its version metadata and profile settings. Its source patch set was not recovered. This image uses Debian's maintained native x86 Firefox ESR and ports the lightweight theme/profile/window helper; it does not claim a custom x86 Firefox engine build. It retains reduced-motion styling, no rich address-bar suggestions, restrained caching/history, two default content processes, no spare prelaunch process, and disabled speculative fetches/telemetry/recommendations. The native browser paint limit is 30 FPS rather than the emulated 10 FPS. Native JIT, site isolation, certificate checks, Safe Browsing and ASLR remain enabled. The native browser needs neither the emulated cold-start keeper nor the RV64 JIT/TLS workarounds.

`guest/desktop/PORTING.txt` records the adaptations, and `provenance.json` records source hashes. Only the theme, launchers, preferences and game assets are imported: browsing history, cookies, passwords and RISC guest credentials are not copied. The signed uBlock Origin 1.75.0 distribution bundle is carried over and enabled by Firefox policy. Rebuilding the image still resets ephemeral desktop and Sunshine pairing state; re-pair the KVM Box entry after a new guest is started.

Build the image with the Docker command above, then `host/build.py` as before. Run guest helper tests with `CGO_ENABLED=0 go test ./...` from `guest/`. The custom engine sources and licenses are included under `guest/desktop/xdoom/`.

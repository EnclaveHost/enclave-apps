# Shield-route desktop candidate (SVN25), built 2026-11-24

One change to the guest against SVN24 (a5a8c9fc..., the live release): the Shield compute overlay in the root image.

It is the overlay of the development image `enclave-kvm-box-desktop:0.6.12-direct-candidate-2`
(sha256:55ebfefe78ee..., enclave-apps main 0d63827) put on the native tree of SVN23 and SVN24: the OpenCL frontend,
the Q8 executor, llama's OpenCL plugin and libllama (built from the pinned source and the bound patches, with
device-side glue), the K-quant compiler and its reference kernels, the source-declines table of 122 requests,
`/shield/service.py`, the `llama-cli` wrapper, and `route: {mailbox, continueInline}` in `/shield/native-service.json`.
Two files stay the native ones: `libggml-shielded.so` and `libshield-core.so` carry the Unix-socket worker route; the
new executor needs no symbol they lack. Mesa, the graphics records and everything else are SVN24's bytes:
`root-image/root-files.json` differs from v21's in 35 entries, all under `opt/shield`, `shield/` and the wrapper
(`root-diff.json`). The platform archive differs in `plat/native-boot` and `plat/native-desktop/profile.json`
(`platform-diff.json`); it was linked in the r50 glibc namespace as v21 was (a first run outside it is kept in
`failed-first-package/`).

What the overlay does differently in a guest: device-side glue and lazy kernel arguments are on for `llama-cli`; the
service leaves the 8 MiB file bound as the soft limit and the executor raises its own, so session regions exist; the
mailbox, arenas and three-product exchanges are used only when the guest has four or more processors.

What was run on these bytes: sd-cli through this SDK tree gives the fixture PNG (048b2445...) with 392 exchanges; llama
through the new wrapper and tree runs the OpenCL route at 98.8 tokens a second against the native route's 94.6 on two
processors and 109.6 against 109.8 on six (SVN23's tree: 94 against 104); the root image boots in the isolated KVM
probe (`probe/valid.result.json`: dm-verity over the whole image and the model volume, the 21 control-graph checks,
the service reaching its worker). The same overlay ran in measured SNP guests on metal0 as candidate 2 (masked llama
and SD through the service, llama-cli and sd-cli through OpenCL with shared regions and arenas mapped). Not run before
activation: the two corruption probes, anything with a GPU on these exact bytes, Moonlight, and nothing on the NucBox.

## Release (2026-11-24)

Signed as SVN25 with the native desktop release key: signed image 8695919cb134018d3ce58e771ad9053a0586f543ac8ccea3bc2bbfbe41f0840e,
launch measurement 6886b5428333a283edfce9b8c55bb8756782ed7aa38615f934df245fa41841d9, verified by the signer and by the
independent check (`independent-signature-verification.json`). `candidate.json` is the exported record.

Activated on nucbox-k11-4 the way SVN24 was: staged 08:20Z, the relay's Shield policy moved (ae6326749c2b..., desktop
image floor 25) with one relay restart at 08:20Z, then the hooks and the box's policy and host bundle 2026.11.20
installed again, finished 08:25Z. The node adopted deployment 0xf9bb979a on the new image at 08:39Z
(`status=running image=8695919c...`), the lease was kept and no claim was sent; the eight other apps on the box answered
200 again by 08:39Z and the desktop's origin answers 401, which is the page waiting for its owner. The controller, its
CID and catalog version 0.9.2 did not change, so no owner signature was needed. The previous policy and hooks are
kept beside the live ones as `*.before-native-svn25-20261124`, and SVN24's release folder is still on the box:
going back is the box step and the relay step in reverse.

Not verified on the production path: nothing ran inside the production desktop. It starts stopped and only the owner
can start it and open its terminal, so masked llama and SD on the NucBox's card, the route's speed there, the number
of processors the guest gets (the mailbox and arenas need four), and the xdoom sound fix carried from SVN24 are all
still to be seen live.

This folder holds the recipe and records. The switch scripts, the policy file and the box's configuration name
internal hosts and stay in the work folder (`work/native-shield-image-v22/activation` of the 2026-10-23 directory),
with the SDK tree, the base tree and the images.

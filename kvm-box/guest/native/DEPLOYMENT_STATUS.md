# Native deployment and candidate status — 2026-10-27 UTC

The catalog app is `kvm-box-native:0.9.0`. The signed v13 release has been
published and deployed on NucBox, and the owner has reported usable Moonlight
streaming. This is a dated checkpoint, not a promise that an instance remains
healthy: check its authenticated status before starting work.

The deployed release record pins:

- Controller SHA-256: `104468ceffadc0792d09674ef97bcd30aaa212ee1137cf1f8c0560c99cf82786`.
- IGVM SHA-256: `a530885827ea28c43ec1621475887d9c8968569b2d204608522c8cf4fc8508a3`.
- Runtime: `f548e338e4e41bb1326693688c7b0067c9f330a24f4a21a61037bd334449c74f`.
- Profile SHA-256: `3123d061ce7bdca4847c24dccd4845e7eac967de7fe711b23ed833982ef11d01`.

A fresh authenticated guest diagnostic found the deployed Vulkan library
`aa600783f4e5d7e8763a5b0159fb90309d8deb19d9ece27ed9d5d6f266fc95a8`
and no physical GPU nodes. The desktop was responsive and the owner's local
Moonlight connector active. Its Shield service remained retired after the
previous transport failure, with 590 completed verified offloads recorded.
Do not clear retirement or substitute unverified GPU execution to restore it.

## Qualified candidate, not deployed

The [multi-workgroup checkpoint](../../shield-graphics/qualification/grid-checkpoint.json)
extends the integer affine path to 4,096 rows through standard OpenGL/Vulkan.
Its separate root image is
`4a5dc041155e33c923d5224f3be3103291405a62f5426fb97b604f62cad98db5`,
with verity root `27bafabba66ad4d4ed102cb2e6cfae53c7ab54443c0b6de92df725d24c01d383`.
Native KVM qualification checked 524,288 protected output values across both
APIs, 2,048 verified GPU exchanges, CPU-only negative controls, final-tile
corruption, retired retries and desktop survival. These tests used a dedicated
worker, fixed vsock relay and synthetic owner admission. They do not establish
production VBS attestation or Moonlight acceptance for the new image.

The separate signed SVN18 package is built and independently verified:

- IGVM SHA-256: `a2272bcd8f7d5db883ae212ee1cb499a8bf100f9cc6f23073e61b68343f185f9`.
- Profile SHA-256: `dffd7d80cd0dd1ff929dd3b4f79d4dc58ff81570b52eec2aab740d04e8ee3dd6`.
- Launch measurement: `99291ba3048a00505505b418220e9006f389ac228ca7ffc623058a0bb4d894d0`.

All 233 platform source inventory entries match v13. The platform archive differs
only in the authenticated desktop profile and its matching boot parameters.
The exported bundle passed file-hash and three ISO payload binding checks;
nine metadata mutation controls rejected mismatched pins and stale approval flags.
Local signing and these integrity checks do not establish hardware admission.

The host bridge's completed-stream cleanup fix is separately staged and tested
on Windows; it has not been activated. Deploying the candidate still requires
hardware admission and coordinated release pins, followed by a fresh app process
and positive owner acceptance. Preserve the existing retirement until that fresh process.

The SVN18 image, root/hash media, profile, bridge fix and prospective desktop
configuration are now staged together on NucBox under the new image hash's
release directory. The unchanged model media was hash-checked and reused.
Readback verified all six staged payload/config files. The production config
parser accepted the new configuration and rejected four pin/identity mutations;
ACL checks found only SYSTEM/Administrators access on the nine staged paths.
Manager epochs, all app states, VM uptimes and the live runner/config/bridge
hashes remained unchanged. Staging does not enable hardware admission or select
the image for the running app.

The current desktop home is ephemeral: restarting loses its Firefox/session
state. Restart timing is pending the owner's preference.

General rasterization, texture handling, visibility and encoding remain local.
Physical GPU access is restricted to the masked service channel; public-matrix
approval is independent of compiler eligibility. No general game acceleration,
full CUDA compatibility or graphics conformance certification is claimed.

# Desktop candidate with the sixteen-lane SwiGLU frontend (SVN27), built 2026-11-24. Signed, not staged.

One change to the guest against SVN26 (07c4efd2..., the live release): the OpenCL frontend in the root image,
`/opt/shield/opencl/lib/libOpenCL.so.1`, rebuilt from enclave-apps main bb54abc. Its SwiGLU glue kernel has a
sixteen-lane form, used where the processor has AVX-512 (`SHIELD_OPENCL_GLUE_LANES=8` keeps eight). It is the same
256-bit exponential on each lane, so the bits are the same: 7.9 million values compared, none different. On EyesOff's
27B it takes the glue kernels of a drafting run from 951 ms to 785 ms, 0.35% of the run.

The frontend was built the way candidate 2's was (`build-declines-frontend.py`, the same builder image and the same
table of 122 requests; the receipts differ in the frontend, `q8_glue.inc` and the build command), packaged by
`package-standard.py` and built into `enclave-kvm-box-desktop:0.6.13-direct-candidate-1` (sha256:5b2f1a7bf28b...).
From that image the native tree takes the frontend and the four records that name it; `standard/inputs.json` and
`files.sha256.json` are the native tree's with those values moved. The root image differs from SVN26's in six files,
all under `opt/shield` (`root-diff.json`): `files.sha256.json`, `opencl/lib/libOpenCL.so.1`, `standard/inputs.json`,
`standard/overlay-files.json`, `standard/source-declines/build-receipt.json`, `standard/source-declines/command.json`.
The model volume is SVN26's (`kvm-box-public-models-v5`), the same file. The platform archive differs in
`plat/native-boot` and `plat/native-desktop/profile.json` (`platform-diff.json`), linked in the r50 glibc namespace.

What was run on these bytes: the root image and the whole model volume in the isolated KVM probe
(`probe/valid.result.json`). What was run on this source and not on these bytes: every 27B round of 2026-11-24 after
06:44 (the route's text identical to the native engine's in each). Not run: the two corruption probes, llama or SD
through this exact library, anything with a GPU, Moonlight, and nothing on the NucBox.

Signed as SVN27 with the native desktop release key: signed image
45d7352a4e4dcc13b51adf60dc278526126f0f16c9c7b252d4c7d6016209be57, launch measurement
3ec3a6036c7c0a52bf727051180510d858b5035d160aa6aed930d9395ef51164, verified by the signer and by the independent check.

Not activated, by the owner's choice on 2026-11-24: the change is too small to restart the box for. Nothing was
staged on the NucBox or the relay; SVN26 stays live. To activate later, copy `activation/` from
`native-models-image-v23`, rename `svn26` to `svn27` in it with this folder's hashes, and follow SVN26's order
(`prepare.mjs <controller cid>`, then `stage`, `relay`, `box`, `retry`). If another release is cut first, rebuild on
top of it instead: this image carries SVN26's model volume and nothing newer.

Order of work, each step a script here: `inputs/build_root_image.py root-spec.json`, `probe/build.py` and
`probe/run.py valid`, `package.py` (under `inputs/with-glibc-r50.sh`), `sign-candidate.py`, `verify-independent.py`,
`export.py`.

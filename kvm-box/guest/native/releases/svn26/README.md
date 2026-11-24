# Desktop candidate with the 27B model on the model volume (SVN26), built 2026-11-24

Two changes to the guest against SVN25 (8695919c..., the live release), both about the model volume:

- The model volume is `kvm-box-public-models-v5`: the two files of v4 (`qwen2.5-0.5b-instruct-q8_0.gguf`,
  `sd15-q8.gguf`, same bytes) and `Qwen3.8-27B-UD-Q4_K_XL.gguf` (17,559,178,144 bytes, sha256 3f227079...b01e, the
  file EyesOff runs and the only 27B with a Shield calibration and policy). Built by `metal/volumes.mjs` from
  `models-src/` (hard links): ext4 with its dm-verity tree appended, 20,794,449,920 bytes, root hash 52703ab7...,
  salt c238d2ec.... The profile's `models` block names it; the desktop mounts it read-only at `/models` as before.
- The root image differs from SVN25's in two files, `shield/model-volume.json` and `shield/mount-models.sh`, which
  carry the new volume's numbers (`root-image/root-files.json` against v22's; `root-diff.json`).

Nothing else moves: `/shield/native-service.json` still lists only the 0.5B Qwen and SD 1.5, so the Shield service
does not admit the 27B. `llama-cli -m /models/Qwen3.8-27B-UD-Q4_K_XL.gguf` runs it on the processor inside the guest.
Adding the 27B to the service (policy `~/shield-perf/models27/q27.json`, calibration `q27.calib`, 24 GB reserve) is a
separate change; it has only run against two V100 workers on metal0, never against the NucBox's worker.

The platform archive differs from SVN25's in `plat/native-boot` and `plat/native-desktop/profile.json`
(`platform-diff.json`), linked in the r50 glibc namespace (`inputs/with-glibc-r50.sh`).

What was run on these bytes: the root image and the whole model volume in the isolated KVM probe
(`probe/valid.result.json`: dm-verity over the root image, all 20,631,977,984 data bytes of the model volume read
through dm-verity and mounted read-only, the control-graph checks). Not run before activation: the two corruption
probes, llama on the 27B anywhere on these bytes, anything with a GPU, Moonlight, and nothing on the NucBox.

What is different on the host this time: the model disk is new, so `models.iso` (20.8 GB) is staged with the other
media instead of being reused, and the manager hashes it at every desktop launch (about 1 GB/s on the NucBox, measured
on the old 2.5 GB disk).

Order of work, each step a script here: `metal/volumes.mjs build` (`volume-build.log`), `inputs/build_root_image.py
root-spec.json`, `probe/build.py` and `probe/run.py valid`, `package.py` (under `inputs/with-glibc-r50.sh`),
`sign-candidate.py`, `verify-independent.py`, `export.py`, `activation/prepare.mjs <controller cid>`,
`activation/svn26.sh stage | relay | box | retry`.

## Activation (2026-11-24)

Live on nucbox-k11-4 since 23:05 UTC: signed image 07c4efd2fe3db9bc906e0d89d4db62284ed5747d534d204efbd4e758cbbc24a1,
launch measurement c4664608d87ab50ffcf5c2574079081d7203b6e58e129302a709fe2697ae8a09, policy 866bc49e... on the relay
and the box, app identity 7d4c0f91 (was 97044489). `activation/svn26.sh stage`, `relay` (22:48), `box` (22:49 to
22:55), `retry`. The first desktop launch after the bundle install ended "not ready within 120000 ms" while the box
was still restarting its other apps; the node's second launch (23:04) was adopted at 23:05 and the page answered at
23:09 once its certificate was issued. All nine apps answered again at 23:15.

Seen in the running desktop, from the owner page's terminal: `/models` lists the three files with the 27B at
17,559,178,144 bytes; `llama-cli -m /models/Qwen3.8-27B-UD-Q4_K_XL.gguf -ngl 0 -t 4 -c 4096` loads it and answers
(prompt 4.6 tokens a second, generation 2.7) on the desktop's four processors and 24 GiB. Not measured: the load time,
a longer context, the Shield route (the service does not list this model), Moonlight on this image.

Rollback: `activation/svn26.sh box-revert`, then `relay-revert`; the previous hooks and policies are kept on the box
and the relay as `*.before-native-svn26-20261124`. The SVN25 release folder is still on the box.

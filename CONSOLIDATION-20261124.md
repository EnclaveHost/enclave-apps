# Branch consolidation, 2026-11-24

Every branch tip that was not merged into `origin/main` at 0d63827, what was decided for it, and the commit on main that
carries it. Nothing here was deployed, published or signed. Work was done in a separate worktree
(`consolidation/20261124`); no existing checkout or worktree was changed, no branch was deleted, nothing was force-pushed.

Main went from 0d63827 to d83f4ff (plus this file).

## Landed

| Branch tip | Commit on main | What it is | Tests run on the merged tree |
|---|---|---|---|
| `sia-s3/fix-shard-topology`, `origin/sia-s3/spike` (a1830ce) | 6afaed2 (merged by another session a few minutes before this pass reached it) | sia-s3 0.1.10, the version in production | sia-s3 host tests 2/2; wasm32-wasip2 release build; vendored `sia_storage` lib tests in debug on a loaded machine: 170 pass, 3 timing tests fail (`upload::tests::test_upload_race_*`); the same tests pass in the release run on the 0.2.0 merge below, which contains this code |
| `sia-s3/spike` local (0863354) | 6afaed2 | ancestor of a1830ce | same |
| `fix/cron-cas-retry-20261014` (d4308cc) | 4dd15ce | cron 0.1.3, and the nine Eyesoff search/warm-up commits it was cut from (aaa93cd..6e78198) | cron `cargo test` 28/28; wasm build; `cron/tests/e2e.py` 18 checks pass on the wasm component; eyesoff-ai `cargo test` 226/226 and node tests at this point |
| `fix/cron-favicon-20261013` (6d7ad4f), `fix/cron-storage-recovery-20261003` (115733b) | 4dd15ce | ancestors of d4308cc | same |
| `origin/codex/eyesoff-search-egress-20260928` (aaa93cd) | 4dd15ce | ancestor of d4308cc | same |
| `eyesoff/mcp-popup` (f6826c7) | 2653bde | the eyesoff-ai chain, 24 commits past the shared base | eyesoff-ai `cargo test --locked` 275/275 (pinned nightly-2026-07-25); node tests compact 4, padbank 2, queue 3, session 13, stop 7, sw 10, toolcalls 5, warmup 9, all pass; wasm32-wasip2 release build |
| `eyesoff/desktop-vision`, `deferred-schemas`, `pad-gauge`, `end-parks`, `multi-chat`, `shell-signin`, `warm-signed-in`, `chat-sessions`, `tool-results`, `test-ledger`, `waste-limits`, `stop-keeps-work`, `queue-messages` (local 645841a, origin 1347ca2) | 2653bde | ancestors of f6826c7 (1347ca2 was already on main as deec13d) | same |
| `codex-it/fix-eyesoff-upload-vision` (22807f8, local only) | 2653bde | ancestor of f6826c7 | same |
| `codex/capacity-work-20260930` (1cf79b1) | 51b462b | capacity-work, a new app | `cargo test --release` 3/3; wasm lib build; `reference` binary build |
| `codex/tuna-egress-diagnostics` (b108013) | 21b9dbe | egress error text in eyesoff-ai and api-mcp-adapter no longer claims IPv6-only routing | eyesoff-ai 274/274 (two tests replaced by one); api-mcp-adapter 22/22; both wasm builds |
| `risc-box/resume-hold` (f1d9e8d) | c784d00 | risc-box: pinned base images from the host blob cache, opt-in by config (`blobs`) | risc-box `cargo test --lib` 82/82 (78 before); wasm build |
| `origin/decentralize/sia-recovery` (54818ba) | 20fba2d | sia-s3 0.2.0: replication across indexers, recovery from pinned metadata snapshots, merged with the 0.1.10 fix. Conflict: version lines only (kept 0.2.0) | sia-s3 host tests 36/36; wasm build; vendored `sia_storage` 173/173 in release |
| `origin/decentralize/wallet-uploads` (50f93d7) | f94d983 | s3-ipfs-adapter 0.5.6: wallet-signed uploads. The branch already carried main's 0.5.1 to 0.5.4 fixes, so the four conflicted files take its side and the adapter tree equals the branch tip | `cargo test` 75/75; wasm build; `scripts/e2e.sh` (minio + kubo + wasmtime, loopback) ALL PASS |
| `codex/risc-cpu-gamestream-20260927` (17a3bb6) | 7481512 | risc-box: GameStream host starts on the CPU encoder; the control endpoint requires a paired client certificate | risc-box lib 84/84; wasm build |
| `codex/masked-video-20260927` (eefbe9d) | f2ecbb3 | risc-box: experimental masked H.264 transform offload, cargo feature `masked-video` (off by default), crate `risc-box/shield-video`, workflow `shield-video-test.yml`. Conflict in `gamestream/host.rs`: kept main's side | risc-box lib 84/84 with and without the feature; shield-video 6/6; wasm builds with and without the feature |
| `origin/feat/private-snapshots-review` (755e4e2), everything outside `kvm-box/` | d443a75, 0ff14f3, 8375c8c, 077f65f, c6b808d (cherry-picks of 815a51f, 6bdd93d, 6a14ff0, da6b5dd, 4ebd467 with their `kvm-box/` paths removed, original author and date kept), then d83f4ff | `snapshot-crypto` crate, encrypted private snapshots in risc-box, `risc-box/rollout` signing page, `risc-box/wasm64/private-snapshot-build` | snapshot-crypto 15 + 2; risc-box lib 85/85; wasm build; rollout node tests 16, 5, 4, 2 pass (the migrate tests need the pinned tool build, `SNAPSHOT_CRYPTO_TOOL`, sha256 cb1d5dee…); `scripts/private-snapshot-http-test.py` 7/7 after d83f4ff fixed a name it shadowed. `rollout/verify.sh` itself was not run: it calls helper scripts by absolute path outside this repository |

## Already on main by content

| Branch tip | Evidence |
|---|---|
| `fix/app-favicons-20261013` (47399a2), `fix/adapter-leaves-cache` (38e2b85), `fix/adapter-sia-timeout` (effa13c) | Every s3-ipfs-adapter commit is patch-identical to one on main (5012371, 6545fe6, 70cae71, dedd3ce, b6695c9, 7d941e9, 83e9877). The favicon commit 4514d25 is not patch-identical but all four of its lines are on main (439acfc, 95e38a1). |
| `codex/eyesoff-startup-prefixes-20260928` (da4beff) | Its one commit past the shared base, "Add depot", is patch-identical to e15c87d on main; depot has seven later commits there. The shared base landed with 4dd15ce. |

## Not landed

| Branch tip | Why |
|---|---|
| `risc-box/jit-pack` (caa8e45), `risc-box/jit-speed` (local 4b5d819, origin 812d2c8), `risc-box/jit-budget` (d64b2f9): packed JIT modules, the pack queue, JIT profiling and budget knobs | The branch predates SMP steps 1 to 5 on main (4cc7558..62699f7), which made the JIT state and the context block per hart. A hand port compiles and passes 95 of 96 emulator tests with `--features codegen` (main alone: 83 pass); the one failure, `formation_packs_hot_regions_into_one_module`, expects a second machine to reuse the first one's compiled placement, which the per-hart design no longer gives. That is a design decision for the JIT's author, and the result cannot be checked here without the SET wasm64 runtime and its codegen verb. The port is kept on `origin/consolidation/20261124-jit-pack-port` (5f481df). 3e084fc from this stack is already on main as 791d92c. |
| `codex/risc-chat-state-20261002` (5bd3f56, local only): RISC Box computers per Eyesoff chat | Conflicts in `eyesoff-ai/src/chat.html` (8), `lib.rs` (2), `tools.rs` (3), `risc-box/src/s3.rs` (3), `risc-box/Cargo.lock`, `.gitignore`. The S3 client it extends was rewritten on main (resumable downloads) and Eyesoff's tool plumbing gained chat sessions since; it needs a port by someone who knows both, not a merge. Branch left as it is. |
| `origin/feat/private-snapshots-review`, the `kvm-box/guest/native/` part (SNAPSHOTS.md, `snapshot-probes/`, and all of 50029ab and 755e4e2) | `kvm-box/` belongs to another session that is mid-release. |
| `kvm-box/shield-residual-chain` (4905c9d), `kvm-box/shield-residual-inline` (9da03be) | Not examined: same reason. |

## Things a deploy needs to know

- sia-s3 on main is 0.2.0. Production runs 0.1.10 (a1830ce). 0.2.0 has never been deployed; a config without `replicas`
  runs as one replica, but the store layer is new. Build 0.1.10 from a1830ce if a plain rebuild is wanted.
- risc-box on main now starts its GameStream host without NVENC and encodes on the CPU. The branch's own note says this
  was never tried end to end with Moonlight.
- The eyesoff-ai chain, cron 0.1.3, s3-ipfs-adapter 0.5.6 and the private-snapshot work were tested here with their
  unit and loopback harnesses only. Nothing was run on a production host or against a live model, Sia network or wallet.
- Comments in `eyesoff-ai/src/vision.rs`, `image.rs`, `lib.rs`, `docs/openai-api.md` and `api-mcp-adapter/README.md`
  still say egress is IPv6-only; b108013 changed the error text and the search module only.

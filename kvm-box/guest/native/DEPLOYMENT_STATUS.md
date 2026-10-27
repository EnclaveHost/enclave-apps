# Native deployment status — 2026-10-27 08:04 UTC

The signed SVN18 release and tested bridge are now selected by the production
manager, router, node and admission policies on NucBox. The catalog name remains
`kvm-box-native:0.9.0`. Fresh TPM verification and two independent app challenges
passed on the new image, including thirteen negative controls per challenge.
The disposable 12-GiB hardware VM was removed and its run state archived.

The owner's restart authorization was received. Before any production stop,
fresh inspection found the old native VM already absent. Its host log records
lease expiry, and a two-RPC chain read at block 53149425 confirms only 14
micro-USDC ($0.000014) of hosting credit remains. All browser spending sessions
are ended or expired. A fresh funded session has been requested, as the user
requires session funding. No funding transaction was submitted in this cutover.

Configuration activation finished at 2026-10-27 08:02:34 UTC. All nine legacy
VMs and their manager epoch were preserved. **A fresh production native app
has not started; owner desktop, Moonlight and live protected-compute acceptance
are still pending funding.** Selecting a verified image is not proof of a
serving app.

The selected release pins:

- Controller: `104468ceffadc0792d09674ef97bcd30aaa212ee1137cf1f8c0560c99cf82786`.
- Image: `a2272bcd8f7d5db883ae212ee1cb499a8bf100f9cc6f23073e61b68343f185f9`.
- Runtime: `f548e338e4e41bb1326693688c7b0067c9f330a24f4a21a61037bd334449c74f`.
- Profile: `dffd7d80cd0dd1ff929dd3b4f79d4dc58ff81570b52eec2aab740d04e8ee3dd6`.
- Bridge: `cc751de67c7d303f989367e1ac5ed5eb316f2b13cd10be74ff44256551d18d73`.
- Vulkan library: `038d5c4bf64fafdc8eb45b5c4634262c450f49dc0e4a48995f8486c714e70020`.

The [qualification checkpoint](../../shield-graphics/qualification/grid-checkpoint.json)
records host, native KVM and hardware verification separately. Isolated native
testing checked 524,288 outputs, 2,048 verified GPU exchanges, CPU-only negative
controls, late-tile corruption, retired retries and desktop survival. General
rasterization, textures, floating-point shaders and encoding stay local.

The staging file ACL defect was corrected before hardware boot. Its original
allow-list-only ACL validation missed empty rule sets; file validation now requires
both SYSTEM and Administrators grants. The first API restart's short health wait
caused a rollback; a longer bounded readiness check succeeded. Neither event
restarted legacy VMs. The original signed export remains immutable packaging
evidence; deployment results are recorded separately.

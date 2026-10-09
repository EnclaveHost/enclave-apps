# Moving enclave.host's source off GitHub

The goal: depot becomes the home of `EnclaveHost/enclave` and
`EnclaveHost/enclave-apps`, and the public GitHub repositories go away.

Moving the **git history** is the easy part, and depot does it today: the full
enclave repository (2,033 refs, 420 MiB) mirrors in one run of
`scripts/mirror.sh` and clones back byte-identical.

Moving **everything that runs on or verifies against GitHub** is the real
project. Some of it is not ours to change. This document lists what depends
on GitHub, in the order it would break, and a sequence that never leaves the
platform unverifiable.

The inventory was taken from `origin/main` at 7c91b334f on 2026-10-09. File
references are into that tree.

## What depends on GitHub

### 1. The attestation chain (hardest: do not delete GitHub before this moves)

- **Sigstore identity = GitHub Actions OIDC.** Release certificates are
  issued for `token.actions.githubusercontent.com`, repo `EnclaveHost/enclave`,
  workflow `tinfoil-release-publish.yml`. Our verifier enforces exactly that
  identity in `verifier/provenance.mjs`, `verifier/web/provenance.mjs`,
  `verifier/consumer.mjs`, and in the copy bundled into the relay at
  `relay/vendor/enclave-verifier-node.mjs`. A self-hosted git server cannot
  mint these certificates. Replacing them means our own OIDC issuer plus
  Fulcio, or a release-signing-key scheme, then rewriting the policy in every
  verifier.
- **Tinfoil's verifier only knows GitHub.** `@tinfoilsh/verifier`
  (`site/vendor/verifier.js`) and `tinfoil-cli` resolve `releases/latest`,
  `tinfoil.hash` and `/attestations/` through `github-proxy.tinfoil.sh`. The
  supervisor's self-check (`supervisor.js` ~3636–3690) and the site's
  `site/js/core/verify.js` do the same. **This is Tinfoil's code.** It needs
  Tinfoil's support, or a move to metal-only attestation for the fleet.
- **Tinfoil Containers deploy from a GitHub repo and tag**
  (`scripts/tinfoil-update-fleet.sh`, `scripts/autoscale.mjs`: `container
  create --repo EnclaveHost/enclave --tag …`). Also a platform dependency.
- **The repository string `EnclaveHost/enclave` is pinned** in the CLI
  (`cli/enclave.mjs`), the relay (`relay/mcp.js`, `relay/reverify.mjs`,
  `relay/tunnel.js`) and the enclave configs (`ENCLAVE_REPO` in
  `enclaves/*/tinfoil-config.yml`). It is also **stored on-chain** in every
  `EnclaveRegistry` row (`register(endpoint, repo, …)`). A new identity means
  re-registering every host.

### 2. CI/CD (GitHub Actions)

`deploy.yml` (push to main → site, relay, contracts, sidecar images, release
dispatch, bot commits), `tinfoil-release*.yml` (images, tags, measured
releases), `autoscale.yml` (cron), `fleet-op.yml`, the toolchain workflows
(which publish build products as GitHub release assets), the verifier crons,
and the test and secret-scan checks.

Secrets and variables live in GitHub settings (`DEPLOY_SSH_KEY`,
`DEPLOYER_PRIVATE_KEY`, `TINFOIL_API_KEY`, `ADMIN_TOKEN`, `AUTOSCALE_*`,
`DEPLOY_BASE_OVERRIDE`, …). Autoscale even keeps state in repo variables.

depot provides the trigger a replacement CI needs: signed push webhooks
compatible with GitHub's `X-Hub-Signature-256`. The runner and the workflow
logic still have to be built (a self-hosted runner service, or the same
scripts driven by a webhook receiver on a build box).

### 3. Container registry (ghcr.io)

`enclave-supervisor`, `enclave-wasm-manager`, `enclave-worker`, `enclave-mps`
and `enclave-wasmtime` are pulled by digest:

- by Tinfoil at boot (`enclaves/*/tinfoil-config.yml`);
- by metal boxes (`metal/build-image.mjs`, `metal/oci-pull.mjs`, which uses
  GHCR's anonymous token endpoint);
- by the wasm build (`wasm/Dockerfile.wasm`).

The packages belong to the org and probably survive deleting the repository
(unconfirmed). Moving registries changes every image reference, which means
a new measured release.

### 4. Installers and release assets

- `cli/install.sh` and `cli/install.ps1` resolve `cli-v*` tags through
  `api.github.com` and download from `github.com/…/releases/download/`, so
  `curl https://get.enclave.host | sh` breaks.
- `scripts/release-cli.sh` publishes with `gh release`.
- `wasm/Dockerfile.wasmtime` downloads ORT, the llama.cpp stack and ffmpeg
  from our own GitHub release assets.
- `metal/update.mjs` lists GitHub releases and fetches tags from a GitHub
  clone.
- The enclave site's "Get the app" (APKs and `mobile-index.json`) comes from
  `enclave-apps` releases. Its `mobile-shell.yml` builds the signed APKs.

### 5. Links

All cheap to change:

- `site/index.html` ("Source on GitHub", `sameAs`), the footer,
  `site/host.html`, `site/develop.html` (clone instructions), `README.md`;
- `cli/enclave.mjs` error hints;
- `eyesoff-ai/src/legal.html`, which names **GitHub Issues as the legal
  contact channel**. That needs a real replacement, not just a new URL.

## A sequence that never breaks verification

1. **Deploy depot** (see the README) at a stable name, for example
   `git.enclave.host` as a custom domain, with a rollback witness at a
   second provider (README, *Rollback*): as the source of truth it must not
   accept R2 quietly serving an old manifest. Mint an admin token, a
   personal token, and a read-only token for any mirroring job.
2. **Mirror** both repositories into depot (`scripts/mirror.sh`), and keep
   them in sync. Make every developer push update both remotes:
   `git remote set-url --add --push origin https://git.enclave.host/enclave.git`.
   Protect `refs/heads/main` and `refs/tags/v*` in the depot config, as on
   GitHub.
3. **Make depot the source of truth for development.** Clones, reviews and
   history come from depot. GitHub becomes a one-way mirror that CI still
   runs from: a webhook-driven job pushes depot → GitHub.
4. **Replace CI** with a runner triggered by depot webhooks, using the same
   deploy scripts, with secrets moved off GitHub. Keep the release workflows
   on GitHub until step 5, because they *are* the attestation identity.
5. **Replace the attestation chain.** This is the decision point:
   - Either keep releasing through a public GitHub repository (it can be a
     release-only repository, with source elsewhere).
   - Or move to our own signing identity: an OIDC issuer plus Sigstore, or a
     pinned release key, verified by our verifier. Tinfoil hosts would then
     need Tinfoil's cooperation, or would be retired in favour of
     metal/pVM-attested hosts.

   Then move images off GHCR, installer and toolchain assets off GitHub
   releases, the on-chain `repo` strings, and the pinned
   `EnclaveHost/enclave` constants, in one measured release.
6. **Archive, don't delete.** An archived repository keeps its releases,
   attestations and packages. Clients already in the field (installed CLIs,
   pinned verifiers, APK links) keep working while they age out. Delete only
   once nothing resolves against it.

## What depot offers for each step

| Need | depot |
| --- | --- |
| Host the history, private by default | yes: encrypted at rest, per-repository read/write tokens |
| Public source for transparency and verification | yes: a repository or pattern can be public (anonymous clone and browse) |
| Developer pushes, protected `main` and release tags | yes: `protected` patterns, atomic pushes |
| Trigger CI | push webhooks, GitHub-compatible signature |
| Releases, release assets, OIDC identity | **no**: belongs to the new release/attestation design (step 5) |
| Container registry | **no**: R2 plus IPFS, or a registry, is a separate choice |
| Issues and pull requests | **no**: review happens over branches; a forge UI is a separate project |

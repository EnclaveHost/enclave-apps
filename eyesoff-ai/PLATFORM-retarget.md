# Platform ask: retarget a deployment to another catalog app

## Problem

A deployment's identity (its bytes32 id, and everything derived from or
attached to it: the `<id8>.app.enclave.host` hostname, escrow balance,
config override, relay-staged secrets, custom domains) is welded to one
appId forever. A rebrand that creates a new slug (llm-chat became
eyesoff-ai, appId is keccak(publisher, slug) so the slug cannot follow)
strands every live deployment: the only way onto the new app is a new
deployment, which mints a new id and therefore a new hostname. Concretely:
0xcc1f4f3f... serves https://cc1f4f3f.app.enclave.host, that URL is pinned
in the shipped APK and user bookmarks, and it cannot move to the eyesoff-ai
app today.

What exists now: `upgradeDeployment(id, version)` keeps appId fixed;
`transferDeployment` moves the owner wallet, not the app; `create` mints a
new id. The gap is one relaxed constraint, not new machinery: on version
change the runner already restarts the deployment in place with a new
image reference, and a retarget is byte-for-byte that same restart with a
different appId in the reference.

## Proposal

Ledger (rev bump): `retargetDeployment(id, newAppId, version)`, or extend
`upgradeDeployment` with an optional appId argument.

- Auth: msg.sender == deployment owner.
- Gates, same set the upgrade path runs today, evaluated against the
  TARGET app: version approved and not yanked, app active, share fit vs
  the version's specs, platform GPU cap, publisher-fee snapshot re-taken
  from the target app (fees can differ across apps).
- v1 restriction, relaxable later: newApp.publisher == oldApp.publisher.
  That covers the rebrand case exactly and defers every question about
  moving a deployment into someone else's catalog economics.
- Record: image reference becomes catalog://newAppId/version. NOTHING else
  changes: id, owner, escrow, maxRate, config override, secrets, custom
  domains all stay, so the hostname keeps naming the same record and the
  attestation story stays true (hostname id == ledger id == quoted VM).
- Runner: indistinguishable from an upgrade restart; no claim change, no
  new availability flag unless old runners validate the appId in the
  reference against the one they claimed (if so, gate on schema rev).

MCP: `build_upgrade` grows an optional `app` param ([publisher/]slug or
appId); when present it emits the retarget call. CLI: `enclave upgrade
<id> <version> --app eyesoff-ai`.

## Migration for 0xcc1f4f3f... once shipped

1. Confirm the eyesoff-ai version's cid actually holds the current build
   (crate 0.42.1: rename + de-jargoned UI); publish 1.0.2 first if 1.0.x
   predates it.
2. `build_upgrade {id: 0xcc1f4f3f..., app: "eyesoff-ai", version: "1.0.x"}`,
   owner signs, runner restarts in place.
3. Verify: same URL answers, GET /warmup (toolchain-hang canary), the
   attestation panel resolves the id, and get_config shows the intended
   config. Note the eyesoff-ai 1.0.1 template differs from the live
   deployment's tuning (max_calls 32 vs 8; nnCtx 131072): decide which
   should win and set the deployment override accordingly.
4. Retire the llm-chat listing at leisure (active:false), old versions
   stay resolvable for history.

## Alternative considered and rejected

Gateway alias (map cc1f4f3f to a fresh deployment's id off-chain): quick,
but the hostname stops naming the record it serves, the in-app attestation
status derives the id from the hostname and would name a record the quote
does not match, and the escrow/history split across two records. The
ledger retarget keeps every invariant; the alias breaks the one that
matters most here (verifiability).

# Deployment selection and authorization

The local SNP broker and owner connector accept exactly one binding mode:

- `--deployment-id 0x<64 hex digits>`: require the exact nonzero Enclave
  deployment ID in the guest's attested HOST_DATA.
- `--development-unbound`: explicitly omit the deployment check. This mode
  must not be presented as a wallet-bound production deployment.

The ID comes from the owner's trusted Enclave deployment flow. It is not
learned from a guest, worker, broker state file or an arbitrary generated local
identifier. Enclave's existing guestd protocol uses the deployment ID's raw
32 bytes as HOST_DATA (`isolation/m4/guestd/server.go`, `hostDataFor`). KVM Box
uses that same representation. It does not hash a new descriptor or add an
incompatible identity scheme.

The broker sets QEMU's `sev-snp-guest.host-data` to base64 of the raw bytes,
then requires its verifier to report both the expected value and an executed
HOST_DATA check. The owner connector supplies its own expected ID to the
existing SNP judge before forwarding the owner's credential or application
traffic. Both checks still require the image/app identity, certificate chain,
TCB policy, fresh nonce and TLS-key binding. Controller and desktop use the
same deployment label but retain their distinct expected measurements/AppIDs.

`state.json` exposes the launch mode for diagnostics. Its fields do not define
the owner's expected identity. The connector's separate command-line choice is
fixed for its lifetime. `start.sh` supplies one choice to both local processes;
a remote owner must supply their own expectation through the remote connection
flow once that flow is implemented.

Malformed, zero, missing, duplicate or conflicting binding arguments are
refused. Broker argument abbreviations are disabled. The launcher validates
binding arguments before starting services, including through a symlinked
checkout. An intentionally unbound connector can still reach a bound guest;
the development option explicitly omits that check, it does not attest an
all-zero deployment.

## Limits

HOST_DATA is a host-chosen label authenticated as part of the SNP report. It
does not authorize the wallet, establish a live lease, prevent a host launching
another genuine guest with the same label, or prove that a synthetic test ID
exists on-chain. The catalog, ledger owner, version selection, capability
routing, account authentication and lease lifecycle still require integration.
The current 0.6.11 desktop remains an unbound development run. This change does
not retroactively bind it or publish it.

The existing local owner password remains baked into the image as a hash;
shared catalog-image enrollment needs the production account protocol. The
current connector assumes a local forwarder; remote transport also remains
separate work. No private wallet keys are needed for binding verification.

## Verification

`python3 -m unittest discover -s tests -p test_deployment_binding.py -v`
checks Python/JavaScript wire agreement, exact QEMU byte encoding, invalid
inputs, explicit development mode, abbreviated/duplicate arguments and
validation before service creation through real and symlinked paths.
`test_release.py` retains the seven release-digest/path/caller regressions.

The hardware harness in the task's evidence launches separate controller SNP
guests from the exact 0.6.11 release. Synthetic test labels exercise correct,
wrong and absent bindings, replay rejection, and the actual owner connector.
The host state is deliberately populated with the connector's expected label
on misroute tests; the signed report still wins. A public invalid test password
reaches guest authentication only after successful attestation. No real owner
credential or wallet signature is used by these tests.

These tests establish the local SNP binding path, not production wallet
integration or Hyper-V attestation. They exercise the controller guest; the
same broker code emits the desktop launch argument, but a bound desktop
hardware rollout still needs its own acceptance evidence.

## NucBox deployment profile

The live 2026-10-24 inventory identifies NucBox K11 with Ryzen 9 8945HS,
Windows 11 Pro and the Radeon 780M worker. Its registered node is
`nucbox-k11-4`, backend `hyperv-partition-per-app`, with per-partition TLS and
`hostOsExcluded: true`, `operatorExcluded: false`, `inTee: false` in the public
fleet response. These are capability declarations, not an independent proof
of every advertised protection. SSH inventory independently confirms the
machine, CPU, OS and active hypervisor.

This target needs a separately measured Enclave Shield/Hyper-V desktop profile
and its existing partition verifier, not the EPYC Turin SNP release. The
chosen untrusted-GPU boundary remains mandatory: supported compute uses the
masked verified worker channel, remaining compute stays in the guest. Host-OS
isolation must not be described as excluding the hypervisor/physical operator.
The current production inference profile serves a pinned model and Wasm
components; it does not yet admit this native desktop image. Native desktop
profile admission, image/runtime identity, model volume policy, desktop TLS
routing and wallet/lease integration must be completed before publishing it.

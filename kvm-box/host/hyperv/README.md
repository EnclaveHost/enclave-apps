# NucBox desktop preparation

This directory prepares a separate Linux kernel for the native desktop inside
the NucBox OpenHCL per-app partition. It does not deploy a desktop, publish an
application, authorize a wallet, reserve a worker or qualify an attestation
policy. The existing SNP desktop remains a separate development image.

`prepare-kernel.py` owns its output copy. The source tree, configuration and
guest TPM transport must match explicit pins. The destination tree and receipt
must not exist. Do not copy the kernel tree manually and pass that copy as an
existing destination.

```sh
python3 prepare-kernel.py snapshot \
  --source-tree /path/to/recovered-kernel \
  --output /path/to/work/kernel-source.json

python3 prepare-kernel.py \
  --source-tree /path/to/recovered-kernel \
  --tree /path/to/work/new-kernel-copy \
  --source-manifest /path/to/work/kernel-source.json \
  --source-manifest-sha256 REVIEWED_MANIFEST_SHA256 \
  --driver /path/to/enclave/isolation/m3/vtpm/enclave_openhcl_tpm.c \
  --receipt /path/to/work/kernel-preparation.json
```

Review and pin the manifest independently before preparation. Generating a
manifest does not authenticate an upstream source release. The current input is
a recovered local 6.6.87.2 tree with existing generated files, not a certified
clean upstream source checkout.

The helper verifies every manifest entry, then verifies the copied tree before
running any preparation command. It refuses external symlinks, hard links,
special files, missing/extra/changed entries, existing destinations and receipts,
and paths that would write inside the source. Commands run through explicit
system tools with a minimal environment. The exact configuration changes are
allowlisted, including virtual input, guest TPM transport, firewall prerequisites
and the local version. Unexpected configuration changes fail preparation.

The receipt binds the helper, input manifest, source snapshot, driver, tools,
environment, commands, configuration changes and prepared snapshot. It starts
as `preparing` before the copy; success or failure replaces that journal
atomically. On failure, the owned partial tree stays available for diagnosis.
Use a fresh destination for a new attempt. Exit 2 is a validation refusal before
copying; exit 1 is a preparation failure after the journal was created.

The checks prevent accidental reuse, drift and source aliasing. They do not
protect against a malicious operating system or concurrent filesystem mutation.
Device permissions and process confinement for `/dev/uinput`, `/dev/uhid`, TPM
and report interfaces still belong to the measured guest image and must be
tested on the target.

Thirty synthetic tests cover pin/manifest tampering, source aliases and special
files, no-clobber behavior, hostile inherited environment variables, unexpected
configuration changes, retained failure journals and receipt binding. Run them
from the app root with:

```sh
python3 -m unittest discover -s tests -p test_hyperv_kernel.py -v
```

The experimental desktop V8 identity tooling is in the isolated Enclave guest
worktree, with its own cross-language vectors. Runtime admission remains
disabled until the full native companion, sealed-secret, masked-worker and
profile verification paths are implemented and tested.

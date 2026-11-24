UNEXECUTED diagnostic host harness. No remote grant exists for this plan.
The guest image built successfully; this host harness still needs syntax/cmdlet
validation and its acceptance tests run under resource guards. The first
read-only host AST query timed out; no VM/VHD/tasks were created. Metal0 recovery
now holds native operations. Do not launch until the exact plan is reviewed and
a fresh coordinated slot is obtained. Do not reuse a prior capture-only grant.

This is one new disposable VM and one new 1 GiB synthetic VHD, two cold boots,
no NIC or user state. The independent watchdog removes only the exact owned
VM and retains its synthetic VHD for diagnosis. Never substitute existing disks.
The plan contains machine-specific paths and one-attempt unique identities;
review/re-pin all files before use. A failure or ambiguous create/cleanup must
be reconciled, not retried automatically. Final acceptance also requires a fresh
independent VM/task absence query saved as cleanup-verification.json.

The image has a PUBLIC TEST KEY and placeholder binding digests. Even a passing
run would not qualify desktop/GPU/filesystem/attestation/custody recovery.

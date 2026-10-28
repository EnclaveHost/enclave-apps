PRIVATE VM SNAPSHOTS / LOCAL SIGNING

Open http://127.0.0.1:8769 in your wallet browser.

RISC Box private resumable snapshots are implemented and tested: 90 Rust tests,
seven HTTP/S3 checks on both runtime profiles, and both SET codegen selftests.
The production memory64/SET component is frozen as RISC Box 0.6.70.

The queue contains artifact upload, catalog publication and a gated atomic
version/config update. Owner-signed read, key merge and read-back prepare
private storage credentials. Migration preserves original objects and verifies
that the uploaded encrypted copies decrypt to byte-identical originals.
The existing inactive RISC deployment remains on 0.6.69 until its private key,
legacy snapshot migration and restore are verified. Publication requires the
owner's wallet signature and enough Base ETH for the checked maximum fee.

Native guest-managed checkpoint qualification is in progress. The disposable
availability diagnostic was built locally; that does not establish native
snapshot capture or restore support. No native publication is prepared.

The older direct-Sia/keyless-gateway rollout is superseded. Generic S3 remains
unchanged; Jot, Depot, IPNS Publisher and S3-IPFS adapter are outside this change.

Signing engine
--------------
The code implements sequential pinned upload, publish, create and update
requests. Upload authorization uses personal_sign, the established Enclave
upload endpoints, exact CID matching and full frozen-DAG CAR verification.
Transaction requests are encoded by the server from the reviewed plan.
Deployment appRef/config changes can be atomic through ledger.multicall;
vault-owned operations are wrapped in SessionVault.ownerCall. No browser
endpoint accepts arbitrary transaction calldata or contract addresses.

Preflight requires pinned contract code, unchanged address-book entries,
expected wallet/vault ownership, exact pre-state, dual-provider simulation
including post-state reads, no pending wallet nonce, reviewed fee/gas caps,
and enough balance for the maximum fee including Base's L1 fee. Verification
checks the exact transaction, canonical receipt block and post-state through
two RPCs with three confirmations. It does not prove runner health or native
attestation: those require separate production evidence.

Each request is journaled and fsynced before it reaches the wallet. Unknown
wallet outcomes remain held. An explicit 4001 rejection resets only its own
attempt. A transaction hash cannot be replaced. Lost transaction replies can
be recovered with the hash; confirmed, reverted or unresolved requests are
never silently resent. An interrupted upload that was already in progress
requires operator review/recovery; the page does not guess its outcome.

There is no browser control to override readiness. Preparing a real release
requires completing the blockers, freezing artifacts and deployment config,
reviewing exact code hashes/pre/postconditions and authority/vault holds,
then replacing the pinned plan and starting with its own journal. The current
plan must NOT simply be relabeled ready. Production preparation and execution
have not been exercised; current tests use fixtures for signing behavior.
No old journal may be deleted to bypass an unresolved transaction.

Keys and running versions
-------------------------
No actual snapshot keys are included in the public plan or catalog config.
The current RISC application is not CPU-TEE isolated. Snapshot encryption
protects stored objects; it does not protect from the app host or from an
operator of a relay-readable secret store.

No signatures, uploads, transactions or migrations have occurred.

Run
---
Node.js 22+: node server.mjs
Tests: ./verify.sh (mandatory queue, cgroup, host-memory and disk guards)
User service: sia-rollout-page.service
State: ../../work/sia-rollout-state (private; never discard an unresolved journal).
The server binds 127.0.0.1 only and enforces Host, Origin and POST tokens.

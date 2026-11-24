# sia-s3: replication across indexers

From 0.2.0, sia-s3 can keep its buckets on several Sia indexers at once. If those indexers are run by independent
operators, writes, reads and ongoing storage no longer depend on any single one of them. Distinct URLs do not prove
distinct operators; the deployer has to establish that. Code: `src/replica.rs` (the cluster), `src/store.rs` (one
indexer), `src/config.rs`.

Nothing here has been activated. No production indexer, deployment or catalog version uses it yet.

## Why replicate indexers, not move them

An indexer (indexd) is the one party that, for a given app key:

- holds the Sia contracts with the hosts, and renews them from its own Siacoin wallet;
- funds the app's host accounts, which every upload and download spends from;
- pins slabs (moves uploaded sectors into its contracts) and keeps the object list (sia-s3's whole index);
- repairs slabs (migrates sectors) when hosts go away.

The vendored SDK (`vendor/sia-sdk-rs/sia_storage`: `Sdk`, `Builder`) has no wallet, contract-formation, renewal
or account-funding API. An app can upload, download, pin, delete, list objects and list hosts, and all of that goes
through one indexer. Moving indexd or its PostgreSQL (today on minipc) to another machine moves that authority; it
does not remove it. Replication makes each indexer one of N replicas, each with its own contracts, funding and hosts.

## Configuration

```json
{
  "replicas": [
    { "name": "minipc", "indexer": "https://sia.enclave.host", "appKey": "$SIA_APP_KEY" },
    { "name": "b",      "indexer": "https://indexd.example.org", "appKey": "$SIA_APP_KEY_B" },
    { "name": "c",      "indexer": "https://sia.example.net",    "appKey": "$SIA_APP_KEY_C" }
  ],
  "writeQuorum": 2,
  "region": "auto", "buckets": ["ipfs"],
  "credentials": [{ "accessKeyId": "$S3_ACCESS_KEY_ID", "secretAccessKey": "$S3_SECRET_ACCESS_KEY" }]
}
```

- `replicas` replaces `indexer`. A config without it is one replica (`primary`).
- `replicas` can also be a single `"$SIA_REPLICAS"` string whose secret holds the JSON array. The owner can then
  change the set with a secrets update; in the version config, changing it needs a new catalog version.
- A replica's `appKey` defaults to the top-level `appKey`. A replica whose `$NAME` is unset is a config error. It
  never falls back to another indexer's key. Config debug output never prints a seed.
- Indexer URLs are parsed and canonicalized: the scheme, a lowercase host without a trailing dot, the port only
  when it is not the scheme's default, and the path without a trailing slash.
  - Refused: credentials, a query or a fragment, schemes other than http(s), and the same canonical URL twice (one
    endpoint must not count as two quorum votes).
  - Plain `http://` is allowed for a local or tunnelled indexer and logs a warning. Over it, anyone on the path sees
    which objects exist and can withhold or replay the indexer's answers. They cannot forge objects, which are
    signed and sector-proof checked.
- `writeQuorum` (W) defaults to a majority and must be between 1 and N.
- Deadlines (seconds): `metadataTimeoutSecs` 120, `uploadTimeoutSecs` 1800, `uploadGraceSecs` 30,
  `metadataGraceSecs` 5, `stallTimeoutSecs` 120. Maintenance: `repairEverySecs` 300, `repairMaxBytesPerPass` 1 GiB.
- The app key must already be registered on every indexer (indexd admin `POST /api/apps/register`, or the
  connect-key flow) with a quota that covers the data.

| N, W | Writes during one indexer outage | Restart during one outage |
|---|---|---|
| 1, 1 | no | no |
| 2, 1 | yes | needs both, or the dead one removed from the config |
| 2, 2 | no | yes |
| 3, 2 | yes | yes (recommended) |

## Fault model

- **Fail-stop indexers with durable acknowledgements.** An indexer may crash, hang, lose answers, or apply a
  request long after its client gave up. Everything it acknowledges, it keeps. W-of-N writes and N - W + 1 boot
  reads rest on this.
- **Partial rollback is tolerated.** Deletion floors are copied to every replica. A deleted key therefore stays
  deleted even if up to N - W indexers roll back to an older state or lose theirs (tested with one of three rolled
  back to before the DELETE). If more do at once, the key can come back.
- **Not Byzantine-fault-tolerant.** An indexer cannot forge objects: they are sealed and signed with the app key,
  and data is checked against Sia sector proofs. It can withhold objects, replay old ones, or lie about what it
  holds. A majority of indexers doing so defeats these guarantees. N - W + 1 does not cover lying indexers.
- **One writer.** Exactly one sia-s3 process may write to a set of app keys at a time. indexd has no fencing (no
  conditional or generation-checked pin). A second writer is invisible until a reload, and two writers break
  conditional PUTs and version ordering. Requests a stopped writer left in flight cannot bring a deleted key back
  (floors hide them). They may briefly surface an older version of an undeleted key until the newer one wins the
  merge.

## Guarantees under that model

- **Versions.** Every write is stamped with a millisecond clock that only moves forward (started past every version
  and floor loaded) plus a random write id. Replicas merge by taking each key's newest version; a newer deletion
  hides it.
- **Writes and boot.** A write succeeds when W replicas acknowledge it, and answers 503 `ServiceUnavailable`
  otherwise. The app serves once N - W + 1 replicas have loaded, so every acknowledged write is visible. A write that
  answered 503 may still become visible, as after an S3 500.
- **Deadlines and latency.** Every replica call has a deadline. Once W replicas have answered, the others get a
  grace period (30 s for uploads, 5 s for metadata) and are then abandoned. A hung or slow indexer delays a write by
  at most that grace, or fails it at the deadline when W cannot be reached. The write lock is never held past a
  metadata deadline. A streamed body drops a replica that stops taking data for the stall time. A read moves to the
  next replica after the stall time plus one second per MiB without progress, resuming at the first unsent byte.
  The same code runs natively and on wasm32-wasip2 (tokio timers).
- **Unknown outcomes.** A metadata call that fails or is abandoned may still be applied, at any later time. Its
  replica becomes *dirty* and is re-read from its indexer's object feed. No safety decision depends on how long ago
  that was.
- **DELETE** returns 204 only once the deletion is acknowledged on W replicas, as either:
  - a tombstone written into the key's host object on each replica that holds the key, or
  - a deletion floor in a new ledger object, on a replica that does not hold the key, or whose tombstone failed.

  Otherwise it answers 503, and a retry re-applies the same deletion. Any W set meets any boot set, so a boot always
  sees an acknowledged delete.
- **Deletion evidence is kept for good.** Maintenance copies every deletion into a floor on every replica. A floor
  is a key hash plus the deletion's version, about 80 bytes. It lives in a ledger object whose encrypted body is
  checked against the length and SHA-256 in its signed metadata.
  - Once a replica has pinned the floor, the tombstone leaves its object and the payload is reclaimed.
  - Any version older than a floor is dead wherever and whenever it appears: an abandoned pin applied hours later,
    or old data an indexer replays. It is never served, and it is purged.
  - Ledgers are merged when more than 16 pile up. The merged ledger is pinned before any old one is deleted, so no
    floor is ever dropped.
  - A replica with a ledger it cannot read does not load: availability is lost rather than deletion safety.
  - There is no time-based tombstone collection, and no option to discard floors.
- **Repair** copies each key's newest version to replicas that lack it. It reads from any replica that has the
  version and checks the size, and the MD5 for single-part ETags. It also applies missed deletes and purges dead
  versions.

## Feature parity and limits

- **Multipart.** Up to 10,000 parts. An entry that does not fit in 1 KiB of metadata gets a manifest (store.rs).
  The same applies to keys near S3's 1 KiB key limit and to copies onto crowded objects. Ranges, CopyObject,
  multipart ETags, SigV4 and conditional PUTs are unchanged.
- **0.1.x data** loads unchanged. Entries without stamps order by mtime.
- **Cold recovery.** `recovery export` writes snapshot version 2 when a manifest key is present. `recovery read`
  fetches the manifest body from the snapshot's hosts, verifies it, and needs no indexer (tested with the mock
  indexer offline).
- **Cost.**
  - Every replica stores and uploads its own copy (N times the storage and upload).
  - A manifest adds one small upload.
  - Floors add about 80 bytes per deleted key, forever. Ledger writes cost one small upload per replica per
    maintenance pass that has new deletions.
  - A delete on a replica that lacks the key costs a ledger upload (about 45 s on the live network) before it is
    acknowledged.
  - Repair writes one object per key: keys that shared an object (copies, packed files) get one each on the
    repaired replica.
- **Rollback to 0.1.x.** Not supported once deletes, manifests or replicas exist. 0.1.x ignores floors and manifests
  (it leaves them alone as foreign v2 objects), so older versions could reappear and manifest keys disappear in it.

## What stays centralized, and what activation needs

| Still a single party or a manual step | Why |
|---|---|
| Each indexer operator, for its replica | contracts, renewal, funding, slab repair and the object list live in indexd; the SDK has no path to app-held contracts |
| The sia-s3 deployment (one writer) | indexd has no fencing; failover of the writer is an Enclave-level concern |
| The owner's signing authority | secrets (app keys, S3 credentials) and catalog versions |
| Registration on each indexer | an indexer admits an app key by its operator's action (any indexd that registers it can be listed) |

Activation needs, none of it done here:

1. One or more additional indexd instances (v0.6.0, optionally with `indexd/enclave-small.patch`) with PostgreSQL,
   run by operators verified to be independent of the minipc one.
2. A funded Siacoin wallet per indexer, for on-chain contract formation and renewal. About 14 GB of contracted
   storage per replica for today's ~4.7 GB.
3. The sia-s3 app key registered on each indexer with quota.
4. A real-network validation of this code against those indexers. Mock tests do not cover indexd's real timing,
   errors or 1 KiB limit enforcement.
5. A review, then a catalog version built from a committed, clean checkout (not from any test build) and published
   with the owner's existing authority, and a redeploy with the replica list and keys as secrets.
6. The initial backfill: watch `/healthz` → `replicas[].behind` fall to 0.

## Operator procedures

- **Add a replica.** Register the app key on the new indexer, add it to the list, keep N - W + 1 reachable for the
  next restart, and restart. Repair backfills it, and maintenance gives it every floor.
- **An indexer is down for a while.** With W reachable, clients see no change. When it returns it is re-read and
  caught up. A restart during the outage needs N - W + 1 other replicas.
- **An indexer is gone for good, or censored.** Remove it and restart. Re-adding an indexer account that missed
  writes is safe for deletions (floors from the others reach it), but a fresh app key is cleaner.
- **Restart or replace the writer.** Stop the old process first: one writer at a time.
- **Recover without any indexer.** `sia-s3 recovery export <indexer> <snapshot>` per replica, then `recovery read`
  (README).

## Validation (native mock tests; no real indexer, host or deployment)

- Faults injected into the mock indexer:
  - a lost response after the pin was applied;
  - a call that never answers;
  - a call applied only when the test releases it, after any number of listings and restarts;
  - an offline indexer;
  - a replica rolled back to a state from before a DELETE.
- Data goes through the real SDK upload and download path: erasure coding, encryption, sector proofs.
- Main cases:
  - A deleted key stays deleted in each of these situations:
    - a pin lands after the DELETE, after maintenance reclaimed the payload, and after a cold restart;
    - stale pre-delete data is replayed;
    - a commit's response is lost;
    - a replica rolls back.
  - A hung replica delays a W=1 write only by the grace period; with W=2 it fails at the deadline.
  - Writes continue through an outage and are backfilled.
  - DELETE refuses until the deletion is durable on W replicas.
  - A 16-part manifest key, a 1000-character key, and copies of both work across replicas, survive a reload, and are
    reclaimed down to floors.
  - Cold recovery of manifest keys works with the indexer offline.

### Test memory: the disk-backed mock

Every object these tests pin is a full 12-shard slab of 4 MiB sectors. The SDK really does encrypt, erasure-code and
root-hash all of them. The vendored mock host transport kept every sector in RAM, so the 16-part manifest test on two
indexers needed more than 2 GiB and could not run under a bounded test cgroup.

**What changed.** The vendored mock (`vendor/sia-sdk-rs/sia_storage/src/rhp4/mock.rs`, `src/mock.rs`) has an optional
disk backend: `MockNetwork::with_disk_sectors(base, limit)`.
- It is opt-in. `MockNetwork::new()` still keeps sectors in memory, exactly as before.
- It is test-only: the mock module exists only under `cfg(test)` or the SDK's `mock` feature, which sia-s3 enables only
  as a dev-dependency.
- It is native-only (`cfg(not(target_arch = "wasm32"))`). The production wasm build contains none of it.

**What stays the same.** Only where the bytes rest changes:
- the SDK path: encryption, erasure coding, sector roots;
- the indexer faults;
- slow hosts and read failures;
- `clear_sectors`.

**Where the files go.**
- Each disk-backed network creates a fresh, uniquely named directory under `target/mock-sectors/`, which is on disk, not
  the tmpfs `/tmp`.
- Files are `<hex host key>/<hex sector root>.sector`, derived only from the two 32-byte binary values. No
  caller-supplied string reaches the filesystem.

**How it reads.** A read opens the file, seeks to the range and reads exactly that range.
- Its errors match the in-memory store's: "host not found" and "sector not found".
- An out-of-range request is an error instead of the in-memory slice panic.

**Writes and the budget.** Writes go to `.part` and are then renamed. A write over the byte budget, or any I/O failure,
fails the write; it is never dropped silently.

**Cleanup.** On drop, the store removes exactly the files it wrote, then the empty host directories, then its own
directory. Nothing is removed recursively, so anything else found there is left alone and reported.

**Tests.**
- `disk_and_memory_mock_sectors_answer_alike` compares both backends through the real SDK:
  - complete bytes, and ranges at the sector edges and mid-sector;
  - the identical read failure once hosts lose their sectors;
  - only hash-named files, exactly 4 MiB each, inside the store's own directory;
  - an enforced budget;
  - the directory removed on drop.
- Only `many_part_uploads_and_long_keys_use_manifests_everywhere` uses the disk backend. Its assertions, part count,
  replicas and recovery steps are unchanged.

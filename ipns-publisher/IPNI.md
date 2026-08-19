# HTTP content provider (IPNI) — Step 0 findings and design

The IPNS half answers *name → CID*. This half answers *CID → bytes*: it makes
the site's DAG discoverable and retrievable over **HTTP** from the
s3-ipfs-adapter (`https://ipfs.enclave.host`), so third-party gateways
(ipfs.io, dweb.link/Rainbow, eth.limo) can fetch it with **no libp2p/bitswap
node in the loop** — replacing nan's Kubo as the site's only provider.

## Step 0 — what actually works (probed 2026-08-19, not transcribed from specs)

**The site's only provider today is Kubo, bitswap-only.** The delegated
router returns exactly one provider for the site root
(`bafybeiclw67…`): peer `12D3KooWA49n…` at `62.238.4.214` (enclave.host's own
box), advertising `/tcp/4001`, `/quic-v1`, `/webrtc-direct`, `/webtransport`
— every transport but HTTP. That is precisely the dependency this replaces.

**Mechanism A (delegated-router provider *writes*) is not available.**
`PUT https://delegated-ipfs.dev/routing/v1/providers/<cid>` → 405,
`PUT …/routing/v1/providers` → 404. The `/routing/v1` providers surface is
read-only on the router the target gateways consult.

**Mechanism B (IPNI advertisement chain + announce to cid.contact) is the
route, and cid.contact already ingests HTTP providers.** `GET
https://cid.contact/providers` returns live providers whose only address is
`/dns/…/tcp/443/https` — HTTP-transport providers, exactly the shape we need.
The announce endpoint is **`PUT https://cid.contact/announce`** (verified:
`PUT` → 400 on a bad body i.e. the verb is accepted; `GET`/`POST` → 405).

**The content is not in the adapter yet.** `https://ipfs.enclave.host/ipfs/<site-root>?format=raw`
→ 404 `"CID … is not in this gateway's index (it only serves the configured
bucket)"`. The adapter is healthy (`/healthz` 200, `server: s3-ipfs-adapter/0.3.0`)
but serves only its configured (machines) bucket. **Announcing the real site
root is therefore gated on migrating the site's DAG into the adapter's R2
bucket** — announcing a root the adapter can't serve would be a false provider
claim (an explicit no). Until then the mechanism is proven against a throwaway
CID that the adapter (or a test gateway) actually serves.

**Verification substrate:** go 1.26 is available locally, so every wire format
below is checked **byte-for-byte against go-libipni** in offline vector tests
(`scripts/ipni-vectors/`), the same "match the reference implementation
exactly" doctrine the IPNS record codec already follows against kubo. The
final live-gateway fetch is a post-deploy gate (needs a public publisher URL
and real content in the adapter).

## The exact formats (from ipni/specs + go-libipni source, pinned here)

**Advertisement** (`ingest/schema`, dag-cbor), fields in schema order:
`PreviousID` (optional Link), `Provider` (String = our ed25519 peer id),
`Addresses` (`["/dns4/ipfs.enclave.host/tcp/443/https"]` — the adapter, the
*retrieval* endpoint), `Signature` (Bytes), `Entries` (Link → EntryChunk),
`ContextID` (Bytes), `Metadata` (Bytes), `IsRm` (Bool), `ExtendedProvider`
(optional, unused).

**Advertisement.Signature** (`ingest/schema/envelope.go`, matched exactly):
```
payload = multihash.Sum(
    previousID.Bytes()  (cid.Undef.Bytes() == empty, for the first ad)
  ‖ entries.Bytes()
  ‖ Provider            (utf-8)
  ‖ Addresses           (each utf-8, concatenated, no separators)
  ‖ Metadata
  ‖ IsRm ? 0x01 : 0x00,
  SHA2-256)            // == 0x12 0x20 ‖ sha256(buf), the multihash, not the raw digest
Signature = libp2p signed-envelope over `payload`
            domain = "indexer", payloadType = "/indexer/ingest/adSignature"
```
The libp2p envelope is the protobuf `{1: PublicKey{Ed25519,pub}, 2: payloadType,
3: payload, 5: sig}`, `sig = ed25519(varint-len-prefixed(domain‖payloadType‖payload))`.

**EntryChunk** (dag-cbor): `Entries` (`[Bytes]` = the block **multihashes**,
not CIDs), `Next` (optional Link → next chunk). Chunked so no block exceeds
the ~4 MiB ingest limit.

**Metadata** for HTTP retrieval: `transport-ipfs-gateway-http` = uvarint
`0x0920` with no trailing bytes → the Metadata field is exactly `[0xA0, 0x12]`.

**SignedHead** (`dagsync/ipnisync/head`, dag-json), served at
`GET /ipni/v1/ad/head`: `head` (Link → latest ad), `topic` (optional String,
default `/indexer/ingest/mainnet`), `pubkey` (Bytes, libp2p-marshaled),
`sig` (Bytes = `ed25519(head.Bytes() ‖ topic-utf8)`).

**Ad-chain HTTP surface** (`dagsync/ipnisync`, `IPNIPath = /ipni/v1/ad`): the
indexer crawls `GET <publisher>/ipni/v1/ad/head` then walks the chain via
`GET <publisher>/ipni/v1/ad/<cid>` (each ad and entry-chunk block, dag-cbor).

**Announce message** (`announce/message`, cbor-gen *tuple* not a map),
`PUT /announce` body: `[Cid, Addrs, ExtraData]` (3-array `0x83`; 4-array
`0x84` adds `OrigPeer`). `Cid` = the head ad CID (CBOR tag-42 link). `Addrs` =
array of **binary multiaddrs of the publisher** (this app's own public HTTPS
endpoint, where the indexer fetches the chain), *not* the retrieval endpoint.

## Two addresses, do not conflate them

- **Advertisement.Addresses** = where a client fetches the *content*:
  `/dns4/ipfs.enclave.host/tcp/443/https` (the adapter's trustless gateway,
  `GET /ipfs/<cid>?format=raw`).
- **Announce Message.Addrs** = where the *indexer* fetches the *ad chain*:
  this app's own `/dns4/<app-host>/tcp/443/https` (serving `/ipni/v1/ad/*`).

## Build plan (each milestone verified)

1. Findings (this file). ✓
2. IPNI codec — ad, entry chunk, signature, signed head, announce message —
   with go-libipni byte-for-byte vector tests.
3. Serve the ad chain (`/ipni/v1/ad/head`, `/ipni/v1/ad/<cid>`) + announce to
   cid.contact; multihash set pulled from the adapter's
   `?format=car&dag-scope=all`.
4. Live gate (post-deploy): announce a throwaway adapter-served CID, prove
   ipfs.io/dweb.link fetches it over HTTP with Kubo not providing it.
5. Re-announce on site-root change (chain to previous, optional IsRm of the
   old context); head/sequence persistence + announce retries; status in
   `/api/status`.

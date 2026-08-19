# ipns-publisher: an IPNS name, signed inside the enclave

Holds the ed25519 key of an IPNS name as a deployment secret inside the
TEE, signs records there, and publishes them outward. The stable name for
a site (enclave.host's, or anyone's) stops depending on a plaintext key
file on some box: the key lives in attestation, and every publish is an
outbound-only act.

```
   deployment secret            the enclave                        the world
   IPNS_ED25519_SK  ──>  sign IpnsEntry (V1+V2)  ──>  GET /routing/v1/ipns/<name>
                         sequence recovery,           PUT to delegated routing
                         republish timer              PUT_VALUE onto the IPFS DHT
```

Three publish surfaces, in order of increasing self-sufficiency:

1. **HTTP record server**: the app serves its signed record on the
   delegated-routing read surface (`/routing/v1/ipns/<name>`, and
   `/ipns/<name>?format=ipns-record`), so gateways and resolvers that can
   reach the deployment resolve the name without any DHT at all.
2. **Delegated-routing PUT** (IPIP-379): every new record is PUT to the
   configured delegates (default `https://delegated-ipfs.dev`), which
   announce it to the network on our behalf. This ships the key-custody
   story even where the DHT client is not in play.
3. **Direct DHT PUT_VALUE**: a hand-rolled, outbound-only libp2p stack
   (TCP + multistream-select + Noise XX + yamux + Kademlia) walks the DHT
   to the 20 closest peers and stores the record there, like kubo does.
   No rust-libp2p: it does not build for wasm32-wasip2; this stack is
   hand-rolled the way the suite hand-rolls S3, IPFS and HTTP.

## Step 0 finding: fleet egress is TCP-only

Recorded per the transport decision: the platform's egress front is SOCKS5
with **CONNECT only** (`egress.js` answers anything else GENERAL), and the
transparent-egress wasmtime shim permits "TCP bind/connect + UDP bind but
DENIES raw UDP egress" (`wasmtime-egress.patch`). So there is no outbound
UDP from a wasip2 guest: **no QUIC, no /udp multiaddrs**. The DHT client
filters every peer to its `/tcp/` addresses and skips QUIC-only peers,
which still leaves plenty of TCP-reachable DHT servers for a PUT.

## Record correctness

The record codec is verified byte-for-byte against kubo 0.42 (unit tests
embed a kubo-minted record: same key + same fields reproduces identical
bytes, V1 and V2 signatures included), and records minted here pass
`ipfs name inspect --verify` and are accepted by kubo's `/routing/v1`
PUT endpoint. CBOR keys ride in canonical order (TTL, Value, Sequence,
Validity, ValidityType); the V2 signature covers `ipns-signature:` + the
CBOR; V1 stays for back-compat; records stay under the spec's 10 KiB cap.

## Sequence monotonicity

IPNS resolvers keep the highest sequence they have seen (ties: longest
validity), so a publisher that forgets its sequence bricks its name until
the old record expires. This app has no durable disk guarantee, so on
boot it recovers: reads `/data/ipns-publisher-state.json` when the
platform persists it, and asks every delegate (later: the DHT) for the
current record, verifying the signature before believing a sequence. An
unchanged value keeps its sequence and extends the EOL; a changed value
publishes max(known)+1.

## Config

`ENCLAVE_CONFIG` (or `IPNSPUB_CONFIG` locally), JSON; `$NAME` string
values resolve from the environment (deployment secrets):

```json
{
  "ipnsKey": "$IPNS_ED25519_SK",
  "value": "/ipfs/bafy…",
  "lifetimeSecs": 172800,
  "republishSecs": 14400,
  "ttlSecs": 3600,
  "delegates": ["https://delegated-ipfs.dev"],
  "bootstrap": ["/ip4/…/tcp/4001/p2p/12D3Koo…"],
  "dht": true,
  "api_key": "$API_KEY"
}
```

- `ipnsKey` — the only secret. Hex or base64 of any of: a 32-byte ed25519
  seed, a 64-byte seed||pub, or the libp2p PrivateKey protobuf that
  `ipfs key export` writes. Only ed25519 names are supported.
- `value` — what the name points at: an `/ipfs/…` path (any `/…` path is
  passed through), a bare CID, or an `http(s)://` URL that answers the
  current CID in its body (first line, `/ipfs/…` or bare CID), fetched at
  every publish.
- `delegates` — IPIP-379 endpoints that take the PUT; also queried for
  sequence recovery on boot. Empty list disables the fallback.
- `bootstrap` — DHT entry points, `/ip4|ip6|dns*/…/tcp/…/p2p/…` form;
  defaults to the Protocol Labs bootstrappers.
- `dht` — set `false` to run http-only (record server + delegates).
- `api_key` — protects `POST /publish` (as `X-Api-Key` or `?key=`; the
  fleet TLS proxy strips `Authorization`, so no Bearer).

## API

| route | what |
|---|---|
| `GET /routing/v1/ipns/<name>` | the signed record, `application/vnd.ipfs.ipns-record` (404 for any name but ours) |
| `GET /ipns/<name>?format=ipns-record` | same record, gateway-style (also via `Accept:`) |
| `GET /api/status` | identity, sequence, EOL, per-delegate outcomes, DHT state |
| `GET /` | status page |
| `GET /healthz` | `{"ok":true}` |
| `POST /publish` | re-resolve the value and publish now (`api_key`-guarded) |

## Build & test

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/ipns-publisher.wasm

cargo test          # unit: kubo byte vectors (records, identities), bases, urls
scripts/e2e.sh      # against a local kubo: sign, PUT, recover, serve
```

Local run:

```bash
wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8000=18500' \
  --env 'IPNSPUB_CONFIG={"ipnsKey":"<hex seed>","value":"/ipfs/bafy…","delegates":["http://127.0.0.1:8080"]}' \
  target/wasm32-wasip2/release/ipns-publisher.wasm
```

Dev subcommands (native build): `ipns-publisher id <key>` prints the
identity chain; `ipns-publisher mkrecord <key> <value> <+secs|rfc3339>
<seq> <ttl-ns>` writes a signed record to stdout.

## Deploy on enclave.host

Publish the component (guide topic "publish"), then deploy CPU-only with
port `http:8000` (fleet policy: never 8080), egress enabled, and the
config above with `$IPNS_ED25519_SK` set as a deployment secret. The
engine reads `ENCLAVE_PORTS` and binds the actual port, as ever.

# sia-s3: production deployment runbook

Prepared 2026-10-13/14 (UTC), **not executed**. Nothing here has been published, pinned, deployed, signed or changed.
Every claim cites code (`main:` = enclave repo `origin/main` f02f2460e, `guest@<sha>` = the commit that built the NucBox
guest/host binary pinned in `main:windows/installer/build-bundle.ps1:31,37`) or a live read-only probe. **UNVERIFIED**
marks what I could not prove.

Goal: (1) publish + deploy `sia-s3` (CPU-only, `http:8000`, egress, secrets) on the NucBox; (2) bulk-import the adapter's
bucket into it; (3) deploy a SECOND `s3-ipfs-adapter` deployment pointed at sia-s3; (4) verify; (5) switch nan's Caddy
from 0x7ae476a3 to the new adapter. Old adapter + R2 stay up for rollback.

## 0. Blockers and decisions (read first)

| # | Item | Status |
|---|---|---|
| B1 | **The version config MUST contain `"egress": "public-web"`.** Without it a Shield partition only gets forwarders for the `https://` origins named in its config (443, names only); every SiaMux dial to a host's `:9984` is refused. With it the guest gets a SOCKS front at `127.0.0.2:1080` (any public IP or name, any TCP port except 0 and 25) and `ENCLAVE_EGRESS=socks5://guest:public-web@127.0.0.2:1080`. Evidence: `guest@c85e0c592:isolation/m2/shieldconfig/publicweb.go:16,23-35`; commit 33f7e73fe message; live risc-box config carries `"egress":"public-web"` (catalog read below). | Fixed in the config in step 1 |
| B2 | **NucBox nucbox-k11-4 is FULL: 8/8 app slots** (`/enclaves` → `availability.apps.running 8, capacity 8`, `nodeSlotsFree 0`). The plan needs 2 more slots (sia-s3 + adapter #2) during the overlap, 1 more after the old adapter retires. `APP_SLOTS` comes from `C:\ProgramData\Enclave\node-config.cmd` (`main:windows/node/agent.mjs:244`, `main:windows/installer/install-host.ps1:309`); the installer caps it at 8 (`install-host.ps1:212`), and **every auto-update re-runs install-host.ps1** (`update-host.ps1:7,40`), so a hand edit to 10 is reverted at the next Windows release. | **Decision needed** (coordinator/Steven): stop a deployment (candidate: the market test RISC Box 0x2215bad4, owner = CLI key 0x3977…), or hand-set `APP_SLOTS=10` + restart the `\Enclave\Node` task and accept/hold auto-update, or use the single-adapter alternative (section 8) which needs only 1 slot |
| B3 | Owner wallet. Owner signatures are needed for create, fund, **and every secrets put** (message signature, `main:cli/enclave.mjs:2900-2906`). `enclave … --signer/--unsigned` cannot sign messages (`externalAccount.signMessage` throws, `cli/enclave.mjs:431-439`) and Frame is not installed. | Recommended: owner = agent wallet `0x29479Bf0…647C` (hot key, `ETH_AGENT_WALLET` in ~/.bashrc), pays ~$0.36/day total. Alternative: owner = governance Trezor → hosting is FREE on k11-4 (its registry payout wallet is 0x0b2d, ledger rev 12 self-hosting rule `main:contracts/EnclaveDeployments.sol:92-100`; the live adapter shows `rate 0`) but costs 4 more Trezor confirmations (2 creates via `--unsigned` + 2 secrets messages via `trezorctl ethereum sign-message`). |
| B4 | Money. Agent wallet has 0.0000825 ETH and 2.29 USDC (read 10-14 ~04:00Z); enough for 2 creates + funds (~0.00002 ETH gas). Moving it is a coordinator decision. Governance has 0.0044 ETH (publish costs ~0.00002 ETH). | Decision |
| B5 | App-to-app path (adapter #2 → sia-s3) goes adapter → its Enclave-network exit → the sia-s3 label's provider IP → back to the NucBox. `enclave-wg` refuses its own circuit's guard/exit IPs and a provider refuses its own IP (`main:network/wg/cmd/enclave-wg/main.go:211-219`, `main:network/wg-provider/kernel.mjs:34-40`), but shield-egress tries every resolved address on every one of the app's circuits (`guest@d24c508ca:isolation/m2/egress/app_routes.go:299-340`). It fails only if, on every circuit, every provider IP of the label is that circuit's own guard/exit. | Verify in step 5 (UNVERIFIED live); fallback in section 7 |
| B6 | Sia-side capacity: indexd wallet funded with ~6,244 SC, 37+ contracts; the import adds ~4.7 GB × 3 (10-of-30 erasure). | UNVERIFIED that the contracts' 8 GiB growth each covers it; watch indexd during the import |

No blocker on the inbound side: on the NucBox the SigV4 headers arrive intact (section A4).

## Live facts this runbook relies on (read-only reads, 2026-10-14 ~04:00Z)

- App catalog (address book `appCatalog`) = `0x18419CA2b502D423A8de6269AEeE171a378626e3`, `catalogSchema() = 9`,
  `owner() = 0x0b2d009c0c9Af05b12100D77F3c815fea822eE61` (the governance Trezor). Rev 9: the catalog owner's publishes
  are **approved on publish** (`main:contracts/EnclaveAppCatalog.sol:440,451`). Slug `sia-s3` under 0x0b2d is unused
  (appId `0x093fa917…c6`, 0 versions).
- Ledger (book `deployments`) = `0xB1783B61E1FDee6a6163DC3CdaC27bF32E7d117B` (rev 16d).
- s3-ipfs-adapter `1.0.13` (catalog index 13, CID `bafybeiew67vbyf…vzqti`, ports `http:8000`, mem 1024, cpu 1) config:
  `{"endpoint":"$S3_ENDPOINT","region":"auto","bucket":"ipfs","prefix":"","credentials":{"accessKeyId":"$S3_ACCESS_KEY_ID","secretAccessKey":"$S3_SECRET_ACCESS_KEY"},"upload":{"uploadKey":"$UPLOAD_KEY","allowOrigins":["https://enclave.host"]},"refreshSecs":300,"maxKeys":50000,"api_key":"$API_KEY","wasi":"0.2","_media":{…}}`.
  **The endpoint is a secret**, so adapter #2 reuses version 1.0.13 unchanged; no adapter publish is needed.
- Deployment 0x7ae476a3: owner 0x0b2d, `catalog://0x4306e588…7a7c/13`, cpuMilli 20, public, envelope
  `{"network":{"relay":"us-west"},"isolation":{"cpuTee":false,"gpuTee":false},"placement":{"hostId":"0x2fcef48d…447a"}}`,
  runner nucbox-k11-4, rate 0. `/api/status`: 1,218 files, 4,655,584,580 bytes, rootCid
  `bafybeiggednuab4iebrie72bdrflsfxw2zurfkazsqtgijthfi7e2ambzy`; prefixes `pins/` (467 objects, 4.61 GB, largest 796 MB) and
  `site/` (751, 44 MB).
- Hosts (`/enclaves`): **nucbox-k11-4** (hyperv-partition-per-app, secrets true, cpuShareFree 0.35, RAM free 39 GB,
  slots 0 free); **metal0** (cpuShareFree 0, `secrets: false`: cannot run either app); **pixel10-pvm-cpu** (maxAppMemMb 512,
  ~1 GFLOPS: excluded by memMb ≥ 1024).
- Artifact: `target/wasm32-wasip2/release/sia-s3.wasm`, a `wasi:cli/run` component (port mode, own listener). **The branch
  is still moving** (06636a7 landed while this was written and the wasm was rebuilt: 3,228,646 B, sha256 `9dbc5f11…689e0f`,
  CID `bafybeie6bbi7rlgwez7fdpndx5lhlqqfpxxlyyc5lvbnt4umrwo2b5g65e`). Publish from a clean commit and recompute right
  before step 1: `CID=$(ipfs add -Q --only-hash --cid-version=1 --chunker=size-262144 <wasm>)` (the gateway recomputes the
  same CID; memory enclave-catalog-wasm-unpinned). Commands below use `$WASM` and `$CID`.
- Indexer: `https://sia.enclave.host` (nan Caddy → indexd 127.0.0.1:9982). App key seed `nan:/etc/indexd/apps/sia-s3.seed`
  (root 0600, 64 hex chars + newline), already registered on indexd.
- `UPLOAD_KEY` lives in `nan:/etc/nan-relay/api-relay.env` (one `UPLOAD_KEY=` line), consumed by `enclave-api-relay.service`.

## 1. Publish sia-s3 (one Trezor confirmation)

### 1a. Pin the wasm (no Trezor)

`/v1/apps/upload-token` accepts ANY wallet's `enclave-upload:<sha256>:<expiry>` signature; the wallet is only a rate-limit
identity (`main:relay/api-relay.js:1873-1904`). The CLI cannot do this step for a hardware wallet (`pinBytes` →
`account.signMessage`, `cli/enclave.mjs:2515-2534`; external accounts throw, `:431-439`), so pin with the CLI key and
publish the catalog record separately from the Trezor:

```bash
cd ~/.cache/enclave-wg   # any dir whose node_modules has viem
cat > /tmp/pin-sia-s3.mjs <<'EOF'
import fs from "node:fs"; import crypto from "node:crypto"; import { privateKeyToAccount } from "viem/accounts";
const bytes = fs.readFileSync(process.argv[2]);
const hash = crypto.createHash("sha256").update(bytes).digest("hex"), expiry = Math.floor(Date.now() / 1000) + 300;
let pk = fs.readFileSync(process.env.HOME + "/.config/enclave/key", "utf8").trim(); if (!pk.startsWith("0x")) pk = "0x" + pk;
const signature = await privateKeyToAccount(pk).signMessage({ message: `enclave-upload:${hash}:${expiry}` });
const t = await (await fetch("https://api.enclave.host/v1/apps/upload-token", { method: "POST",
  headers: { "content-type": "application/json" }, body: JSON.stringify({ hash, expiry, signature }) })).json();
if (!t.token) throw new Error(JSON.stringify(t));
const up = await fetch("https://ipfs.enclave.host/add-wasm", { method: "POST", body: bytes, headers: {
  "content-type": "application/wasm", "x-upload-address": t.address, "x-upload-expiry": String(expiry), "x-upload-token": t.token } });
console.log(up.status, await up.text());
EOF
WASM=/home/steven/Projects/enclave-apps-wt/sia-s3/sia-s3/target/wasm32-wasip2/release/sia-s3.wasm
CID=$(ipfs add -Q --only-hash --cid-version=1 --chunker=size-262144 "$WASM")
cp /tmp/pin-sia-s3.mjs ./pin-sia-s3.mjs && node pin-sia-s3.mjs "$WASM" && rm pin-sia-s3.mjs
# expect 200 {"cid":"<$CID>"}; then:
curl -s -o /dev/null -w '%{http_code}\n' -r 0-99 https://ipfs.enclave.host/ipfs/$CID  # 206
```

The pin lands in the R2 bucket via the live adapter. Once the version is approved, nan's hourly
`enclave-pin-adapter.timer` also pins it into local Kubo (it pins every approved catalog version,
`nan:/opt/enclave-gateway/pin-adapter-wasm.sh`), so the cold-start fallback covers sia-s3 too.

### 1b. Catalog record: `publishVersion` from 0x0b2d

Ports, config, resources and fee are all **part of the immutable version** and covered by its approval
(`main:contracts/EnclaveAppCatalog.sol:100-123`, ports ≤ 96 bytes `:210`, fee `:305-325,380-381`). Choices:

- `ports = "http:8000"`. Never 8080: Shield runs serve-mode apps on 127.0.0.1:8080, and a run-mode app gets
  `ENCLAVE_PORTS=http:N=N` for its declared port 1-49999 (`guest@c85e0c592:isolation/m3/domexec.c:342,358-371`); the fleet's
  reserved labels are 1080/8080/8090/8091 (memory: enclave-port-range-49999). The CLI also falls back to appPort 8080 when
  a version declares no `http:` port (`cli/enclave.mjs:2074-2077`).
- `res = [vramMb 0, gpuGflops 0, memMb 2048, cpuGflops 62]` (order per `cli/enclave.mjs:2646-2651`). 62 nominal GFLOPS
  ≈ 1 vCPU ≈ 6.2% of k11-4; 2048 MB is a guess for slab buffers (UNVERIFIED sizing; memory is not priced). memMb ≥ 1024
  keeps the phone (maxAppMemMb 512) from claiming it.
- `fee = 0`.
- `config` (≤ 4096 B inline; stamped `wasi:"0.2"` as the CLI would, `cli/enclave.mjs:2676-2712`):

```json
{"indexer":"https://sia.enclave.host","appKey":"$SIA_APP_KEY","region":"auto","buckets":["ipfs","s3check"],"credentials":[{"accessKeyId":"$S3_ACCESS_KEY_ID","secretAccessKey":"$S3_SECRET_ACCESS_KEY"}],"egress":"public-web","wasi":"0.2"}
```

`"egress":"public-web"` is B1 (decided from the MEASURED, unresolved config, so a secret can never switch it,
`publicweb.go:21-23`). The `s3check` bucket is for end-to-end checks without writing test keys into `ipfs` (which the
adapter indexes). sia-s3 ignores unknown keys (`src/config.rs`).

Build and simulate (simulation from 0x0b2d **passed** on 2026-10-14 with an equivalent config: returns appId
`0x093fa917…`, index 0; `cast estimate` = 731,757 gas):

```bash
CFG='{"indexer":"https://sia.enclave.host","appKey":"$SIA_APP_KEY","region":"auto","buckets":["ipfs","s3check"],"credentials":[{"accessKeyId":"$S3_ACCESS_KEY_ID","secretAccessKey":"$S3_SECRET_ACCESS_KEY"}],"egress":"public-web","wasi":"0.2"}'
DATA=$(cast calldata "publishVersion(string,string,string,string,string,uint32[4],string,string,uint256)" \
  sia-s3 "Sia S3" "S3 endpoint backed by Sia: objects are encrypted and erasure-coded inside the enclave" 0.1.0 \
  "$CID" "[0,0,2048,62]" "http:8000" "$CFG" 0)
cast call --from 0x0b2d009c0c9Af05b12100D77F3c815fea822eE61 0x18419CA2b502D423A8de6269AEeE171a378626e3 "$DATA" --rpc-url https://base-rpc.publicnode.com
```

Sign on the Trezor with the proven kit (trezorctl offline legacy tx, signed bytes verified before broadcast;
memory steven-trezor-signing, one-off-signing-localhost):

```bash
mkdir -p ~/enclave-prod/sia-s3-publish && cp ~/enclave-prod/connectivity-activation/trezor-send.sh ~/enclave-prod/sia-s3-publish/
node -e 'require("fs").writeFileSync(process.env.HOME+"/enclave-prod/sia-s3-publish/trezor-txs.json", JSON.stringify({txs:[{what:"catalog publishVersion sia-s3 0.1.0", to:"0x18419CA2b502D423A8de6269AEeE171a378626e3", data:process.argv[1], gas:"900000"}]},null,1))' "$DATA"
bash ~/enclave-prod/sia-s3-publish/trezor-send.sh    # Steven confirms on the Trezor (m/44'/60'/0'/0/0)
```

(Or `node ~/enclave-prod/connectivity-activation/serve.mjs`-style localhost page with the browser wallet.) Then confirm:
`node <main>/cli/enclave.mjs apps sia-s3 --json` → `approval: "approved"`, cid as above, version index 0. Do not
blind-retry a "timed out" publish: read the catalog first (memory: enclave-governance-gas-tank).

## 2. Deploy sia-s3

Use the **main** CLI (v1.5.0). The installed `~/.local/bin/enclave` is 1.2.0 (Aug 27). One way:
`git -C ~/Projects/enclave archive origin/main | tar -x -C <dir>` then `ln -s ~/Projects/enclave/node_modules <dir>/node_modules`
and run `node <dir>/cli/enclave.mjs …` (verified: prints 1.5.0 and reads availability).

Secrets (env-var names, single-line, ≤ 4 KB each; staged on the relay between create and fund so the first boot has them,
`cli/enclave.mjs:2208-2224,2312-2328`; injected as guest env only, memory enclave-deployment-secrets):

```bash
umask 077; S=~/enclave-prod/sia-s3-secrets; mkdir -p $S
AK=$(openssl rand -hex 10 | tr a-f A-F); SK=$(openssl rand -base64 30 | tr -d '/+=' | cut -c1-40)
{ echo "SIA_APP_KEY=$(ssh nan 'tr -d "\n" < /etc/indexd/apps/sia-s3.seed')"; echo "S3_ACCESS_KEY_ID=$AK"; echo "S3_SECRET_ACCESS_KEY=$SK"; } > $S/sia-s3.env
```

Losing `SIA_APP_KEY` loses the data; leaking it exposes it. Keep `sia-s3.env` 0600 and off git.

Create (owner = agent wallet, B3/B4):

```bash
export ENCLAVE_KEY=$(grep -oP '^export ETH_AGENT_WALLET=\K.*' ~/.bashrc | tr -d "\"'")   # must derive to 0x29479Bf0…647C
node <main>/cli/enclave.mjs deploy sia-s3:0.1.0 --cpu 0.07 --isolation hyperv-partition-per-app \
  --max-rate 0.05 --fund 1 --secrets-file ~/enclave-prod/sia-s3-secrets/sia-s3.env --no-wait --yes
```

- `--isolation hyperv-partition-per-app` makes only Shield hosts eligible (`cli/enclave.mjs:2099-2113`; k11-4 enforces it,
  `main:windows/node/chain.mjs:560-564`; the Shield verifier accepts exactly this value, `main:relay/shield-app-verifier.mjs:26-31`).
  Do NOT pass `--waf`: Shield refuses protection rules (`shield-app-verifier.mjs:32-33`).
- `--max-rate 0.05`: the default cap is the CHEAPEST live price (the phone posts 2 µUSDC/s/node); k11-4 posts 40, so for
  70 milli the cap (1 µ/s) would be below k11-4's 3 µ/s and the ledger refuses the claim (cap checked at claim/renew,
  `main:contracts/EnclaveDeployments.sol:72-74`; CLI default `cli/enclave.mjs:2263`). Cost at k11-4: ~3 µUSDC/s ≈ $0.26/day.
- Must be public (default) and CPU-only: Shield secret release refuses anything else
  (`main:relay/shield-secrets.mjs:29`, `shield-app-verifier.mjs:8,53`). Egress exists only for secret (V6/V7) domains
  (`main:windows/vbslike/datapath/node-bridge.mjs:140-178`; host sets `ENCLAVE_SECRETS_V1/EGRESS_V1/CONFIG_SOCKET_SERVER_V1`,
  `install-host.ps1:257-260`), and the node enrolls every public, leased, ready deployment in the Enclave network
  automatically (`main:windows/node/network-enroll.mjs:93-94`). No operator list to edit.
- `--no-wait`: the CLI otherwise waits ~3 min for a claim, which B2 will block.
- With B2 unresolved it sits `queued` (funded). Once a slot frees: `POST https://api.enclave.host/v1/claim-hint
  {"id":"0x<full id>","force":true}` (memory: enclave-catalog-wasm-unpinned).

Expect after claim: partition spawn + guest proof, then a ZeroSSL/LE cert (~3-6 min; LE's weekly duplicate limit for k11-4
names was spent this week, memory nucbox-version-switch-certs), then `https://<first 8 hex of id>.app.enclave.host/healthz`.
sia-s3 binds its port before contacting the indexer and retries boot 5 s→300 s (`src/main.rs:52-75,160-175`), so it
reports ready (front `/.well-known/enclave-ready` dials the app port) even before egress is up; no `startupEgress` flag
is needed (`main:windows/node/startup-egress.mjs:13`). "refused:admit" egress for a few minutes after a partition start
is normal.

## 3. Verify sia-s3 through its public origin

`/healthz` (unauthenticated, counts only) must show the store loaded (no boot error). Then a SigV4 round trip through the
real ingress (proves Authorization + Host survive and egress to Sia works):

```bash
EP=https://<label>.app.enclave.host; set -a; . ~/enclave-prod/sia-s3-secrets/sia-s3.env; set +a
head -c 3000000 /dev/urandom > /tmp/s3c.bin
curl -sS --aws-sigv4 "aws:amz:auto:s3" --user "$S3_ACCESS_KEY_ID:$S3_SECRET_ACCESS_KEY" -T /tmp/s3c.bin "$EP/s3check/probe.bin"   # PUT ~45 s
curl -sS --aws-sigv4 "aws:amz:auto:s3" --user "$S3_ACCESS_KEY_ID:$S3_SECRET_ACCESS_KEY" "$EP/s3check?list-type=2"
curl -sS --aws-sigv4 "aws:amz:auto:s3" --user "$S3_ACCESS_KEY_ID:$S3_SECRET_ACCESS_KEY" -r 0-1023 "$EP/s3check/probe.bin" | cmp - <(head -c 1024 /tmp/s3c.bin)
```

`scripts/s3check.py s3check` is the full suite but speaks plain HTTP only (`http.client.HTTPConnection`, line 52): run it
with a one-line local HTTPS change, or against a local `wasmtime run` instance. sia-s3 requires `x-amz-content-sha256`
(hex or `UNSIGNED-PAYLOAD`, `src/sigv4.rs:126-134`); if a curl call gets `NotImplemented … hex digest`, add
`-H "x-amz-content-sha256: UNSIGNED-PAYLOAD"` (curl signs headers you pass). UNVERIFIED which curl behaviour this box has.

## 4. Import the adapter bucket into sia-s3

`sia-s3 import <bucket> <manifest>` packs objects into 512 MiB batches with the same metadata the server writes, skips
keys already present with the same size (re-runnable), and **a running server sees imported keys only after it restarts**
(README "Bulk import"). nan has no wasmtime; the workstation has wasmtime 49.0.1. Run on the workstation (it uploads
~3× the data, ~14 GB, to Sia hosts over this uplink), or install a static wasmtime on nan (a production-box change: ask).

```bash
# manifest: every object the live adapter indexes (1,218 today), fetched from the live adapter by CID
curl -s https://7ae476a3.app.enclave.host/api/files > /tmp/files.json
python3 -I -c 'import json;f=json.load(open("/tmp/files.json"));json.dump([{"key":x["path"],"size":x["size"],"url":"https://7ae476a3.app.enclave.host/ipfs/"+x["cid"]} for x in f],open("/tmp/manifest.json","w"))'
set -a; . ~/enclave-prod/sia-s3-secrets/sia-s3.env; set +a
CFG='{"indexer":"https://sia.enclave.host","appKey":"$SIA_APP_KEY","region":"auto","buckets":["ipfs","s3check"],"credentials":[{"accessKeyId":"$S3_ACCESS_KEY_ID","secretAccessKey":"$S3_SECRET_ACCESS_KEY"}]}'
wasmtime run -S inherit-network -S allow-ip-name-lookup --dir /tmp::/tmp --env SIA_APP_KEY --env "ENCLAVE_CONFIG=$CFG" \
  ~/Projects/enclave-apps-wt/sia-s3/sia-s3/target/wasm32-wasip2/release/sia-s3.wasm import ipfs /tmp/manifest.json
```

UNVERIFIED: that the adapter serves every `site/…` file by its own CID at `/ipfs/<cid>` (it serves its whole index;
spot-check one before the run). `.enclave-index/leaves.bin` is not in `/api/files` and does not need importing (the adapter
rebuilds it). Then `enclave restart <sia-s3 id>` (owner key) so the server loads the imported keys. The deployment can
also be created after the import, which saves that restart.

## 5. Adapter #2 (same version 1.0.13, new secrets)

```bash
{ echo "S3_ENDPOINT=https://<sia-s3 label>.app.enclave.host"    # scheme://host only, no path (s3.rs Endpoint::parse)
  echo "S3_ACCESS_KEY_ID=$AK"; echo "S3_SECRET_ACCESS_KEY=$SK"
  echo "UPLOAD_KEY=$(ssh nan 'grep -oP "^UPLOAD_KEY=\K.*" /etc/nan-relay/api-relay.env')"
  echo "API_KEY=$(openssl rand -hex 24)"; } > ~/enclave-prod/sia-s3-secrets/adapter2.env
# UPLOAD_KEY must equal what the RUNNING api-relay uses; cross-check (prints nothing secret, just "same"/"DIFFERENT"):
ssh nan 'a=$(grep -oP "^UPLOAD_KEY=\K.*" /etc/nan-relay/api-relay.env); b=$(tr "\0" "\n" < /proc/$(systemctl show -p MainPID --value enclave-api-relay)/environ | grep -oP "^UPLOAD_KEY=\K.*"); [ "$a" = "$b" ] && echo same || echo DIFFERENT'
node <main>/cli/enclave.mjs deploy s3-ipfs-adapter:1.0.13 --cpu 0.02 --isolation hyperv-partition-per-app \
  --max-rate 0.02 --fund 1 --secrets-file ~/enclave-prod/sia-s3-secrets/adapter2.env --no-wait --yes
```

(`s3-ipfs-adapter:1.0.13` resolves to the 0x0b2d app's index 13; dry run with `--unsigned --from 0x29479Bf0…` on 10-14
produced a valid `EnclaveDeployments.create` to 0xB1783B61.) The adapter's own config has no `"egress"` key, so it gets
forwarder mode: its resolved `https://` origins (the `S3_ENDPOINT` value) get a 443 forwarder via `/etc/hosts`, which is
exactly what it needs (`guest@33f7e73fe` "Configured origins keep their forwarders").

Verify (the adapter re-hashes the whole bucket at first boot, ~80 min for ~4.7 GB; memory enclave-catalog-wasm-unpinned):

- `https://<adapter2>.app.enclave.host/api/status`: `state` hashing → `ready`, `files 1218` (or more), `error null`,
  `missing []`, and **`rootCid` equal to the old adapter's** (`bafybeiggednuab4…ambzy` at time of writing, or the old
  adapter's current value if pins landed since). Equal rootCid = byte-identical bucket.
- `diff <(curl -s https://7ae476a3.app.enclave.host/api/files | jq -S .) <(curl -s https://<adapter2>.app.enclave.host/api/files | jq -S .)` is empty.
- Spot reads: a few `/ipfs/<cid>` with `-r 0-99` and one of the 796 MB pins with a mid-file range.
- If `/api/status` shows S3 errors like `egress` / connect refused toward the sia-s3 label: that is B5 (section 7).

## 6. Cutover on nan (Caddy) and the pin script

Current vhost (`nan:/etc/caddy/Caddyfile`, block `ipfs.enclave.host`): reads go Kubo first and fall back to the adapter on
404/403; writes (`/add-wasm /add-json /add-image /healthz`) go to the adapter. The change is the adapter hostname in
the two `reverse_proxy` blocks, nothing else (keep the `@upload_preflight`, `/site-root`, `@gwcar`, error fallback):

```bash
ssh nan 'cp -a /etc/caddy/Caddyfile /etc/caddy/Caddyfile.pre-sia-cutover-$(date -u +%Y%m%dT%H%M%SZ) &&
  sed -i "s/7ae476a3\.app\.enclave\.host/<NEW8HEX>.app.enclave.host/g" /etc/caddy/Caddyfile &&
  grep -n "app.enclave.host" /etc/caddy/Caddyfile && caddy validate --config /etc/caddy/Caddyfile && systemctl reload caddy'
```

Check `grep` shows exactly the 4 edited lines inside the `ipfs.enclave.host` block (2× `reverse_proxy https://…`, 2×
`header_up Host …`) and no other vhost references 7ae476a3 (on 10-14 none did). Then:
`curl -s https://ipfs.enclave.host/healthz`, a range GET of a CID that Kubo lacks, and a real signed upload
(`enclave publish` of anything, or step 1a's script on a tiny component) → the new CID appears in adapter #2's
`/api/files` and in sia-s3.

Also on nan: `ADAPTER_DEP=` in `/opt/enclave-gateway/pin-adapter-wasm.sh` (line 43) → the new adapter id (it orders the
bootstrap pin first; same CID as before, so this is tidy-up, not urgent). The script is tracked in the enclave repo as
`scripts/nan-pin-adapter-wasm.sh`: change it there too. Update memory note enclave-ipfs-host-cutover.

Final delta: uploads that reached the OLD adapter between the manifest snapshot and the Caddy switch exist only in R2.
Right after the switch, regenerate the manifest from `https://7ae476a3.app.enclave.host/api/files`, re-run the import
(skips existing), `enclave restart <sia-s3>`, and wait for adapter #2's next refresh (300 s).

## 7. If B5 bites (adapter #2 cannot reach the sia-s3 label)

Option: give sia-s3 a nan-fronted name, e.g. `s3.sia.enclave.host` → Caddy `reverse_proxy https://<label>.app.enclave.host`
**without** `header_up Host` (Caddy passes the client's Host by default; SNI comes from the upstream address), and set
`S3_ENDPOINT=https://s3.sia.enclave.host`. The signature then covers `s3.sia.enclave.host`, which is what sia-s3 receives.
Needs a DNS record + Caddy block + `request_body` limit ≥ 9 MiB (adapter parts are 8 MiB). UNVERIFIED end to end.

## 8. Alternative with one slot and no Caddy change

Skip adapter #2: after steps 1-4, change 0x7ae476a3's secrets (`S3_ENDPOINT`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`)
and restart it. Owner is 0x0b2d, so this needs Trezor message signatures (`enclave-secrets:put:<id>:<expiry>:<sha256(payload)>`
via `trezorctl ethereum sign-message`, POST `/v1/secrets/<id>`, `cli/enclave.mjs:2900-2906`), costs the ~80-min re-hash
window (Kubo-first reads hide most of it), and rollback = put the R2 values back. Needs only sia-s3's slot.

## 9. Rollback

- Caddy: restore the `Caddyfile.pre-sia-cutover-*` backup, `caddy validate`, `systemctl reload caddy`. 0x7ae476a3 and R2
  are untouched by this runbook; keep both until adapter #2 has run clean for days. Uploads made after the cutover live
  only in sia-s3: before a rollback, copy them back (adapter #2 `/api/files` minus the old list).
- Deployments: `enclave stop <id>` (owner key) then `enclave refund`.

## Appendix A: answers with evidence

### A1. Publishing

- Pin = `POST https://ipfs.enclave.host/add-wasm` with `x-upload-{address,expiry,token}`; token from
  `POST https://api.enclave.host/v1/apps/upload-token {hash,expiry,signature}`, signature over
  `enclave-upload:<sha256hex>:<expiry>` (expiry ≤ 10 min) by **any** wallet (`api-relay.js:1873-1904`, `cli/enclave.mjs:2515-2534`).
- Catalog record = `publishVersion(slug,name,desc,version,cid,uint32[4] res,ports,config,feePerSec6)` (rev ≥ 5 overload,
  `cli/enclave.mjs:277-283,2741-2746`). appId = keccak(publisher, slug), so **publisher must be 0x0b2d** to keep the platform
  app under governance and to get rev-9 auto-approval (`EnclaveAppCatalog.sol:440,451`; CLI reads the minted
  `VersionApprovalSet`, `cli/enclave.mjs:2755-2770`). The CID must not be claimed by another live version (`_reserveCid`); it is new.
- Signing: `enclave publish --signer` needs Frame/Clef (not installed) and still fails at the pin (message signature through
  `externalAccount`, `cli/enclave.mjs:431-439`). Proven route = trezorctl offline-signed legacy tx, signed bytes checked,
  then broadcast (kit `~/enclave-prod/connectivity-activation/trezor-send.sh`, used for the 10-13 activation), or a
  localhost page with the browser wallet.
- Ports/config/fee/resources: all immutable per version and approval-covered (`EnclaveAppCatalog.sol:100-123`).

### A2. Deploying

- `enclave deploy <slug>:<version> --cpu F --fund USD [--secrets-file .env] [--isolation …] [--max-rate USD/h] [--no-wait]`
  (`cli/enclave.mjs:2013-2360`). Create → read-after-write → stage secrets → fund (EIP-3009) → claim-hint.
- Who pays: the deployment owner funds it in USDC; free when the claiming host's registry payout wallet is the owner
  (rev 12). Publisher fee here is 0.
- Which hosts: only nucbox-k11-4 today (B2 blocks it until a slot frees). metal0 reports `secrets: false` and no free CPU;
  the phone caps app memory at 512 MB. All app ingress/egress is the WireGuard Enclave network (`app.enclave.host`
  resolves to provider IPs, e.g. 7ae476a3 → 165.245.191.129, 146.190.246.39); the relay returns 410 for the old `/x/`
  app path (`main:relay/api-relay.js:2661-2663`).
- 8080: never declare it (section 1b).

### A3. Egress (NucBox Shield partition, production)

Path: app → `ENCLAVE_EGRESS` SOCKS front `127.0.0.2:1080` in the guest (public-web domains only) → vsock → `shielded-bridge`
→ host `shield-egress` (`egress-web-v1 <host|ip> <port>`) → the app's own Enclave-network circuit (`enclave-wg` SOCKS) →
guard → exit (DigitalOcean provider) → internet (`main:windows/vbslike/manager/egress.mjs:4-17`, `main:network/egress-map.mjs:14-16`).

Restrictions, all layers:
- Ports: any 1-65535 except 25 (`guest@d24c508ca:isolation/m2/egress/web.go:46`; `main:network/wg/policy.go:14-20`;
  provider nftables `main:network/wg-provider/kernel.mjs:34-40`). 9984/9964/5556 pass.
- Destinations: public addresses only; private, loopback, link-local, CGNAT, documentation (incl. 203.0.113/24),
  multicast refused (`wg/policy.go:42-66`, `egress/dialer.go RefuseAddr`). For a NAME, shield-egress resolves by DoH
  through the app's route and refuses the whole name if ANY answer is non-public (`app_routes.go:299-318`), so a Sia host
  whose DNS mixes public and private answers fails; literal IPs are judged alone (`web.go DialWeb`). One circuit attempt
  (resolve + dial every address) shares a 10 s budget before the next circuit is tried.
- **IPv6: none.** Providers drop all IPv6 and enclave-wg refuses v6 without a v6 route (`kernel.mjs:34-40`,
  `wg/policy.go`). IPv6-only Sia hosts are unreachable; the SDK must skip them (it picks from many hosts). Dual-stack names
  are fine (each address is tried, `wg/socks.go:215-245`).
- Caps per app: **96 concurrent connections, 2,400 new per minute** at shield-egress
  (`guest@d24c508ca:isolation/m2/cmd/shield-egress/main.go:138`); 1,024 sockets per circuit in enclave-wg. 30-50 SiaMux
  hosts fit, but upload (30 shards) + concurrent reads + compactor can approach 96: watch for `refused … concurrency`.
- UDP: not carried (TCP CONNECT only).
- Billing: default owner policy = free providers only, platform pays (`main:network/network-policy-core.mjs:13-16`).
  Each lone small PUT moves ~120 MiB through the providers.
- metal0 (for completeness): egress only when `EGRESS_SOCKS_UPSTREAM` is configured (`main:supervisor.js:3949-3966`), and
  egress.js dials only the first resolved address (`main:egress.js:69-72`); not a target here.

### A4. App-to-app and inbound headers

- Reaching `https://<sia-s3>.app.enclave.host` from adapter #2: allowed by policy (nan and provider IPs are public and not
  blocked), subject to B5. No loopback path between partitions exists or is needed.
- Inbound on the NucBox: provider SNI router (TLS untouched) → circuit → host app-ingress SNI gate → `ShieldIngress` raw
  pipe → manager data plane (`ENCLAVE-SPLICE/1` preamble, then ciphertext) → hv_sock → in-guest monitor → **front
  terminates TLS** (TLS 1.3, partition-held key) → Go `httputil.ReverseProxy` to the app
  (`main:network/privacy-agent.mjs:142-143,183`, `main:network/shield-ingress.mjs:32-36`,
  `main:windows/vbslike/datapath/datapath.mjs:95-166`).
  - `Authorization`: untouched; the front proxies every path except its own `/.well-known/enclave-*` endpoints and has no
    CORS/OPTIONS interception (`guest@c85e0c592:isolation/m2/front/main.go:324-346`). The "gateway strips Authorization"
    memory is about the retired supervisor `/x/` path (`main:supervisor.js:4443`), which production no longer routes to.
  - `Host`: preserved, `pr.Out.Host = pr.In.Host` (`front/ready.go:87`). sia-s3 has its own TCP listener (not wasi:http),
    so it sees the raw `host:` line that the adapter signed (adapter signs `host` without `:443`, `s3.rs:144-147`).
  - `x-amz-date`, `x-amz-content-sha256`, `x-amz-copy-source`, `range`: ordinary headers, passed. Removed: hop-by-hop
    (Connection, Keep-Alive, TE, Trailer, Transfer-Encoding, Upgrade, Proxy-*) and Forwarded/X-Forwarded-* (Rewrite mode).
    A chunked client body is re-chunked; sia-s3 refuses chunked bodies (`src/http.rs:69`), the adapter always sends
    Content-Length.
  - Body: no size limit or buffering on the path (Shield refuses WAF, so `maxBodyMb`/2 MiB gate never apply,
    `main:windows/vbslike/manager/server.mjs:64-65`); sia-s3's own cap is `maxBodyBytes` 64 MiB (adapter parts are 8 MiB).
  - Timeouts: front ReadHeaderTimeout 10 s, Idle 2 min (`front/main.go:221`), app must START answering within 180 s
    (`front/ready.go:73`), data plane cuts after 180 s with no bytes either way (`datapath.mjs:81` idleMs 180_000). The adapter's
    own per-request deadline is 90 s (`s3.rs REQUEST_DEADLINE`): a sia-s3 PUT that takes > 90 s (lone PUT ~45 s today) fails
    on the adapter side first. UNVERIFIED under load.
  - Verdict: a SigV4 request from adapter #2 arrives at sia-s3 intact. UNVERIFIED live until step 3/5 run.
- Caveat on provenance: the guest front/egress code above is the source of the pinned images (`runtime.bin` 5fbfb947
  "public-web-20261005", `shield-egress.exe` ab08c73d from `isolation/egress-torn-reread` d24c508ca), not byte-proven by me;
  memory notes the live front was built from 28d642888 + an uncommitted diff with byte-identical code sections.

### A5. Cutover

Section 6. `UPLOAD_KEY` = the `UPLOAD_KEY=` value in `nan:/etc/nan-relay/api-relay.env`, which must match the running
`enclave-api-relay` process (cross-check command in section 5); the adapter verifies the relay-minted
`x-upload-token = HMAC(UPLOAD_KEY, "<addr>:<sha256>:<expiry>")` (`api-relay.js:1903`). Secrets bind at partition
creation, so a secrets change needs `enclave restart` (or `secrets set … --restart`).

## Trezor touches (Steven)

1. Step 1b `publishVersion` (one confirmation). Required.
2. Only if 0x0b2d owns the new deployments (B3 alternative): 2 `create` txs + 2 secrets messages; or section 8's
   secrets message for 0x7ae476a3.

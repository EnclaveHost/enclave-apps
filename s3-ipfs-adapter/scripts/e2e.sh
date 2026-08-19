#!/usr/bin/env bash
# End-to-end verification of s3-ipfs-adapter against minio + kubo.
#
# Proves the adapter's whole claim: the CIDs it mints for bucket objects are
# byte-identical to `ipfs add --cid-version 1` (root CID equality is a merkle
# proof for every block underneath), and the gateway serves bytes, ranges,
# raw blocks and CARs that verify. Needs: minio, wasmtime, ipfs (kubo),
# curl, python3, all on PATH. Everything runs on loopback; no network.
set -euo pipefail
cd "$(dirname "$0")/.."

MINIO_PORT=19000
APP_PORT=18400
BASE="http://127.0.0.1:$APP_PORT"
S3="http://127.0.0.1:$MINIO_PORT"
BUCKET=testbkt
AK=testkey
SK=testsecret12345

WORK=$(mktemp -d "${TMPDIR:-/tmp}/s3ipfs-e2e.XXXXXX")
PIDS=()
OK=0
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  if [ "$OK" = 1 ]; then
    rm -rf "$WORK"
  else
    # keep the evidence: app logs + fixtures survive a failed run
    echo "FAILED - work dir kept at $WORK" >&2
    tail -5 "$WORK"/app*.log 2>/dev/null >&2 || true
  fi
}
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== build =="
cargo build --release --target wasm32-wasip2 2>&1 | tail -1

echo "== minio =="
MINIO_ROOT_USER=$AK MINIO_ROOT_PASSWORD=$SK \
  minio server --address "127.0.0.1:$MINIO_PORT" "$WORK/minio" >"$WORK/minio.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 50); do
  curl -sf "$S3/minio/health/ready" >/dev/null && break
  sleep 0.2
  [ "$i" = 50 ] && fail "minio did not come up"
done

echo "== dataset =="
D="$WORK/dataset"
mkdir -p "$D/docs/img" "$D/sp ace"
printf 'hello world' > "$D/hello.txt"
: > "$D/empty.bin"
head -c $((6 * 1024 * 1024 + 123)) /dev/urandom > "$D/big.bin"
head -c $((50 * 1024 * 1024)) /dev/urandom > "$D/huge.bin"   # two-level DAG (>174 chunks)
printf '# readme\n' > "$D/docs/readme.md"
head -c 20000 /dev/urandom > "$D/docs/img/logo.png"
printf 'tricky name content\n' > "$D/sp ace/plus+file ü.txt"

s3put() { # key, file
  local enc
  enc=$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1],safe="/"))' "$1")
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    --aws-sigv4 "aws:amz:us-east-1:s3" --user "$AK:$SK" -T "$2" "$S3/$BUCKET/$enc")
  [ "$code" = 200 ] || fail "PUT $1 -> $code"
}
code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
  --aws-sigv4 "aws:amz:us-east-1:s3" --user "$AK:$SK" "$S3/$BUCKET/")
[ "$code" = 200 ] || fail "create bucket -> $code"
(cd "$D" && find . -type f | sed 's|^\./||') | while read -r f; do
  s3put "$f" "$D/$f"
done
pass "seeded $(find "$D" -type f | wc -l) objects"

echo "== kubo reference =="
export IPFS_PATH="$WORK/ipfs"
ipfs init -e >/dev/null 2>&1
ipfs add -r --cid-version 1 "$D" >"$WORK/kubo-add.txt" 2>/dev/null
KUBO_ROOT=$(python3 - "$WORK/kubo-add.txt" <<'EOF'
import sys
for line in open(sys.argv[1]):
    parts = line.rstrip("\n").split(" ", 2)
    if len(parts) == 3 and parts[2] == "dataset":
        print(parts[1])
EOF
)
[ -n "$KUBO_ROOT" ] || fail "no kubo root"
echo "kubo root: $KUBO_ROOT"

echo "== app =="
CONFIG=$(python3 - <<EOF
import json
print(json.dumps({
  "endpoint": "$S3", "region": "us-east-1", "bucket": "$BUCKET",
  "credentials": {"accessKeyId": "$AK", "secretAccessKey": "$SK"},
  "refreshSecs": 0,
}))
EOF
)
wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env "ENCLAVE_PORTS=http:8000=$APP_PORT" --env "ENCLAVE_CONFIG=$CONFIG" \
  target/wasm32-wasip2/release/s3-ipfs-adapter.wasm >"$WORK/app.log" 2>&1 &
PIDS+=($!)

for i in $(seq 1 60); do
  curl -sf "$BASE/ping" >/dev/null && break
  sleep 0.5
  [ "$i" = 60 ] && { cat "$WORK/app.log"; fail "app did not come up"; }
done
pass "app is up (and serving while indexing)"
curl -sf "$BASE/" | grep -q 's3-ipfs-adapter' || fail "UI did not render"

status() { curl -sf "$BASE/api/status"; }
jget() { python3 -c 'import sys,json;v=json.load(sys.stdin).get(sys.argv[1]);print("" if v is None else v)' "$1"; }
for i in $(seq 1 600); do
  [ "$(status | jget state)" = ready ] && break
  sleep 0.5
  [ "$i" = 600 ] && { cat "$WORK/app.log"; fail "index never became ready"; }
done
APP_ROOT=$(status | jget rootCid)
pass "index ready, root: $APP_ROOT"

echo "== CID equality against kubo =="
[ "$APP_ROOT" = "$KUBO_ROOT" ] || {
  echo "--- kubo per-file:"; cat "$WORK/kubo-add.txt"
  echo "--- app per-file:"; curl -s "$BASE/api/files"
  fail "root CID mismatch: app=$APP_ROOT kubo=$KUBO_ROOT"
}
pass "root directory CID identical to ipfs add (merkle-proves every block)"

curl -s "$BASE/api/files" >"$WORK/app-files.json"
python3 - "$WORK/kubo-add.txt" "$WORK/app-files.json" <<'EOF' || exit 1
import sys, json
kubo = {}
for line in open(sys.argv[1]):
    parts = line.rstrip("\n").split(" ", 2)
    if len(parts) == 3 and parts[2].startswith("dataset/"):
        kubo[parts[2][len("dataset/"):]] = parts[1]
app = {f["path"]: f["cid"] for f in json.load(open(sys.argv[2]))}
kubo_files = {k: v for k, v in kubo.items() if k in app}
assert set(app) == set(kubo_files), (set(app) ^ set(kubo_files))
for path, cid in app.items():
    assert kubo_files[path] == cid, f"{path}: app={cid} kubo={kubo_files[path]}"
print(f"PASS: all {len(app)} per-file CIDs identical to ipfs add")
EOF

echo "== gateway bytes =="
curl -sf "$BASE/ipfs/$APP_ROOT/big.bin" -o "$WORK/got-big.bin"
cmp "$WORK/got-big.bin" "$D/big.bin" || fail "big.bin bytes differ"
pass "whole-file fetch matches (6 MiB, multi-chunk)"

curl -sf "$BASE/ipfs/$APP_ROOT/huge.bin" -o "$WORK/got-huge.bin"
cmp "$WORK/got-huge.bin" "$D/huge.bin" || fail "huge.bin bytes differ"
pass "whole-file fetch matches (50 MiB, two-level DAG)"

curl -sf -r 1000000-3000000 "$BASE/ipfs/$APP_ROOT/huge.bin" -o "$WORK/got-range.bin"
python3 -c 'import sys;d=open(sys.argv[1],"rb").read();sys.stdout.buffer.write(d[1000000:3000001])' "$D/huge.bin" >"$WORK/want-range.bin"
cmp "$WORK/got-range.bin" "$WORK/want-range.bin" || fail "range bytes differ"
pass "HTTP range (mid-chunk boundaries) matches"

curl -sf -r "-1234" "$BASE/ipfs/$APP_ROOT/big.bin" -o "$WORK/got-suffix.bin"
tail -c 1234 "$D/big.bin" >"$WORK/want-suffix.bin"
cmp "$WORK/got-suffix.bin" "$WORK/want-suffix.bin" || fail "suffix range differs"
pass "suffix range matches"

HELLO_CID=$(python3 -c 'import sys,json;print({f["path"]:f["cid"] for f in json.load(open(sys.argv[1]))}["hello.txt"])' "$WORK/app-files.json")
[ "$(curl -sf "$BASE/ipfs/$HELLO_CID")" = "hello world" ] || fail "hello by direct CID"
pass "small file by direct (raw) CID"

EMPTY_CID=$(python3 -c 'import sys,json;print({f["path"]:f["cid"] for f in json.load(open(sys.argv[1]))}["empty.bin"])' "$WORK/app-files.json")
CL=$(curl -sfI "$BASE/ipfs/$EMPTY_CID" | tr -d '\r' | awk 'tolower($1)=="content-length:"{print $2}')
[ "$CL" = 0 ] || fail "empty file content-length: '$CL'"
pass "empty file (content-length 0)"

CL=$(curl -sfI "$BASE/ipfs/$APP_ROOT/huge.bin" | tr -d '\r' | awk 'tolower($1)=="content-length:"{print $2}')
[ "$CL" = "$(stat -c%s "$D/huge.bin")" ] || fail "HEAD content-length: '$CL'"
pass "HEAD reports true content-length"

curl -sf "$BASE/ipfs/$APP_ROOT/sp%20ace/plus%2Bfile%20%C3%BC.txt" -o "$WORK/got-uni.txt"
cmp "$WORK/got-uni.txt" "$D/sp ace/plus+file ü.txt" || fail "unicode path bytes differ"
pass "space/plus/unicode path"

curl -sf "$BASE/ipfs/$APP_ROOT/docs/" | grep -q 'readme.md' || fail "dir listing"
pass "directory listing HTML"

echo "== trustless gateway =="
HUGE_CID=$(python3 -c 'import sys,json;print({f["path"]:f["cid"] for f in json.load(open(sys.argv[1]))}["huge.bin"])' "$WORK/app-files.json")
curl -sf "$BASE/ipfs/$HUGE_CID?format=raw" -o "$WORK/got-block.bin"
ipfs block get "$HUGE_CID" >"$WORK/want-block.bin" 2>/dev/null
cmp "$WORK/got-block.bin" "$WORK/want-block.bin" || fail "dag-pb root block differs from kubo's"
pass "raw dag-pb block byte-identical to kubo's"

curl -sf "$BASE/ipfs/$APP_ROOT?format=car" -o "$WORK/root.car"
export IPFS_PATH="$WORK/ipfs2"
ipfs init -e >/dev/null 2>&1
ipfs dag import "$WORK/root.car" >/dev/null 2>&1 || fail "kubo rejected the CAR"
ipfs cat "$APP_ROOT/docs/img/logo.png" >"$WORK/car-logo.png" 2>/dev/null
cmp "$WORK/car-logo.png" "$D/docs/img/logo.png" || fail "bytes out of imported CAR differ"
ipfs cat "$APP_ROOT/huge.bin" 2>/dev/null | cmp - "$D/huge.bin" || fail "huge.bin out of imported CAR differs"
pass "CAR imports into kubo and round-trips bytes (hashes verified by kubo)"

echo "== refresh on change =="
printf 'hello world v2' > "$WORK/hello2.txt"
s3put "hello.txt" "$WORK/hello2.txt"
NEW_EXPECT=$(ipfs add --cid-version 1 -Q --only-hash "$WORK/hello2.txt" 2>/dev/null)
curl -sf -X POST "$BASE/api/refresh" >/dev/null
for i in $(seq 1 120); do
  NOW=$(curl -s "$BASE/api/files" | python3 -c 'import sys,json;print({f["path"]:f["cid"] for f in json.load(sys.stdin)}.get("hello.txt",""))' 2>/dev/null || echo "")
  [ "$NOW" = "$NEW_EXPECT" ] && break
  sleep 0.5
  [ "$i" = 120 ] && fail "refresh never picked up the new object (have '$NOW', want $NEW_EXPECT)"
done
[ "$(curl -sf "$BASE/ipfs/$NEW_EXPECT")" = "hello world v2" ] || fail "new content by new CID"
NEW_ROOT=$(status | jget rootCid)
[ "$NEW_ROOT" != "$APP_ROOT" ] || fail "root CID did not change on refresh"
pass "refresh re-indexed the changed object (new root: $NEW_ROOT)"

echo "== upload / delete through the app =="
head -c $((1 * 1024 * 1024 + 77)) /dev/urandom > "$WORK/upload.bin"
UP_EXPECT=$(ipfs add --cid-version 1 -Q --only-hash "$WORK/upload.bin" 2>/dev/null)
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WORK/upload.bin" \
  "$BASE/api/upload?path=incoming/upload%20me.bin")
[ "$code" = 200 ] || fail "upload -> $code"
for i in $(seq 1 120); do
  NOW=$(curl -s "$BASE/api/files" | python3 -c 'import sys,json;print({f["path"]:f["cid"] for f in json.load(sys.stdin)}.get("incoming/upload me.bin",""))' 2>/dev/null || echo "")
  [ "$NOW" = "$UP_EXPECT" ] && break
  sleep 0.5
  [ "$i" = 120 ] && fail "uploaded file never appeared with the right CID (have '$NOW', want $UP_EXPECT)"
done
curl -sf "$BASE/ipfs/$UP_EXPECT" -o "$WORK/upload-back.bin"
cmp "$WORK/upload-back.bin" "$WORK/upload.bin" || fail "uploaded bytes differ on the way back"
pass "upload lands in S3, indexes with the kubo-identical CID, round-trips"

code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data 'path=incoming%2Fupload%20me.bin' "$BASE/api/delete")
[ "$code" = 200 ] || fail "delete -> $code"
for i in $(seq 1 120); do
  GONE=$(curl -s "$BASE/api/files" | python3 -c 'import sys,json;print("incoming/upload me.bin" in {f["path"] for f in json.load(sys.stdin)})' 2>/dev/null || echo "")
  [ "$GONE" = False ] && break
  sleep 0.5
  [ "$i" = 120 ] && fail "deleted file still in the index"
done
code=$(curl -s -o /dev/null -w '%{http_code}' "$S3/$BUCKET/incoming/upload%20me.bin" \
  --aws-sigv4 "aws:amz:us-east-1:s3" --user "$AK:$SK")
[ "$code" = 404 ] || fail "object still in S3 after delete ($code)"
pass "delete removes the object from S3 and the index"

code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/upload?path=../escape.bin" --data-binary 'x')
[ "$code" = 400 ] || fail "dot-dot key accepted ($code)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/upload?path=trailing/")
[ "$code" = 400 ] || fail "trailing-slash key accepted ($code)"
pass "bad upload keys rejected"

echo "== pin routes (/add-wasm /add-json /add-image) =="
# A second instance with the pin routes on, wallet-signed with a test key -
# the same HMAC the api-relay mints. Same bucket; pins land under pins/<cid>.
APP2_PORT=18401
BASE2="http://127.0.0.1:$APP2_PORT"
UPKEY=test-upload-key
ADDR=0x00000000000000000000000000000000000000aa
CONFIG2=$(python3 - <<EOF
import json
print(json.dumps({
  "endpoint": "$S3", "region": "us-east-1", "bucket": "$BUCKET",
  "credentials": {"accessKeyId": "$AK", "secretAccessKey": "$SK"},
  "refreshSecs": 0,
  "upload": {"uploadKey": "$UPKEY", "allowOrigins": ["https://enclave.host"]},
}))
EOF
)
wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env "ENCLAVE_PORTS=http:8000=$APP2_PORT" --env "ENCLAVE_CONFIG=$CONFIG2" \
  target/wasm32-wasip2/release/s3-ipfs-adapter.wasm >"$WORK/app2.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 60); do
  curl -sf "$BASE2/healthz" | grep -q '"ok":true' && break
  sleep 0.5
  [ "$i" = 60 ] && { cat "$WORK/app2.log"; fail "pin app did not come up"; }
done
pass "/healthz answers (the gateway liveness contract)"
for i in $(seq 1 600); do
  [ "$(curl -sf "$BASE2/api/status" | jget state)" = ready ] && break
  sleep 0.5
  [ "$i" = 600 ] && { cat "$WORK/app2.log"; fail "pin app index never became ready"; }
done

mint() { # file, expiry -> token (the api-relay's exact HMAC)
  python3 - "$UPKEY" "$1" "$ADDR" "$2" <<'EOF'
import hashlib, hmac, sys
key, path, addr, exp = sys.argv[1:5]
digest = hashlib.sha256(open(path, "rb").read()).hexdigest()
print(hmac.new(key.encode(), f"{addr}:{digest}:{exp}".encode(), hashlib.sha256).hexdigest())
EOF
}
EXP=$(( $(date +%s) + 300 ))

WASM=target/wasm32-wasip2/release/s3-ipfs-adapter.wasm
WASM_EXPECT=$(ipfs add --cid-version 1 -Q --only-hash "$WASM" 2>/dev/null)

code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WASM" "$BASE2/add-wasm")
[ "$code" = 401 ] || fail "unsigned /add-wasm -> $code (want 401)"
pass "unsigned upload refused (401)"

TOK=$(mint "$WASM" "$EXP")
curl -s -X POST --data-binary "@$WASM" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK" \
  -H "origin: https://enclave.host" \
  "$BASE2/add-wasm" >"$WORK/addwasm.json"
CID=$(jget cid <"$WORK/addwasm.json")
[ "$CID" = "$WASM_EXPECT" ] || { cat "$WORK/addwasm.json"; fail "/add-wasm CID $CID != ipfs add $WASM_EXPECT"; }
WASI=$(jget wasi <"$WORK/addwasm.json")
[ "$WASI" = "0.2" ] || fail "/add-wasm classified wasi='$WASI' (want 0.2)"
pass "/add-wasm pins with the kubo-identical CID and classifies the world"

# The freshly pinned CID must serve IMMEDIATELY (no refresh in between):
# a publish is followed by a deploy within seconds.
curl -sf "$BASE2/ipfs/$CID" -o "$WORK/wasm-back.bin"
cmp "$WORK/wasm-back.bin" "$WASM" || fail "pinned wasm bytes differ on the way back"
pass "pinned CID serves immediately (incremental index commit)"

# The fleet's exact fetch shape: CAR + verify by import.
curl -sf -H 'Accept: application/vnd.ipld.car' "$BASE2/ipfs/$CID?format=car&dag-scope=all" -o "$WORK/wasm.car"
export IPFS_PATH="$WORK/ipfs3"
ipfs init -e >/dev/null 2>&1
ipfs dag import "$WORK/wasm.car" >/dev/null 2>&1 || fail "kubo rejected the /add-wasm CAR"
ipfs cat "$CID" 2>/dev/null | cmp - "$WASM" || fail "CAR round-trip bytes differ"
pass "wasm CAR (?format=car&dag-scope=all) verifies block-for-block in kubo"

TOK_OTHER=$(mint "$D/hello.txt" "$EXP")
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WASM" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_OTHER" \
  "$BASE2/add-wasm")
[ "$code" = 403 ] || fail "wrong-bytes token -> $code (want 403)"
EXP_OLD=$(( $(date +%s) - 10 ))
TOK_OLD=$(mint "$WASM" "$EXP_OLD")
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WASM" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP_OLD" -H "x-upload-token: $TOK_OLD" \
  "$BASE2/add-wasm")
[ "$code" = 401 ] || fail "expired token -> $code (want 401)"
pass "token binds to the bytes (403) and to time (401)"

# The multipart path: a >8 MiB body (real component followed by padding -
# layer 1 accepts, so only the CID math and the S3 multipart plumbing are
# under test). CID must still be kubo-identical.
cat "$WASM" >"$WORK/big.wasm"
head -c $((20 * 1024 * 1024)) /dev/urandom >>"$WORK/big.wasm"
BIG_EXPECT=$(ipfs add --cid-version 1 -Q --only-hash "$WORK/big.wasm" 2>/dev/null)
TOK_BIG=$(mint "$WORK/big.wasm" "$EXP")
BIG_CID=$(curl -s -X POST --data-binary "@$WORK/big.wasm" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_BIG" \
  "$BASE2/add-wasm" | jget cid)
[ "$BIG_CID" = "$BIG_EXPECT" ] || fail "multipart CID $BIG_CID != ipfs add $BIG_EXPECT"
curl -sf "$BASE2/ipfs/$BIG_CID" -o "$WORK/big-back.bin"
cmp "$WORK/big-back.bin" "$WORK/big.wasm" || fail "multipart bytes differ on the way back"
code=$(curl -s -o /dev/null -w '%{http_code}' "$S3/$BUCKET/staging/" \
  --aws-sigv4 "aws:amz:us-east-1:s3" --user "$AK:$SK")
pass "20 MiB body streams through S3 multipart, CID kubo-identical"

printf 'not wasm at all' >"$WORK/noise.bin"
TOK_NOISE=$(mint "$WORK/noise.bin" "$EXP")
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WORK/noise.bin" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_NOISE" \
  "$BASE2/add-wasm")
[ "$code" = 415 ] || fail "non-wasm -> $code (want 415)"
pass "not-a-component refused (415)"

printf '{"model":"x","ctx":4096}' >"$WORK/cfg.json"
JSON_EXPECT=$(ipfs add --cid-version 1 -Q --only-hash "$WORK/cfg.json" 2>/dev/null)
TOK_JSON=$(mint "$WORK/cfg.json" "$EXP")
J_CID=$(curl -s -X POST --data-binary "@$WORK/cfg.json" -H 'content-type: application/json' \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_JSON" \
  "$BASE2/add-json" | jget cid)
[ "$J_CID" = "$JSON_EXPECT" ] || fail "/add-json CID $J_CID != $JSON_EXPECT"
[ "$(curl -sf "$BASE2/ipfs/$J_CID")" = '{"model":"x","ctx":4096}' ] || fail "config bytes differ"
printf '[1,2,3]' >"$WORK/arr.json"
TOK_ARR=$(mint "$WORK/arr.json" "$EXP")
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WORK/arr.json" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_ARR" \
  "$BASE2/add-json")
[ "$code" = 415 ] || fail "non-object config -> $code (want 415)"
pass "/add-json pins objects, refuses non-objects"

printf '<svg xmlns="http://www.w3.org/2000/svg"><rect width="4" height="4" fill="#e8a34c"/></svg>' >"$WORK/ok.svg"
TOK_SVG=$(mint "$WORK/ok.svg" "$EXP")
curl -s -X POST --data-binary "@$WORK/ok.svg" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_SVG" \
  "$BASE2/add-image" >"$WORK/addsvg.json"
[ "$(jget svg <"$WORK/addsvg.json")" = "True" ] || { cat "$WORK/addsvg.json"; fail "clean SVG not accepted as svg"; }
SVG_CID=$(jget cid <"$WORK/addsvg.json")
CT=$(curl -sfI "$BASE2/ipfs/$SVG_CID?filename=i.svg" | tr -d '\r' | awk 'tolower($1)=="content-type:"{print $2}')
[ "$CT" = "image/svg+xml" ] || fail "?filename=i.svg content-type '$CT'"
CSP=$(curl -sfI "$BASE2/ipfs/$SVG_CID" | tr -d '\r' | grep -i '^content-security-policy:' || true)
echo "$CSP" | grep -q sandbox || fail "gateway response missing CSP sandbox"
pass "/add-image accepts a clean SVG; gateway serves it typed + sandboxed"

printf '<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"/>' >"$WORK/evil.svg"
TOK_EVIL=$(mint "$WORK/evil.svg" "$EXP")
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WORK/evil.svg" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_EVIL" \
  "$BASE2/add-image")
[ "$code" = 415 ] || fail "hostile SVG -> $code (want 415)"
printf '\x89PNG\r\n\x1a\n0000IHDR' >"$WORK/ok.png"
TOK_PNG=$(mint "$WORK/ok.png" "$EXP")
[ "$(curl -s -X POST --data-binary "@$WORK/ok.png" \
  -H "x-upload-address: $ADDR" -H "x-upload-expiry: $EXP" -H "x-upload-token: $TOK_PNG" \
  "$BASE2/add-image" | jget svg)" = "False" ] || fail "raster PNG not accepted"
pass "hostile SVG refused (415), raster accepted"

ACAO=$(curl -s -o /dev/null -D - -X OPTIONS -H 'origin: https://enclave.host' \
  -H 'access-control-request-method: POST' "$BASE2/add-wasm" | tr -d '\r' \
  | awk 'tolower($1)=="access-control-allow-origin:"{print $2}')
[ "$ACAO" = "https://enclave.host" ] || fail "preflight allow-origin '$ACAO'"
pass "CORS preflight echoes the allowed origin"

# An over-cap /add-json (2 MB vs the 1 MiB ceiling) must be refused from the
# Content-Length alone, before the body is buffered - the parser enforces the
# per-route cap, so an unauthenticated client can't force 32 MiB of buffering
# on a 1 MiB route (guest-OOM regression).
head -c $((2 * 1024 * 1024)) /dev/urandom > "$WORK/toobig.json"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$WORK/toobig.json" \
  -H 'content-type: application/json' "$BASE2/add-json")
[ "$code" = 413 ] || fail "over-cap /add-json -> $code (want 413)"
pass "per-route body cap enforced pre-buffer (413)"

# Two conflicting Content-Length headers -> 400 (request-smuggling desync).
code=$(printf 'POST /add-json HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntest' \
  | { exec 3<>/dev/tcp/127.0.0.1/$APP2_PORT; cat >&3; head -c 20 <&3; } | grep -o '40[0-9]' | head -1)
[ "$code" = 400 ] || fail "duplicate Content-Length -> '$code' (want 400)"
pass "duplicate Content-Length refused (400)"

echo
OK=1
echo "ALL PASS"

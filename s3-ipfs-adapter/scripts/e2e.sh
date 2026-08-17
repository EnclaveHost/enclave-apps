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
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
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

echo
echo "ALL PASS"

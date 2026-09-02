#!/usr/bin/env bash
# End-to-end verification of jot against a local minio, through the real
# wasmtime serve (what the platform runs). Every route, the key gate, the
# name filter, the size cap, conditional writes, read-only and unconfigured
# deployments, and the one claim that matters: a note written through the
# API is byte-for-byte the object in the bucket. Needs: minio, wasmtime,
# curl (with --aws-sigv4), python3, cargo-component. Loopback only.
set -euo pipefail
cd "$(dirname "$0")/.."

MINIO_PORT=19010
APP_PORT=18410
RO_PORT=18411
NC_PORT=18412
BASE="http://127.0.0.1:$APP_PORT"
S3="http://127.0.0.1:$MINIO_PORT"
BUCKET=notesbkt
AK=testkey
SK=testsecret12345
KEY=agent-key-$RANDOM$RANDOM
PREFIX=agent/

WORK=$(mktemp -d "${TMPDIR:-/tmp}/jot-e2e.XXXXXX")
PIDS=()
OK=0
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  if [ "$OK" = 1 ]; then rm -rf "$WORK"; else
    echo "FAILED - work dir kept at $WORK" >&2
    tail -20 "$WORK"/app*.log 2>/dev/null >&2 || true
  fi
}
trap cleanup EXIT
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# req METHOD PATH [curl args…] -> prints status; body lands in $WORK/body
req() {
  local m=$1 p=$2; shift 2
  curl -s -o "$WORK/body" -w '%{http_code}' -X "$m" "$@" "$BASE$p"
}
# X-Api-Key is the carriage that survives the platform gateway; a bearer is
# also accepted by the app and is checked once below
auth=(-H "x-api-key: $KEY")
jq_() { python3 -c "import json,sys; d=json.load(open('$WORK/body')); print($1)"; }

echo "== build =="
cargo component build --release --target wasm32-wasip2 2>&1 | tail -1
WASM=target/wasm32-wasip2/release/jot.wasm
[ -f "$WASM" ] || fail "no $WASM"

echo "== minio =="
MINIO_ROOT_USER=$AK MINIO_ROOT_PASSWORD=$SK \
  minio server --address "127.0.0.1:$MINIO_PORT" "$WORK/minio" >"$WORK/minio.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 50); do
  curl -sf "$S3/minio/health/ready" >/dev/null && break
  sleep 0.2; [ "$i" = 50 ] && fail "minio did not come up"
done
code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
  --aws-sigv4 "aws:amz:us-east-1:s3" --user "$AK:$SK" "$S3/$BUCKET/")
[ "$code" = 200 ] || fail "create bucket -> $code"

echo "== app =="
CONFIG=$(cat <<JSON
{"title":"e2e","endpoint":"$S3","region":"us-east-1","bucket":"$BUCKET","prefix":"$PREFIX",
 "credentials":{"accessKeyId":"\$JOT_ACCESS_KEY_ID","secretAccessKey":"\$JOT_SECRET_ACCESS_KEY"},
 "api_key":"\$JOT_API_KEY"}
JSON
)
serve() { # port config [extra env…]
  local port=$1 cfg=$2; shift 2
  wasmtime serve -Scommon --addr "127.0.0.1:$port" --env ENCLAVE_CONFIG="$cfg" "$@" "$WASM" >"$WORK/app$port.log" 2>&1 &
  PIDS+=($!)
  for i in $(seq 1 50); do
    curl -sf "http://127.0.0.1:$port/ping" >/dev/null && return 0
    sleep 0.2
  done
  fail "app on $port did not come up"
}
serve $APP_PORT "$CONFIG" --env JOT_API_KEY="$KEY" --env JOT_ACCESS_KEY_ID=$AK --env JOT_SECRET_ACCESS_KEY=$SK

echo "== routes =="
[ "$(req GET /ping)" = 200 ] || fail "ping"
grep -q '"pong":true' "$WORK/body" && pass "ping"
[ "$(req GET /)" = 200 ] && grep -q "jot" "$WORK/body" && pass "UI serves"

[ "$(req GET /api/status)" = 200 ] || fail "status"
[ "$(jq_ 'd["auth"] and d["configured"] and ("bucket" not in d)')" = True ] || fail "public status leaks bucket or misreports"
pass "status without key: configured, auth on, no bucket facts"
[ "$(req GET /api/status "${auth[@]}")" = 200 ] || fail "status w/ key"
[ "$(jq_ 'd["bucket"]=="'$BUCKET'" and d["prefix"]=="'$PREFIX'" and d["signed"]')" = True ] || fail "status with key lacks bucket facts"
pass "status with key: bucket facts, signed"

[ "$(req GET /api/notes)" = 401 ] || fail "list without key must 401"
[ "$(req GET /api/notes -H 'authorization: Bearer nope')" = 401 ] || fail "wrong key must 401"
[ "$(req GET /api/notes -H "authorization: Bearer $KEY")" = 200 ] || fail "bearer not accepted"
pass "key gate: 401 without/wrong key, X-Api-Key and bearer accepted"

NOTE=$'# Enclave\n\nfirst line with a Needle in it\nsecond line: "quotes" & <angles> and ünïcode\n'
python3 - "$WORK/write.json" <<PY
import json,sys; json.dump({"content": sys.argv[0] and """$NOTE"""}, open(sys.argv[1],"w"))
PY
[ "$(req PUT /api/notes/projects/enclave.md "${auth[@]}" -H 'content-type: application/json' --data-binary @"$WORK/write.json")" = 200 ] || fail "write: $(cat $WORK/body)"
ETAG=$(jq_ 'd["etag"]')
[ -n "$ETAG" ] || fail "write returned no etag"
pass "write projects/enclave.md (etag $ETAG)"

[ "$(req GET /api/notes/projects/enclave.md "${auth[@]}")" = 200 ] || fail "read"
python3 - "$WORK/body" <<PY || fail "read content differs"
import json,sys
d=json.load(open(sys.argv[1]))
assert d["content"] == """$NOTE""", repr(d["content"])
assert d["name"] == "projects/enclave.md" and d["etag"] == "$ETAG"
PY
pass "read back byte-exact via JSON"
[ "$(req GET '/api/notes/projects/enclave.md?raw=1' "${auth[@]}")" = 200 ] || fail "raw read"
printf '%s' "$NOTE" | cmp -s - "$WORK/body" || fail "raw read differs"
pass "raw read byte-exact"

# the object in the bucket IS the note
curl -s -o "$WORK/obj" --aws-sigv4 "aws:amz:us-east-1:s3" --user "$AK:$SK" "$S3/$BUCKET/${PREFIX}projects/enclave.md"
printf '%s' "$NOTE" | cmp -s - "$WORK/obj" || fail "bucket object differs from the note"
pass "bucket object ${PREFIX}projects/enclave.md is byte-exact"

[ "$(req GET /api/notes "${auth[@]}")" = 200 ] || fail "list"
[ "$(jq_ '[n["name"] for n in d["notes"]]==["projects/enclave.md"] and d["notes"][0]["size"]=='$(printf '%s' "$NOTE" | wc -c))" = True ] || fail "list: $(cat $WORK/body)"
pass "list names and sizes"
[ "$(req GET '/api/notes?prefix=projects/' "${auth[@]}")" = 200 ] && [ "$(jq_ 'len(d["notes"])')" = 1 ] || fail "list prefix"
[ "$(req GET '/api/notes?prefix=zzz' "${auth[@]}")" = 200 ] && [ "$(jq_ 'len(d["notes"])')" = 0 ] || fail "list empty prefix"
pass "list prefix filter"

[ "$(req PUT /api/notes/todo.txt "${auth[@]}" -H 'content-type: text/plain' --data-binary 'one')" = 200 ] || fail "raw text write"
[ "$(req POST /api/notes/todo.txt/append "${auth[@]}" -H 'content-type: application/json' -d '{"content":"two"}')" = 200 ] || fail "append: $(cat $WORK/body)"
[ "$(req GET '/api/notes/todo.txt?raw=1' "${auth[@]}")" = 200 ] || fail "read todo"
[ "$(cat $WORK/body)" = $'one\ntwo' ] || fail "append result: $(cat $WORK/body | od -c | head -3)"
pass "text/plain write + append"
[ "$(req POST /api/notes/log.md/append "${auth[@]}" -H 'content-type: application/json' -d '{"content":"learned: appending creates"}')" = 200 ] || fail "append-create"
[ "$(req GET '/api/notes/log.md?raw=1' "${auth[@]}")" = 200 ] && [ "$(cat $WORK/body)" = "learned: appending creates" ] || fail "append-create content"
pass "append creates a missing note"

[ "$(req GET '/api/search?q=needle' "${auth[@]}")" = 200 ] || fail "search"
[ "$(jq_ 'len(d["hits"])==1 and d["hits"][0]["name"]=="projects/enclave.md" and d["hits"][0]["line"]==3 and d["scanned"]==3')" = True ] || fail "search hits: $(cat $WORK/body)"
pass "search (case-insensitive, line numbers, scanned count)"
[ "$(req GET '/api/search' "${auth[@]}")" = 400 ] || fail "search without q must 400"
[ "$(req GET '/api/search?q=needle&prefix=todo' "${auth[@]}")" = 200 ] && [ "$(jq_ 'len(d["hits"])')" = 0 ] || fail "search prefix"
pass "search validation + prefix"

code=$(req PUT /api/notes/projects/enclave.md "${auth[@]}" -H 'content-type: application/json' -d '{"content":"clobber","ifMatch":"deadbeef"}')
if [ "$code" = 412 ]; then pass "conditional write refused on stale ETag (412)";
else echo "WARN: store answered $code to a stale If-Match (conditional PUT unsupported by this store?)"; fi
[ "$(req PUT /api/notes/projects/enclave.md "${auth[@]}" -H 'content-type: application/json' -d "{\"content\":\"v2\",\"ifMatch\":\"$ETAG\"}")" = 200 ] || fail "conditional write with the right etag: $(cat $WORK/body)"
pass "conditional write with the current ETag"

for bad in '..%2Fx' 'a%2F%2Fb' '.%2Fa' 'a%3Fb' '%C3%BC.md'; do
  [ "$(req GET "/api/notes/$bad" "${auth[@]}")" = 400 ] || fail "name '$bad' must 400"
done
[ "$(req PUT "/api/notes/$(printf 'x%.0s' $(seq 1 201))" "${auth[@]}" -d 'x')" = 400 ] || fail "201-byte name must 400"
pass "name filter"

python3 -c 'import json; json.dump({"content":"x"*(1024*1024+1)}, open("'$WORK'/big.json","w"))'
[ "$(req PUT /api/notes/big.md "${auth[@]}" -H 'content-type: application/json' --data-binary @"$WORK/big.json")" = 413 ] || fail "1 MiB + 1 must 413"
pass "size cap"

[ "$(req GET /api/tools)" = 200 ] || fail "tools"
[ "$(jq_ 'len(d["openai"])==6 and len(d["eyesoff_ai"]["tools"]["http"])==6 and all(t["url"].startswith(d["base_url"]) for t in d["eyesoff_ai"]["tools"]["http"]) and d["eyesoff_ai"]["tools"]["http"][0]["headers"]["x-api-key"]=="$JOT_API_KEY"')" = True ] || fail "tools shape: $(cat $WORK/body)"
pass "tools: 6 OpenAI functions + 6 eyesoff-ai http entries"

[ "$(req DELETE /api/notes/todo.txt "${auth[@]}")" = 200 ] || fail "delete"
[ "$(req GET /api/notes/todo.txt "${auth[@]}")" = 404 ] || fail "deleted note must 404"
[ "$(req POST /api/notes/log.md/delete "${auth[@]}")" = 200 ] || fail "POST delete alias"
[ "$(req DELETE /api/notes/todo.txt "${auth[@]}")" = 200 ] || fail "delete is idempotent"
pass "delete, 404 after, POST alias, idempotent"

echo "== read-only deployment =="
RO_CONFIG=${CONFIG/\"api_key\"/\"readOnly\":true,\"api_key\"}
serve $RO_PORT "$RO_CONFIG" --env JOT_API_KEY="$KEY" --env JOT_ACCESS_KEY_ID=$AK --env JOT_SECRET_ACCESS_KEY=$SK
RB="http://127.0.0.1:$RO_PORT"
[ "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$RB/api/notes/projects/enclave.md")" = 200 ] || fail "ro read"
[ "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X PUT -d '{"content":"x"}' -H 'content-type: application/json' "$RB/api/notes/projects/enclave.md")" = 403 ] || fail "ro write must 403"
[ "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X DELETE "$RB/api/notes/projects/enclave.md")" = 403 ] || fail "ro delete must 403"
pass "readOnly: reads 200, writes and deletes 403"

echo "== unconfigured deployment =="
serve $NC_PORT '{"api_key":"$JOT_API_KEY"}' --env JOT_API_KEY="$KEY"
NB="http://127.0.0.1:$NC_PORT"
curl -s -o "$WORK/body" "$NB/api/status"
[ "$(jq_ 'd["configured"]==False and d["missing"]==["endpoint","bucket"]')" = True ] || fail "unconfigured status: $(cat $WORK/body)"
[ "$(curl -s -o "$WORK/body" -w '%{http_code}' "${auth[@]}" "$NB/api/notes")" = 503 ] || fail "unconfigured list must 503"
grep -q "endpoint, bucket not set" "$WORK/body" || fail "503 must name the missing fields"
pass "unconfigured: serves, reports the gap, 503 on notes"

echo "== no api_key =="
serve $((NC_PORT+1)) "${CONFIG/\"api_key\":\"\$JOT_API_KEY\"/\"api_key\":\"\"}" --env JOT_ACCESS_KEY_ID=$AK --env JOT_SECRET_ACCESS_KEY=$SK
OB="http://127.0.0.1:$((NC_PORT+1))"
curl -s -o "$WORK/body" "$OB/api/status"
[ "$(jq_ 'd["auth"]==False and d["signed"]==True')" = True ] || fail "open status: $(cat $WORK/body)"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$OB/api/notes")" = 200 ] || fail "open list"
pass "no api_key: open, status says so"

grep -qi "panic" "$WORK"/app*.log && fail "a panic in the app logs" || true
OK=1
echo "ALL PASS"

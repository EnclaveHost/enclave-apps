#!/usr/bin/env bash
# End-to-end verification of ipns-publisher against a local kubo.
#
# Two layers, both hermetic against a local kubo (loopback only):
#
#  1. Record + HTTP + delegate (milestones 1-3): the app signs an IPNS
#     record, serves it on /routing/v1/ipns/<name>, PUTs it to a local kubo
#     acting as a delegated-routing endpoint, recovers the sequence from
#     that delegate on restart, and increments on a value change. Every
#     record is cross-checked with `ipfs name inspect --verify`.
#
#  2. DHT (milestones 4-6), only with E2E_DHT=1 and outbound internet: the
#     app publishes to the PUBLIC DHT and an independent, freshly-bootstrapped
#     kubo (empty cache, no delegate) resolves the name via `routing get`.
#     This is the definition of done; it is opt-in because it needs the
#     public network and a throwaway key.
#
# Needs: wasmtime, ipfs (kubo), curl, python3 on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_PORT=18490
BASE="http://127.0.0.1:$APP_PORT"
KUBO_API=15991
KUBO_GW=15992

# A fixed throwaway ed25519 seed (hex). Never a real name key.
SEED=32e1a1eb35a22c55220781fd739bbdab97470ea5a5873b7a0ad33f20182316cc
VAL_A=/ipfs/bafkreifzjut3te2nhyekklss27nh3k72ysco7y32koao5eei66wof36n5e
VAL_B=/ipfs/bafkreigetubgn2b6omjkdwxpwoaos2tua3ssiq3iyzqooy76lxthysl6ia

WORK=$(mktemp -d "${TMPDIR:-/tmp}/ipnspub-e2e.XXXXXX")
export IPFS_PATH="$WORK/kubo"
PIDS=()
OK=0
cleanup() {
  # setsid puts each daemon in its own process group; kill the group so no
  # child survives to hold the harness pipe
  for p in "${PIDS[@]:-}"; do kill -- "-$p" 2>/dev/null || kill "$p" 2>/dev/null || true; done
  if [ "$OK" = 1 ]; then rm -rf "$WORK"; else
    echo "FAILED - work dir kept at $WORK" >&2
    tail -8 "$WORK"/app*.log 2>/dev/null >&2 || true
  fi
}
trap cleanup EXIT
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
kubo() { ipfs --api "/ip4/127.0.0.1/tcp/$KUBO_API" "$@"; }
# read a record back out of the offline kubo via its /routing/v1 HTTP surface
# (`ipfs routing get` needs online mode; the HTTP GET does not)
delegate_get() { curl -s "$DELEGATE/routing/v1/ipns/$1" -H 'Accept: application/vnd.ipfs.ipns-record'; }

echo "== build =="
cargo build --release --target wasm32-wasip2 2>&1 | tail -1
WASM=target/wasm32-wasip2/release/ipns-publisher.wasm
# native binary for the CLI helpers (id / mkrecord)
cargo build --release 2>&1 | tail -1
BIN=target/release/ipns-publisher

NAME=$("$BIN" id "$SEED" | awk '/ipns-name/{print $2}')
echo "name under test: $NAME"

echo "== unit tests =="
cargo test --release 2>&1 | grep -E 'test result' || fail "unit tests"
pass "unit tests"

echo "== milestone 1: record bytes verify against kubo =="
"$BIN" mkrecord "$SEED" "$VAL_A" +172800 7 600000000000 > "$WORK/rec.bin"
kubo_out=$("$BIN" mkrecord "$SEED" "$VAL_A" +172800 7 600000000000 | \
  ipfs name inspect --verify "$NAME" 2>/dev/null) || true
# inspect reads from stdin
"$BIN" mkrecord "$SEED" "$VAL_A" +172800 7 600000000000 | \
  ipfs name inspect --verify "$NAME" 2>/dev/null | grep -q 'Valid: true' \
  || fail "kubo rejected our record"
pass "signed record validates in kubo (V1+V2)"

echo "== start local kubo (offline delegate) =="
ipfs init --profile=test >/dev/null 2>&1
ipfs config Addresses.API "/ip4/127.0.0.1/tcp/$KUBO_API" >/dev/null
ipfs config Addresses.Gateway "/ip4/127.0.0.1/tcp/$KUBO_GW" >/dev/null
setsid ipfs daemon --offline > "$WORK/kubo.log" 2>&1 < /dev/null &
PIDS+=($!)
for i in $(seq 1 30); do kubo id >/dev/null 2>&1 && break; sleep 0.5; done
kubo id >/dev/null 2>&1 || fail "kubo did not start"
DELEGATE="http://127.0.0.1:$KUBO_GW"

run_app() { # $1 = value
  setsid wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
    --env "ENCLAVE_PORTS=http:8000=$APP_PORT" \
    --env "IPNSPUB_CONFIG={\"ipnsKey\":\"$SEED\",\"value\":\"$1\",\"lifetimeSecs\":172800,\"ttlSecs\":600,\"delegates\":[\"$DELEGATE\"],\"dht\":false}" \
    "$WASM" > "$WORK/app.log" 2>&1 < /dev/null &
  APP_PID=$!
  PIDS+=($APP_PID)
  for i in $(seq 1 40); do curl -sf "$BASE/healthz" >/dev/null 2>&1 && return; sleep 0.25; done
  fail "app did not come up"
}
wait_seq() { # $1 = expected sequence
  for i in $(seq 1 40); do
    s=$(curl -s "$BASE/api/status" | python3 -c 'import json,sys;print(json.load(sys.stdin)["sequence"])' 2>/dev/null || echo -1)
    d=$(curl -s "$BASE/api/status" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["delegates"]))' 2>/dev/null || echo 0)
    [ "$s" = "$1" ] && [ "$d" -ge 1 ] && return
    sleep 0.5
  done
  fail "sequence did not reach $1 (got $s)"
}

echo "== milestone 2+3: sign, serve, delegate PUT (fresh name -> seq 0) =="
run_app "$VAL_A"
wait_seq 0
curl -s "$BASE/routing/v1/ipns/$NAME" -H 'Accept: application/vnd.ipfs.ipns-record' \
  | ipfs name inspect --verify "$NAME" 2>/dev/null | grep -q 'Valid: true' \
  || fail "served record did not verify"
pass "record served on /routing/v1 and verifies"
delegate_get "$NAME" | ipfs name inspect 2>/dev/null \
  | grep -q "$VAL_A" || fail "delegate did not accept the PUT"
pass "delegate (kubo) accepted the PUT"
kill $APP_PID 2>/dev/null; wait $APP_PID 2>/dev/null || true

echo "== milestone 7: sequence recovery + increment on value change =="
run_app "$VAL_B"
wait_seq 1   # recovers 0 from the delegate, value changed -> 1
delegate_get "$NAME" | ipfs name inspect 2>/dev/null \
  | grep -q "$VAL_B" || fail "delegate did not get the seq-1 record"
grep -q 'knows sequence 0' "$WORK/app.log" || fail "did not recover sequence from the delegate"
pass "recovered sequence 0 from the delegate, republished value at seq 1"
kill $APP_PID 2>/dev/null; wait $APP_PID 2>/dev/null || true

if [ "${E2E_DHT:-0}" = 1 ]; then
  echo "== milestones 4-6: public DHT PUT_VALUE -> independent kubo resolves =="
  echo "   (needs outbound internet; uses a throwaway key)"
  DSEED=$(python3 -c 'import os;print(os.urandom(32).hex())')
  DNAME=$("$BIN" id "$DSEED" | awk '/ipns-name/{print $2}')
  "$BIN" dhtpublish "$DSEED" "$VAL_A" 600 2>&1 | grep -E 'done:|failed:' | tail -1
  # a second, independent online kubo with an empty cache
  export IPFS_PATH="$WORK/kubo2"
  ipfs init --profile=server >/dev/null 2>&1
  ipfs config Addresses.API /ip4/127.0.0.1/tcp/15993 >/dev/null
  ipfs config Addresses.Gateway /ip4/127.0.0.1/tcp/15994 >/dev/null
  ipfs config Addresses.Swarm --json '["/ip4/0.0.0.0/tcp/15995"]' >/dev/null
  setsid ipfs daemon > "$WORK/kubo2.log" 2>&1 < /dev/null &
  PIDS+=($!)
  for i in $(seq 1 60); do
    n=$(ipfs --api /ip4/127.0.0.1/tcp/15993 swarm peers 2>/dev/null | wc -l)
    [ "$n" -gt 20 ] && break; sleep 1
  done
  ipfs --api /ip4/127.0.0.1/tcp/15993 routing get "/ipns/$DNAME" 2>/dev/null \
    | ipfs name inspect --verify "$DNAME" 2>/dev/null | grep -q 'Valid: true' \
    || fail "independent kubo could not resolve the DHT-published name"
  pass "independent kubo resolved the name from the public DHT (definition of done)"
else
  echo "== milestones 4-6 (DHT) skipped: set E2E_DHT=1 with internet to run =="
fi

OK=1
echo "ALL PASS"

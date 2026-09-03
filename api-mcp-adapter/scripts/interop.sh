#!/usr/bin/env bash
# The cross-app proof: eyesoff-ai's OWN MCP client, unchanged, against a live
# api-mcp-adapter under the real `wasmtime serve`.
#
# scripts/e2e.sh checks this server against a client written here, which can
# only prove the server is self-consistent. This runs the client that
# actually matters - eyesoff-ai's tools.rs - over a socket, and checks the
# things the two apps have to agree on: handshake-free discovery in one round
# trip, the `_meta` contract that carries an http entry's facts across, a
# templated call, pictures out and in, citations, and a per-user tool that is
# listed only when the request names someone.
#
# Needs: wasmtime, python3, cargo-component, and a checkout of eyesoff-ai
# beside this app. Loopback only.
set -euo pipefail
cd "$(dirname "$0")/.."
ADAPTER=$PWD
EYESOFF=${EYESOFF:-$PWD/../eyesoff-ai}
[ -f "$EYESOFF/src/tools.rs" ] || { echo "no eyesoff-ai at $EYESOFF (set EYESOFF=)" >&2; exit 1; }

BACKEND_PORT=18540
APP_PORT=18541
BACKEND="http://127.0.0.1:$BACKEND_PORT"
KEY=interop-key-$RANDOM$RANDOM
USER_SUB=0x00a329c0648769a73afac7f9381e08fb43dbea72

WORK=$(mktemp -d "${TMPDIR:-/tmp}/api-mcp-interop.XXXXXX")
PIDS=()
OK=0
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  if [ "$OK" = 1 ]; then rm -rf "$WORK"; else
    echo "FAILED - work dir kept at $WORK" >&2
    tail -20 "$WORK"/*.log 2>/dev/null >&2 || true
  fi
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== build the adapter =="
cargo component build --release --target wasm32-wasip2 2>&1 | tail -1
WASM=target/wasm32-wasip2/release/api_mcp_adapter.wasm
[ -f "$WASM" ] || fail "no $WASM"

echo "== stub backend =="
python3 scripts/stub_backend.py $BACKEND_PORT >"$WORK/backend.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 50); do
  curl -sf "$BACKEND/ping" >/dev/null && break
  sleep 0.2; [ "$i" = 50 ] && fail "the stub backend did not come up"
done

# The entries the interop test expects to find, in the exact shape an
# eyesoff-ai `tools.http` block writes them - which is the point: this
# config is a block lifted out of a chat deployment unchanged.
CONFIG=$(cat <<JSON
{"title":"interop tools","api_key":"\$MCP_ADAPTER_API_KEY",
 "http":[
  {"name":"echo_get","description":"reflect a GET","url":"$BACKEND/echo/{name}",
   "headers":{"x-api-key":"\$ECHO_KEY"},
   "parameters":{"type":"object","properties":{"name":{"type":"string"},"x":{"type":"string"}},"required":["name"]}},
  {"name":"generate_image","description":"draw a picture","url":"$BACKEND/v1/images/generations","method":"POST","timeout_s":60,
   "headers":{"authorization":"Bearer \$IMAGE_API_KEY"},
   "parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]},
   "body":{"prompt":"\$prompt","n":1},"result":{"image":"data.0.b64_json"}},
  {"name":"upscale_image","description":"upscale the attached picture","url":"$BACKEND/v1/images/upscale","method":"POST",
   "parameters":{"type":"object","properties":{"factor":{"type":"integer"}}},
   "body":{"image":"\$image","factor":"\$factor"},"result":{"image":"data.0.b64_json"}},
  {"name":"search","description":"search with citations","url":"$BACKEND/search",
   "parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]},
   "result":{"text":"text"},"sources":{"list":"results","title":"title","url":"url"}},
  {"name":"notes_read","description":"read a note of the signed-in user","url":"$BACKEND/notes/{name}",
   "headers":{"x-api-key":"\$JOT_API_KEY","x-user":"\$user"},
   "parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]},
   "result":{"text":"content"}},
  {"name":"fail","description":"a 500","url":"$BACKEND/fail"}
 ]}
JSON
)
echo "== adapter =="
wasmtime serve -Scommon --addr "127.0.0.1:$APP_PORT" --env ENCLAVE_CONFIG="$CONFIG" \
  --env MCP_ADAPTER_API_KEY="$KEY" --env ECHO_KEY=echokey --env IMAGE_API_KEY=imgkey \
  --env JOT_API_KEY=notekey "$WASM" >"$WORK/app.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$APP_PORT/ping" >/dev/null && break
  sleep 0.2; [ "$i" = 50 ] && fail "the adapter did not come up: $(tail -5 "$WORK/app.log")"
done

echo "== eyesoff-ai's MCP client, against it =="
cd "$EYESOFF"
ENCLAVE_MCP_INTEROP_URL="http://127.0.0.1:$APP_PORT/mcp" \
ENCLAVE_MCP_INTEROP_KEY="$KEY" \
ENCLAVE_MCP_INTEROP_USER="$USER_SUB" \
  cargo test --lib -- --nocapture --exact tools::tests::interop_with_a_live_adapter 2>&1 | tee "$WORK/test.log"
grep -q "interop: 6 tools, every claim checked" "$WORK/test.log" || fail "the interop test did not run to the end"
grep -q "test result: ok" "$WORK/test.log" || fail "the interop test failed"

OK=1
echo "ALL PASS: eyesoff-ai's client and this adapter interoperate"

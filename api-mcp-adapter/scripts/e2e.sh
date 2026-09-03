#!/usr/bin/env bash
# End-to-end verification of api-mcp-adapter through the real wasmtime serve
# (what the platform runs), against a stub backend that reflects what it
# received. scripts/mcp_client.py is the client and holds every check: the
# protocol surface, the key gate, per-user visibility and fail-closed
# identity, sign-in tokens minted the way the platform mints them (when cast
# is on PATH), every templating rule, pictures in and out, sources, caps,
# HTTP errors, missing secrets, and a locked deployment. Needs: wasmtime,
# python3, cargo-component. Loopback only.
set -euo pipefail
cd "$(dirname "$0")/.."

BACKEND_PORT=18520
APP_PORT=18521
LOCKED_PORT=18522
BASE="http://127.0.0.1:$APP_PORT"
BACKEND="http://127.0.0.1:$BACKEND_PORT"
KEY=adapter-key-$RANDOM$RANDOM

WORK=$(mktemp -d "${TMPDIR:-/tmp}/api-mcp-adapter-e2e.XXXXXX")
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
fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== build =="
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

# Tokens are minted the way the platform mints them (eyesoff-ai's
# PLATFORM-sso.md): claims -> base64url -> EIP-191 personal_sign of
# "EST1.<b64>" by the signer key. Key 0x42..42 is the spec's throwaway; its
# address is the pinned signer.
SIGNER=0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025
AUD_SELF=0x1111111111111111111111111111111111111111111111111111111111111111
AUD_EYES=0x2222222222222222222222222222222222222222222222222222222222222222
SUB_A=0x00a329c0648769a73afac7f9381e08fb43dbea72
SSO_TOKEN=""
if command -v cast >/dev/null 2>&1; then
  mint() { # sub aud iat exp
    python3 - "$@" <<'MINT'
import base64, json, subprocess, sys
sub, aud, iat, exp = sys.argv[1:5]
pk = "0x" + "42" * 32
payload = base64.urlsafe_b64encode(json.dumps({"v": 1, "sub": sub, "aud": aud, "iat": int(iat), "exp": int(exp)}, separators=(",", ":")).encode()).rstrip(b"=").decode()
msg = "EST1." + payload
sig = subprocess.check_output(["cast", "wallet", "sign", "--private-key", pk, msg]).decode().strip()
print(msg + "." + base64.urlsafe_b64encode(bytes.fromhex(sig[2:])).rstrip(b"=").decode())
MINT
  }
  NOW=$(date +%s)
  SSO_TOKEN=$(mint $SUB_A $AUD_EYES $NOW $((NOW+3600)))
else
  echo "WARN: cast (foundry) not on PATH; skipping the sign-in token checks"
fi

echo "== app =="
CONFIG=$(cat <<JSON
{"title":"e2e tools","api_key":"\$MCP_ADAPTER_API_KEY","timeout_s":5,"max_bytes":65536,
 "sso":{"signer":"$SIGNER","audience":"$AUD_SELF","accept":["$AUD_EYES"]},
 "http":[
  {"name":"echo_get","description":"reflect a GET","url":"$BACKEND/echo/{name}",
   "headers":{"x-api-key":"\$ECHO_KEY"},
   "parameters":{"type":"object","properties":{"name":{"type":"string"},"x":{"type":"string"},"flag":{"type":"boolean"}},"required":["name"]}},
  {"name":"echo_post","description":"reflect a POST","url":"$BACKEND/echo/{name}","method":"POST","format":"one line","max_chars":500,
   "parameters":{"type":"object","properties":{"name":{"type":"string"},"content":{"type":"string"},"tag":{"type":"string"}},"required":["name","content"]},
   "body":{"content":"\$content","tag":"\$tag","note":"costs \$5"}},
  {"name":"echo_delete","description":"reflect a DELETE","url":"$BACKEND/echo/{name}","method":"DELETE",
   "parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
  {"name":"generate_image","description":"draw","url":"$BACKEND/v1/images/generations","method":"POST","timeout_s":60,
   "headers":{"authorization":"Bearer \$IMAGE_API_KEY"},
   "parameters":{"type":"object","properties":{"prompt":{"type":"string"},"size":{"type":"string"}},"required":["prompt"]},
   "body":{"prompt":"\$prompt","n":1,"size":"\$size"},"result":{"image":"data.0.b64_json"}},
  {"name":"upscale_image","description":"upscale the attached picture","url":"$BACKEND/v1/images/upscale","method":"POST",
   "parameters":{"type":"object","properties":{"factor":{"type":"integer"}}},
   "body":{"image":"\$image","factor":"\$factor"},"result":{"image":"data.0.b64_json"}},
  {"name":"slow","description":"a slow picture","url":"$BACKEND/v1/images/generations","method":"POST","timeout_s":10,
   "headers":{"authorization":"Bearer \$IMAGE_API_KEY"},
   "parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]},
   "body":{"prompt":"\$prompt"},"result":{"image":"data.0.b64_json"}},
  {"name":"search","description":"search with citations","url":"$BACKEND/search",
   "parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]},
   "result":{"text":"text"},"sources":{"list":"results","title":"title","url":"url"}},
  {"name":"notes_read","description":"read a note of the signed-in user","url":"$BACKEND/notes/{name}",
   "headers":{"x-api-key":"\$JOT_API_KEY","x-user":"\$user"},
   "parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]},
   "result":{"text":"content"}},
  {"name":"big","description":"more than the cap","url":"$BACKEND/big","max_bytes":1024},
  {"name":"fail","description":"a 500","url":"$BACKEND/fail"},
  {"name":"nosecret","description":"a header whose secret is not set","url":"$BACKEND/ping","headers":{"x-key":"\$NOPE"}},
  {"name":"bad name","description":"dropped","url":"$BACKEND/ping"},
  {"name":"echo_get","description":"a duplicate, dropped","url":"$BACKEND/ping"}
 ]}
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
  fail "app on $port did not come up: $(tail -5 "$WORK/app$port.log")"
}
serve $APP_PORT "$CONFIG" --env MCP_ADAPTER_API_KEY="$KEY" --env ECHO_KEY=echokey --env IMAGE_API_KEY=imgkey --env JOT_API_KEY=notekey
# the same config with a key whose secret is not set: must lock, not open
serve $LOCKED_PORT "${CONFIG/\$MCP_ADAPTER_API_KEY/\$UNSET_KEY}" --env ECHO_KEY=echokey

echo "== the page =="
curl -s "$BASE/" | grep -q 'api-mcp-adapter' || fail "GET / is not the page"
echo "PASS: GET / serves the page"

echo "== the client =="
python3 scripts/mcp_client.py "$BASE" "$KEY" "http://127.0.0.1:$LOCKED_PORT" "$SSO_TOKEN"

OK=1
echo "ALL PASS"

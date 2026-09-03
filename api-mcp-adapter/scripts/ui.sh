#!/usr/bin/env bash
# The page at /, driven in a real browser against a live adapter: the key
# gate, the tool list and its tags, the connect snippets, a real tool call
# with citations, a picture rendered inline, a failure reported, and zero
# console errors.
#
# Needs: wasmtime, python3, cargo-component, node, and playwright somewhere.
# playwright is NOT a dependency of this repo, so point PLAYWRIGHT_DIR at a
# checkout that has it (or install it anywhere and set the var); without one
# the script says so and skips rather than failing.
set -euo pipefail
cd "$(dirname "$0")/.."

BACKEND_PORT=18570
APP_PORT=18571
BACKEND="http://127.0.0.1:$BACKEND_PORT"
KEY=ui-key-$RANDOM$RANDOM

# find playwright: an explicit dir, this repo, or the sibling enclave repo
for d in "${PLAYWRIGHT_DIR:-}" "$PWD/node_modules" "$PWD/../node_modules" "$HOME/Projects/enclave/node_modules"; do
  [ -n "$d" ] && [ -f "$d/playwright/index.mjs" ] && export PLAYWRIGHT_IMPORT="$d/playwright/index.mjs" && break
done
if [ -z "${PLAYWRIGHT_IMPORT:-}" ]; then
  echo "SKIP: playwright not found (set PLAYWRIGHT_DIR=<dir containing playwright/>)" >&2
  exit 0
fi
# playwright's own browser download, when the system has no chrome
for c in "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux64/chrome; do
  [ -x "$c" ] && export PLAYWRIGHT_CHROME="$c" && break
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/api-mcp-adapter-ui.XXXXXX")
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

# the same eleven entries scripts/e2e.sh uses, so the page is exercised
# against every shape: pictures both ways, citations, a per-user tool, a
# format prompt, a cap, a failure, a missing secret
CONFIG=$(cat <<JSON
{"title":"ui tools","api_key":"\$MCP_ADAPTER_API_KEY","timeout_s":5,"max_bytes":65536,
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
  {"name":"nosecret","description":"a header whose secret is not set","url":"$BACKEND/ping","headers":{"x-key":"\$NOPE"}}
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

echo "== the page =="
node scripts/ui.mjs "http://127.0.0.1:$APP_PORT" "$KEY"

OK=1

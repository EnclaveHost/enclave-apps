#!/usr/bin/env python3
"""Drive a running adapter as an MCP client and check the whole contract.

Usage: mcp_client.py <base_url> <api_key> <locked_base_url> [sso_token]

Every check is one line: what was asked, what came back, what was expected.
The protocol side (initialize, notifications, batches, errors, the 405s),
the auth side (key, per-user visibility, fail-closed identity), and the
templating side (URL placeholders, query strings, body holes, pictures in
and out, sources, caps, HTTP errors, missing secrets) against the stub
backend scripts/stub_backend.py serves.
"""
import base64
import json
import sys
import urllib.error
import urllib.request

BASE, KEY, LOCKED = sys.argv[1], sys.argv[2], sys.argv[3]
SSO = sys.argv[4] if len(sys.argv) > 4 else ""
META = "enclave.host/tool"
checks = 0


def ok(cond, what):
    global checks
    checks += 1
    if not cond:
        print("FAIL:", what)
        sys.exit(1)
    print("PASS:", what)


def http(method, url, body=None, headers=None, raw=False):
    # header names come back as the server sent them, so the checks below
    # read them lowercased rather than guessing a capitalization
    low = lambda hs: {k.lower(): v for k, v in hs.items()}
    data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
    req = urllib.request.Request(url, data=data, method=method, headers={"content-type": "application/json", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            payload = r.read()
            return r.status, low(dict(r.headers)), (payload if raw else (json.loads(payload) if payload else None))
    except urllib.error.HTTPError as e:
        payload = e.read()
        try:
            return e.code, low(dict(e.headers)), (payload if raw else (json.loads(payload) if payload else None))
        except Exception:
            return e.code, low(dict(e.headers)), payload


def rpc(method, params=None, headers=None, rid=1, base=None, key=KEY):
    msg = {"jsonrpc": "2.0", "id": rid, "method": method}
    if params is not None:
        msg["params"] = params
    h = {"x-api-key": key} if key else {}
    h.update(headers or {})
    return http("POST", (base or BASE) + "/mcp", msg, h)


def call(name, args, headers=None, key=KEY):
    st, _, r = rpc("tools/call", {"name": name, "arguments": args}, headers, key=key)
    assert st == 200, (st, r)
    return r["result"]


def text_of(res):
    return "\n".join(c["text"] for c in res["content"] if c["type"] == "text")


# ---- liveness and status ----------------------------------------------------
st, _, r = http("GET", BASE + "/ping")
ok(st == 200 and r["pong"], "GET /ping")
st, _, s = http("GET", BASE + "/api/status")
ok(st == 200 and s["configured"] and s["auth"] and s["users"] and s["tools"] == 11, f"GET /api/status: {s['tools']} tools, keyed, per-user")
ok(set(s["groups"]) == {"echo", "images", "search", "notes_read", "big", "fail", "nosecret"}, f"groups by eyesoff-ai's rule (a lone notes_read is its own switch): {s['groups']}")
ok(any("bad name" in n for n in s["notes"]), f"a bad entry is a note, not a failure: {s['notes']}")

# ---- auth -------------------------------------------------------------------
st, _, r = rpc("tools/list", key=None)
ok(st == 401 and r["error"]["code"] == -32001, "POST /mcp without the key is 401 + JSON-RPC error")
st, _, r = rpc("tools/list", key="wrong")
ok(st == 401, "POST /mcp with a wrong key is 401")
st, _, r = http("GET", BASE + "/api/tools")
ok(st == 401, "GET /api/tools without the key is 401")
st, _, r = rpc("tools/list", headers={"authorization": "Bearer " + KEY}, key=None)
ok(st == 200, "a bearer key works too (off-platform)")

# ---- protocol ---------------------------------------------------------------
st, _, r = rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}})
ok(st == 200 and r["result"]["protocolVersion"] == "2025-03-26" and r["result"]["serverInfo"]["name"] == "api-mcp-adapter", "initialize echoes a supported version")
ok("attested enclave" in r["result"]["instructions"] and "signed-in" in r["result"]["instructions"], "instructions mention the enclave and per-user tools")
ok(r["result"]["capabilities"]["tools"]["listChanged"] is False, "capabilities: tools only")
st, _, r = rpc("initialize", {"protocolVersion": "1999-01-01"})
ok(r["result"]["protocolVersion"] == "2025-06-18", "an unknown client version gets the newest")
st, h, r = http("POST", BASE + "/mcp", {"jsonrpc": "2.0", "method": "notifications/initialized"}, {"x-api-key": KEY})
ok(st == 202 and not r, "a notification is 202 with no body")
ok("mcp-session-id" not in {k.lower() for k in h}, "stateless: no session id is minted")
st, _, r = rpc("ping")
ok(st == 200 and r["result"] == {}, "ping")
st, _, r = rpc("resources/list")
ok(r["error"]["code"] == -32601, "unknown method is -32601")
st, _, r = http("POST", BASE + "/mcp", b"{not json", {"x-api-key": KEY})
ok(st == 400 and r["error"]["code"] == -32700, "a body that is not JSON is 400 / -32700")
st, _, r = http("POST", BASE + "/mcp", {"id": 1, "method": "ping"}, {"x-api-key": KEY})
ok(r["error"]["code"] == -32600, "a message without jsonrpc 2.0 is -32600")
st, _, r = http("POST", BASE + "/mcp", [
    {"jsonrpc": "2.0", "id": 1, "method": "ping"},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
], {"x-api-key": KEY})
ok(st == 200 and isinstance(r, list) and [m["id"] for m in r] == [1, 2], "a batch answers each request and skips the notification")
st, _, r = http("GET", BASE + "/mcp")
ok(st == 200 and r["endpoint"].endswith("/mcp") and r["tools"] == 11, "GET /mcp is an info page for people")
st, _, r = http("GET", BASE + "/mcp", headers={"accept": "text/event-stream"}, raw=True)
ok(st == 405, "GET /mcp for an event stream is 405")
st, _, r = http("DELETE", BASE + "/mcp", headers={"x-api-key": KEY}, raw=True)
ok(st == 405, "DELETE /mcp is 405 (nothing to end)")
st, h, _ = http("OPTIONS", BASE + "/mcp", raw=True)
ok(st == 204 and h.get("access-control-allow-origin") == "*" and "X-Api-Key" in h.get("access-control-allow-headers", ""), "OPTIONS /mcp answers CORS preflight")
st, _, r = http("POST", BASE + "/", {"jsonrpc": "2.0", "id": 1, "method": "ping"}, {"x-api-key": KEY})
ok(st == 200 and r["result"] == {}, "POST / serves MCP too")

# ---- tools/list: visibility and _meta ---------------------------------------
st, _, r = rpc("tools/list")
tools = {t["name"]: t for t in r["result"]["tools"]}
ok("notes_read" not in tools and "echo_get" in tools, f"per-user tools are hidden from a nameless caller: {sorted(tools)}")
st, _, r = rpc("tools/list", headers={"x-user": "0x00A329c0648769A73afAc7F9381E08FB43dBEA72"})
named = {t["name"]: t for t in r["result"]["tools"]}
ok("notes_read" in named and named["notes_read"]["_meta"][META]["user"] is True, "with X-User the per-user tool is listed and flagged")
ok(named["generate_image"]["_meta"][META] == {"group": "images", "result": "image", "timeout_s": 60}, f"generate_image _meta: {named['generate_image']['_meta']}")
ok(named["upscale_image"]["_meta"][META]["images"] is True and named["upscale_image"]["_meta"][META]["result"] == "image", "upscale_image takes pictures and returns one")
ok(named["echo_get"]["annotations"]["readOnlyHint"] is True and named["echo_delete"]["annotations"]["destructiveHint"] is True, "annotations follow the method")
ok(named["echo_post"]["_meta"][META]["format"] == "one line" and named["echo_post"]["_meta"][META]["max_chars"] == 500, "prompt-side settings ride _meta")
ok(named["search"]["inputSchema"]["required"] == ["q"], "inputSchema is the entry's parameters")
st, _, r = rpc("tools/list", headers={"x-user": "bob"})
ok(st == 400, "a malformed X-User is refused")

# ---- tools/call: templating -------------------------------------------------
res = call("echo_get", {"name": "a b/c", "x": "1", "flag": True})
e = json.loads(text_of(res))
ok(e["method"] == "GET" and e["path"] == "/echo/a%20b%2Fc" and e["query"] == {"x": "1", "flag": "true"}, f"{{arg}} substituted, leftovers on the query: {e['path']} {e['query']}")
ok(e["headers"].get("x-api-key") == "echokey", "the header secret resolved from the deployment's env")
res = call("echo_post", {"content": "hello", "name": "n"})
e = json.loads(text_of(res))
ok(e["method"] == "POST" and e["body"] == {"content": "hello", "note": "costs $5"} and e["path"] == "/echo/n", f"body template filled, optional hole pruned, literal $ kept: {e['body']}")
res = call("echo_post", {"content": "hello", "name": "n", "tag": "t"})
ok(json.loads(text_of(res))["body"]["tag"] == "t", "a filled optional hole is sent")
res = call("echo_delete", {"name": "z"})
ok(json.loads(text_of(res))["method"] == "DELETE", "DELETE goes without a body")
res = call("echo_get", {})
ok(res.get("isError") is True and "missing required argument" in text_of(res), "a missing required argument is refused before anything is sent")
res = call("nope", {})
ok(res.get("isError") is True and "no tool named 'nope'" in text_of(res) and "echo_get" in text_of(res), "an unknown tool lists what exists")

# ---- pictures ---------------------------------------------------------------
res = call("generate_image", {"prompt": "a cat", "size": "1024x768"})
ok(res["content"][0]["type"] == "image" and res["content"][0]["mimeType"] == "image/png", "an image result leads the content")
png = base64.b64decode(res["content"][0]["data"])
ok(png[:8] == b"\x89PNG\r\n\x1a\n", "the picture is the endpoint's bytes, base64 as returned")
ok(res["content"][1]["type"] == "text" and "a cat" in res["content"][1]["text"], "and a short note names the request")
res = call("generate_image", {"prompt": "busy"})
ok(res.get("isError") is True and "without an image" in text_of(res), "an answer without the image path is an in-band error")
data_uri = "data:image/png;base64," + base64.b64encode(png).decode()
res = call("upscale_image", {"factor": 2, "image": data_uri, "images": [data_uri]})
ok(res["content"][0]["type"] == "image" and res["content"][0]["mimeType"] == "image/webp", "the caller's picture rode the $image hole and a webp came back")
ok(base64.b64decode(res["content"][0]["data"])[:4] == b"RIFF", "a data-URI result keeps its own mime and bytes")
ok('"factor":2' in res["content"][1]["text"] and "base64" not in res["content"][1]["text"], "the note echoes the arguments minus the picture payload")
res = call("upscale_image", {"factor": 2})
ok(res.get("isError") is True and "HTTP 400" in text_of(res), "without a picture the hole is pruned and the endpoint says so")

# ---- sources, caps, errors, secrets, identity --------------------------------
res = call("search", {"q": "enclave"})
ok(res["structuredContent"]["sources"] == [
    {"title": "First hit for enclave", "url": "https://example.com/a"},
    {"title": "https://example.com/b", "url": "https://example.com/b"},   # no title: the url stands in
    {"title": "no url", "url": ""},                                       # no url: the title still lands
], f"sources rows ride structuredContent: {res['structuredContent']['sources']}")
ok(text_of(res) == "two hits about enclave", "result.text spares the model the envelope")
res = call("big", {})
ok(text_of(res).endswith("[response was cut off at 1024 bytes]") and len(text_of(res)) < 1200, "a body over the entry's cap is cut and says so")
res = call("fail", {})
ok(res.get("isError") is True and "HTTP 500: boom" in text_of(res), "an HTTP error carries the status and a hint")
res = call("nosecret", {})
ok(res.get("isError") is True and "$NOPE" in text_of(res) and "no such secret" in text_of(res), f"a missing secret is named, and nothing was sent: {text_of(res)}")
res = call("notes_read", {"name": "a"})
ok(res.get("isError") is True and "signed-in user" in text_of(res), "a per-user tool fails closed for a nameless caller")
res = call("notes_read", {"name": "a"}, headers={"x-user": "0x00A329c0648769A73afAc7F9381E08FB43dBEA72"})
ok(text_of(res) == "note a of 0x00a329c0648769a73afac7f9381e08fb43dbea72", "with X-User the endpoint gets the canonical account")
res = call("notes_read", {"name": "a"}, headers={"x-user": "acct_0e64d1897f10b32d3a1bc84e"})
ok(text_of(res) == "note a of acct_0e64d1897f10b32d3a1bc84e", "an account id is an identity too")
res = call("slow", {"prompt": "slow"})
ok(res["content"][0]["type"] == "image", "a slow endpoint inside its timeout still answers")

if SSO:
    st, _, r = rpc("tools/list", headers={"x-sso-token": SSO}, key=None)
    ok(st == 401, "a sign-in token NAMES the caller but does not open the key gate")
    st, _, r = rpc("tools/list", headers={"x-sso-token": SSO})
    ok(st == 200 and "notes_read" in {t["name"] for t in r["result"]["tools"]}, "with the key too, it names them and the per-user tool appears")
    res = call("notes_read", {"name": "t"}, headers={"x-sso-token": SSO})
    ok(text_of(res).startswith("note t of 0x"), f"the token's subject reaches the endpoint: {text_of(res)}")
    st, _, r = rpc("tools/list", headers={"x-sso-token": "EST1.garbage.garbage"})
    ok(st == 401 and "sso_required" in r["error"]["message"], "a bad token is 401 even beside a valid key")
    st, _, s = http("GET", BASE + "/api/status", headers={"x-sso-token": SSO})
    ok(s.get("you", {}).get("via") == "sso", "status says who you are, via sso")

# ---- /api/tools -------------------------------------------------------------
st, _, t = http("GET", BASE + "/api/tools", headers={"x-api-key": KEY})
ok(st == 200 and t["mcp"] == BASE + "/mcp" and t["users"] is True, "GET /api/tools with the key")
ok("notes_read" in t["hidden"] and "notes_read" not in {x["name"] for x in t["tools"]}, "hidden per-user tools are named")
entry = t["eyesoff_ai"]["tools"]["mcp"][0]
ok(entry["url"] == BASE + "/mcp" and entry["handshake"] is False and entry["headers"] == {"x-api-key": "$MCP_ADAPTER_API_KEY", "x-user": "$user"}, f"the eyesoff-ai entry is ready to paste: {entry}")
ok(entry["groups"]["images"] == ["generate_image", "upscale_image", "slow"] and entry["groups"]["notes_read"] == ["notes_read"], f"its groups map keeps eyesoff-ai's switches: {entry['groups']}")
ok(t["claude_code"].startswith("claude mcp add --transport http ") and "--header" in t["claude_code"], "a Claude Code one-liner")
st, _, p = http("GET", BASE + "/api/tools?call=search&args=%7B%22q%22%3A%22x%22%7D", headers={"x-api-key": KEY})
ok(st == 200 and p["ok"] and p["result"] == "two hits about x" and len(p["sources"]) == 3, "the probe runs one tool and reports sources")
st, _, p = http("GET", BASE + "/api/tools?call=generate_image&args=%7B%22prompt%22%3A%22p%22%7D", headers={"x-api-key": KEY})
ok(p["ok"] and p["image"]["mime"] == "image/png" and p["image"]["data_uri"].startswith("data:image/png;base64,"), "the probe hands a picture back as a data URI")
st, _, p = http("GET", BASE + "/api/tools?call=notes_read&args=%7B%22name%22%3A%22q%22%7D", headers={"x-api-key": KEY, "x-user": "0x00a329c0648769a73afac7f9381e08fb43dbea72"})
ok(p["ok"] and p["user"] == "0x00a329c0648769a73afac7f9381e08fb43dbea72", "the probe carries the caller's identity")
st, _, p = http("GET", BASE + "/api/tools?call=search&args=notjson", headers={"x-api-key": KEY})
ok(st == 400, "bad probe args are 400")

# ---- a locked deployment ------------------------------------------------------
st, _, s = http("GET", LOCKED + "/api/status")
ok(st == 200 and not s["configured"] and "$UNSET_KEY" in s["error"], "a key that references an unset secret locks the deployment")
st, _, r = rpc("tools/list", base=LOCKED, key="anything")
ok(st == 503 and r["error"]["code"] == -32000, "…and /mcp refuses rather than serving open")
st, _, r = http("GET", LOCKED + "/api/tools", headers={"x-api-key": "anything"})
ok(st == 503, "…and so does /api/tools")

print(f"ALL PASS ({checks} checks)")

#!/usr/bin/env python3
"""Split an eyesoff-ai config into an adapter config plus the entry that replaces it.

    ./scripts/from-eyesoff-config.py eyesoff.json --url https://<adapter-id8>.app.enclave.host

Reads a deployment's App Config, takes its `tools.http` array, and writes two
files beside it:

  <name>.adapter.json   the api-mcp-adapter config: those entries verbatim,
                        plus an api_key referencing $MCP_ADAPTER_API_KEY
  <name>.eyesoff.json   the same chat config with `tools.http` REPLACED by one
                        `tools.mcp` entry pointing at the adapter, carrying the
                        groups map that keeps the settings switches identical

Nothing is invented: the entries move across byte for byte, because the two
apps take the same entry shape. What changes is where they live, and therefore
which deployment holds the backends' API keys.

The groups map is computed by eyesoff-ai's own rule (an entry's own `group`,
else "images" for anything that makes or takes pictures, else a family name
shared with a sibling, else the function name), so the person's switches do
not move. Pass --group-images/--no-group-images to override how the images
group is decided if you have a reason.

Secrets: the adapter needs whatever secrets the entries reference (they are
printed at the end), and the eyesoff-ai deployment then needs only
MCP_ADAPTER_API_KEY. Set that to the adapter's api_key.
"""
import argparse
import json
import sys
from collections import OrderedDict


def makes_image(e):
    return bool((e.get("result") or {}).get("image"))


def wants_images(e):
    def scan(v):
        if isinstance(v, str):
            return v.strip() in ("$images", "$image", "${images}", "${image}")
        if isinstance(v, list):
            return any(scan(x) for x in v)
        if isinstance(v, dict):
            return any(scan(x) for x in v.values())
        return False
    return scan(e.get("body")) if e.get("body") is not None else False


def own_group(e, images_rule=True):
    g = (e.get("group") or "").strip()
    if g:
        return g
    if images_rule and (makes_image(e) or wants_images(e)):
        return "images"
    return None


def group_of(entries, i, images_rule=True):
    """eyesoff-ai's ToolsConfig::group_of, in Python."""
    e = entries[i]
    g = own_group(e, images_rule)
    if g:
        return g
    name = e.get("name", "")
    fam = name.replace("-", "_").split("_")[0]
    if fam and fam != name:
        for j, o in enumerate(entries):
            if j == i or own_group(o, images_rule):
                continue
            on = o.get("name", "")
            if on.replace("-", "_").split("_")[0] == fam:
                return fam
    return name


def secrets_in(v, out):
    """every $NAME / ${NAME} reference in a config value. `$user` is the
    reserved identity slot, not a secret, and a `$` that starts no name (a
    price, a shell literal) is text."""
    if isinstance(v, str):
        i = 0
        while True:
            i = v.find("$", i)
            if i < 0:
                return
            rest = v[i + 1:]
            if rest.startswith("{"):
                j = rest.find("}")
                name = rest[1:j] if j > 0 else ""
                i += 2 + len(name) + 1
            else:
                k = 0
                while k < len(rest) and (rest[k].isalnum() or rest[k] == "_"):
                    k += 1
                name = rest[:k]
                i += 1 + k
            if name and not name[0].isdigit() and name != "user":
                out.add(name)
    elif isinstance(v, list):
        for x in v:
            secrets_in(x, out)
    elif isinstance(v, dict):
        for x in v.values():
            secrets_in(x, out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("config", help="the eyesoff-ai App Config JSON")
    ap.add_argument("--url", required=True, help="the adapter's MCP endpoint, e.g. https://<id8>.app.enclave.host/mcp (a bare origin gets /mcp appended)")
    ap.add_argument("--api-key-secret", default="MCP_ADAPTER_API_KEY", help="the secret name for the adapter's key (default MCP_ADAPTER_API_KEY)")
    ap.add_argument("--title", default=None, help="the adapter deployment's title")
    ap.add_argument("--no-group-images", action="store_true", help="do not fold picture entries into one 'images' group")
    ap.add_argument("--out-prefix", default=None, help="where to write the two files (default: beside the input)")
    a = ap.parse_args()

    cfg = json.load(open(a.config), object_pairs_hook=OrderedDict)
    tools = cfg.get("tools")
    if not isinstance(tools, dict) or not isinstance(tools.get("http"), list) or not tools["http"]:
        sys.exit("that config has no tools.http array to move")
    entries = tools["http"]
    images_rule = not a.no_group_images

    url = a.url.rstrip("/")
    if not url.endswith("/mcp"):
        url += "/mcp"

    # the adapter config: the entries verbatim, plus the key gate
    adapter = OrderedDict()
    adapter["title"] = a.title or (cfg.get("title") or "tools")
    adapter["api_key"] = "$" + a.api_key_secret
    for k in ("timeout_s", "max_bytes"):
        if k in tools:
            adapter[k] = tools[k]
    adapter["http"] = entries

    # the groups map, by eyesoff-ai's rule
    groups = OrderedDict()
    for i, e in enumerate(entries):
        groups.setdefault(group_of(entries, i, images_rule), []).append(e["name"])

    per_user = any(str(v).strip() in ("$user", "${user}") for e in entries for v in (e.get("headers") or {}).values())
    headers = OrderedDict()
    headers["x-api-key"] = "$" + a.api_key_secret
    if per_user:
        headers["x-user"] = "$user"

    # the chat config, with the block replaced
    eyesoff = json.loads(json.dumps(cfg), object_pairs_hook=OrderedDict)
    et = eyesoff["tools"]
    del et["http"]
    et["mcp"] = [OrderedDict([
        ("url", url),
        ("handshake", False),
        ("headers", headers),
        ("groups", groups),
    ])]

    base = a.out_prefix or a.config.rsplit(".json", 1)[0]
    ap_path, ey_path = base + ".adapter.json", base + ".eyesoff.json"
    for path, doc in ((ap_path, adapter), (ey_path, eyesoff)):
        with open(path, "w") as f:
            json.dump(doc, f, indent=2, ensure_ascii=False)
            f.write("\n")

    # secrets live in urls and headers ONLY: a `$name` in a body template is
    # an ARGUMENT hole, filled from the model's call, and reporting it as a
    # missing secret would send someone hunting for a `$prompt` to set
    need = set()
    for e in entries:
        secrets_in(e.get("url", ""), need)
        secrets_in(e.get("headers") or {}, need)
    print(f"wrote {ap_path}  ({len(entries)} entries, {len(groups)} groups: {', '.join(groups)})")
    print(f"wrote {ey_path}  (tools.http replaced by one tools.mcp entry)")
    print()
    print("secrets the ADAPTER deployment needs:")
    for s in sorted(need):
        print(f"  {s}")
    print(f"  {a.api_key_secret}   (invent one; the adapter's own key)")
    print()
    print("secrets the EYESOFF-AI deployment then needs for tools:")
    print(f"  {a.api_key_secret}   (the same value)")
    if per_user:
        print()
        print("per-user tools are in this set: eyesoff-ai fills x-user from the")
        print("sign-in it verified for the turn, and the adapter lists them only")
        print("when a request names someone. Nothing else to configure.")


if __name__ == "__main__":
    main()

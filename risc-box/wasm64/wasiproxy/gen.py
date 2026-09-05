#!/usr/bin/env python3
"""Generate the wasi pass-through proxy from a resolved WIT JSON.

The proxy is a wasm32 component that exports every interface it imports,
forwarding each call verbatim. Composed under a memory64 component it puts
the engine's component-to-component adapters (which transcode 64<->32)
between a wasm64 app and a host whose typed canonical ABI is 32-bit only.

usage: gen.py proxy.json > src/lib.rs
"""
import json, sys

j = json.load(open(sys.argv[1]))
TYPES, IFACES, PKGS = j["types"], j["interfaces"], j["packages"]
WORLD = [w for w in j["worlds"] if w["name"] == "proxy"][0]
EXPORTED = [v["interface"]["id"] for v in WORLD["exports"].values() if "interface" in v]

KW = set("""as break const continue crate else enum extern false fn for if impl in let loop
match mod move mut pub ref return self Self static struct super trait true type unsafe use
where while async await dyn abstract become box do final macro override priv typeof unsized
virtual yield try gen""".split())
PRIM = {"u8":"u8","u16":"u16","u32":"u32","u64":"u64","s8":"i8","s16":"i16","s32":"i32",
        "s64":"i64","f32":"f32","f64":"f64","char":"char","bool":"bool","string":"String"}

def snake(s):
    s = s.replace("-", "_")
    return s + "_" if s in KW else s

def camel(s):
    return "".join(p[:1].upper() + p[1:] for p in s.split("-"))

def iface_path(iid):
    i = IFACES[iid]
    pkg = PKGS[i["package"]]["name"]           # wasi:cli@0.2.12
    ns, rest = pkg.split(":"); name = rest.split("@")[0]
    # Match wit-bindgen-core's name_package_module when the extracted graph
    # includes multiple versions (e.g. cli/run 0.2.0 and std's cli 0.2.12).
    versions = sum(p["name"].split("@")[0] == pkg.split("@")[0] for p in PKGS)
    module = snake(name)
    if versions > 1 and "@" in rest:
        module += rest.split("@", 1)[1].replace(".", "_").replace("-", "_").replace("+", "_")
    return f"{snake(ns)}::{module}::{snake(i['name'])}"

def kind(t):
    k = TYPES[t]["kind"]
    return (k, None) if isinstance(k, str) else next(iter(k.items()))

def root(t):
    """Follow `use`/alias chains to the defining type (or a primitive name)."""
    while not isinstance(t, str):
        k, v = kind(t)
        if k == "type": t = v
        else: return t
    return t

def owner_path(t):
    return iface_path(TYPES[t]["owner"]["interface"])

def ipath(t): return "crate::" + owner_path(t)
def epath(t): return "crate::exports::" + owner_path(t)
def wrapper(t):
    name = TYPES[t]["name"]
    duplicates = sum(x.get("name") == name and kind(i)[0] == "resource"
                     for i, x in enumerate(TYPES)) > 1
    return "Px" + camel(name) + (str(t) if duplicates else "")

def ty(t, side):
    """Rust type syntax on `side` ('i' import, 'e' export) for a WIT type."""
    t = root(t)
    if isinstance(t, str): return PRIM[t]
    k, v = kind(t)
    name = TYPES[t].get("name")
    if k in ("record", "variant", "enum", "flags", "resource"):
        base = ipath(t) if side == "i" else epath(t)
        return f"{base}::{camel(name)}"
    if k == "list": return f"Vec<{ty(v, side)}>"
    if k == "option": return f"Option<{ty(v, side)}>"
    if k == "result":
        ok = ty(v["ok"], side) if v["ok"] is not None else "()"
        err = ty(v["err"], side) if v["err"] is not None else "()"
        return f"Result<{ok}, {err}>"
    if k == "tuple":
        inner = ", ".join(ty(x, side) for x in v["types"])
        return f"({inner},)" if len(v["types"]) == 1 else f"({inner})"
    if k == "handle":
        hk, ht = next(iter(v.items())); ht = root(ht)
        rname = camel(TYPES[ht]["name"])
        if hk == "own":
            return f"{ipath(ht)}::{rname}" if side == "i" else f"{epath(ht)}::{rname}"
        return f"&{ipath(ht)}::{rname}" if side == "i" else f"{epath(ht)}::{rname}Borrow<'_>"
    raise SystemExit(f"unhandled type kind {k}")

def needs_conv(t):
    t = root(t)
    if isinstance(t, str): return False
    k, v = kind(t)
    if k in ("record", "variant", "enum", "flags", "resource", "handle"): return True
    if k in ("list", "option"): return needs_conv(v)
    if k == "result":
        return any(x is not None and needs_conv(x) for x in (v["ok"], v["err"]))
    if k == "tuple": return any(needs_conv(x) for x in v["types"])
    raise SystemExit(f"unhandled type kind {k}")

NAMED = {}   # (dir, root type id) -> function name; bodies emitted at the end

def conv(t, expr, d):
    """Expression converting `expr` across the boundary. d = 'i2e' | 'e2i'."""
    t = root(t)
    if isinstance(t, str) or not needs_conv(t): return expr
    k, v = kind(t)
    if k in ("record", "variant", "enum", "flags"):
        fn = f"conv_{d}_{snake(TYPES[t]['name'])}_{t}"
        NAMED[(d, t)] = fn
        return f"{fn}({expr})"
    if k == "list":
        inner = root(v)
        if not isinstance(inner, str) and kind(inner)[0] == "handle" and "borrow" in kind(inner)[1]:
            # a list of borrows is only ever a parameter: borrow the elements in place
            return f"{expr}.iter().map(|v| {conv_borrow(inner, 'v')}).collect::<Vec<_>>()"
        return f"{expr}.into_iter().map(|v| {conv(v, 'v', d)}).collect::<Vec<_>>()"
    if k == "option":
        return f"{expr}.map(|v| {conv(v, 'v', d)})"
    if k == "result":
        ok = f"Ok({conv(v['ok'], 'v', d)})" if v["ok"] is not None else "Ok(())"
        okp = "Ok(v)" if v["ok"] is not None else "Ok(())"
        err = f"Err({conv(v['err'], 'e', d)})" if v["err"] is not None else "Err(())"
        errp = "Err(e)" if v["err"] is not None else "Err(())"
        return f"match {expr} {{ {okp} => {ok}, {errp} => {err} }}"
    if k == "tuple":
        names = [f"t{i}" for i in range(len(v["types"]))]
        parts = ", ".join(conv(x, n, d) for x, n in zip(v["types"], names))
        pat = ", ".join(names)
        return f"{{ let ({pat},) = {expr}; ({parts},) }}"
    if k == "handle":
        hk, ht = next(iter(v.items())); ht = root(ht)
        rname = camel(TYPES[ht]["name"])
        if hk == "own":
            if d == "i2e": return f"{epath(ht)}::{rname}::new({wrapper(ht)}({expr}))"
            return f"{expr}.into_inner::<{wrapper(ht)}>().0"
        assert d == "e2i", "borrows only cross as parameters"
        return conv_borrow(t, expr)
    if k == "resource":
        raise SystemExit("bare resource type outside a handle")
    raise SystemExit(f"unhandled type kind {k}")

def conv_borrow(t, expr):
    hk, ht = next(iter(kind(t)[1].items())); ht = root(ht)
    return f"&{expr}.get::<{wrapper(ht)}>().0"

def emit_named(d, t):
    k, v = kind(t)
    src, dst = (ipath(t), epath(t)) if d == "i2e" else (epath(t), ipath(t))
    name = camel(TYPES[t]["name"]); fn = NAMED[(d, t)]
    out = [f"fn {fn}(x: {src}::{name}) -> {dst}::{name} {{"]
    if k == "record":
        fields = ", ".join(f"{snake(f['name'])}: {conv(f['type'], 'x.' + snake(f['name']), d)}" for f in v["fields"])
        out.append(f"    {dst}::{name} {{ {fields} }}")
    elif k == "variant":
        out.append("    match x {")
        for c in v["cases"]:
            cn = camel(c["name"])
            if c["type"] is None:
                out.append(f"        {src}::{name}::{cn} => {dst}::{name}::{cn},")
            else:
                out.append(f"        {src}::{name}::{cn}(v) => {dst}::{name}::{cn}({conv(c['type'], 'v', d)}),")
        out.append("    }")
    elif k == "enum":
        out.append("    match x {")
        for c in v["cases"]:
            cn = camel(c["name"])
            out.append(f"        {src}::{name}::{cn} => {dst}::{name}::{cn},")
        out.append("    }")
    elif k == "flags":
        out.append(f"    {dst}::{name}::from_bits_truncate(x.bits())")
    out.append("}")
    return "\n".join(out)

def arg_expr(p):
    """Export-side param -> expression to pass to the import."""
    t = root(p["type"]); name = snake(p["name"])
    e = conv(t, name, "e2i")
    if isinstance(t, str):
        return f"&{name}" if t == "string" else name
    k, v = kind(t)
    if k == "list":
        return f"&{e}" if needs_conv(t) else f"&{name}"
    return e

def params_sig(ps):
    return ", ".join(f"{snake(p['name'])}: {ty(p['type'], 'e')}" for p in ps)

def ret_sig(f):
    r = f.get("result")
    return f" -> {ty(r, 'e')}" if r is not None else ""

def body(call, f):
    r = f.get("result")
    if r is None: return f"        {call};"
    return f"        let r = {call};\n        {conv(r, 'r', 'i2e')}"

out = []
out.append('//! GENERATED by gen.py from the resolved WIT: do not edit.')
out.append('//! A wasm32 pass-through of the selected WASI interfaces.')
out.append('#![allow(unused, clippy::all, non_camel_case_types)]')
out.append('wit_bindgen::generate!({ path: "wit", world: "proxy", generate_all });')
out.append('pub struct Component;')
out.append('')

for iid in EXPORTED:
    i = IFACES[iid]; path = iface_path(iid)
    E, I = "crate::exports::" + path, "crate::" + path
    res = [tid for n, tid in i["types"].items() if kind(tid)[0] == "resource"]
    # resource wrappers
    for tid in res:
        out.append(f"pub struct {wrapper(tid)}(pub {I}::{camel(TYPES[tid]['name'])});")
    # interface Guest impl
    out.append(f"impl {E}::Guest for Component {{")
    for tid in res:
        out.append(f"    type {camel(TYPES[tid]['name'])} = {wrapper(tid)};")
    for fname, f in i["functions"].items():
        if f["kind"] != "freestanding": continue
        args = ", ".join(arg_expr(p) for p in f["params"])
        out.append(f"    fn {snake(fname)}({params_sig(f['params'])}){ret_sig(f)} {{")
        out.append(body(f"{I}::{snake(fname)}({args})", f))
        out.append("    }")
    out.append("}")
    # resource trait impls
    for tid in res:
        rname = camel(TYPES[tid]["name"])
        out.append(f"impl {E}::Guest{rname} for {wrapper(tid)} {{")
        for fname, f in i["functions"].items():
            fk = f["kind"]
            if isinstance(fk, dict):
                fkind, ftid = next(iter(fk.items()))
                if root(ftid) != tid: continue
            else:
                continue
            short = fname.split(".", 1)[1]
            if fkind == "method":
                ps = [p for p in f["params"] if p["name"] != "self"]
                args = ", ".join(arg_expr(p) for p in ps)
                sig = params_sig(ps)
                out.append(f"    fn {snake(short)}(&self{', ' if sig else ''}{sig}){ret_sig(f)} {{")
                out.append(body(f"self.0.{snake(short)}({args})", f))
                out.append("    }")
            elif fkind == "static":
                args = ", ".join(arg_expr(p) for p in f["params"])
                out.append(f"    fn {snake(short)}({params_sig(f['params'])}){ret_sig(f)} {{")
                out.append(body(f"{I}::{rname}::{snake(short)}({args})", f))
                out.append("    }")
            elif fkind == "constructor":
                args = ", ".join(arg_expr(p) for p in f["params"])
                out.append(f"    fn new({params_sig(f['params'])}) -> Self {{")
                out.append(f"        {wrapper(tid)}({I}::{rname}::new({args}))")
                out.append("    }")
        out.append("}")
    out.append("")

# named converters (may discover more while emitting: iterate to a fixpoint)
done = set()
while set(NAMED) - done:
    for key in sorted(set(NAMED) - done):
        out.append(emit_named(*key)); done.add(key)
out.append("")
out.append("export!(Component);")
print("\n".join(out))

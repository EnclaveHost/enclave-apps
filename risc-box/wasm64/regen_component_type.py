#!/usr/bin/env python3
"""Regenerate wasi-libc's checked-in `*_component_type.o` for another target.

The object wasi-libc ships is a wasm32 relocatable holding a dummy
force-link function and the `component-type:<world>` custom section that
wasm-component-ld needs to type the libc's WASI imports. wasm-ld refuses a
wasm32 object in a wasm64 link, so: compile the same dummy function for the
wanted triple, then splice the original custom section in before the
`linking` section.

  regen_component_type.py IN.o OUT.o --clang CLANG --target wasm64-wasip2
"""
import argparse, subprocess, sys, tempfile, os

def uleb(n):
    out = bytearray()
    while True:
        b = n & 0x7f; n >>= 7
        if n: out.append(b | 0x80)
        else: out.append(b); return bytes(out)

def read_uleb(b, i):
    r = 0; s = 0
    while True:
        x = b[i]; i += 1; r |= (x & 0x7f) << s; s += 7
        if not x & 0x80: return r, i

def sections(data):
    assert data[:4] == b"\0asm", "not wasm"
    i = 8; out = []
    while i < len(data):
        sid = data[i]; size, j = read_uleb(data, i + 1)
        out.append((sid, data[j:j + size], i, j + size)); i = j + size
    return out

def custom_name(payload):
    n, i = read_uleb(payload, 0); return payload[i:i + n].decode(), payload[i + n:]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src"); ap.add_argument("dst")
    ap.add_argument("--clang", default="clang"); ap.add_argument("--target", default="wasm64-wasip2")
    a = ap.parse_args()
    src = open(a.src, "rb").read()
    ct = [(custom_name(p)) for sid, p, _, _ in sections(src) if sid == 0]
    ct = [(n, body) for n, body in ct if n.startswith("component-type:")]
    assert len(ct) == 1, f"expected one component-type section, found {[n for n,_ in ct]}"
    name, body = ct[0]
    world = name.split(":", 1)[1]           # e.g. wasip2__wasi_libc
    tag = world.split("__")[0]              # wasip2
    with tempfile.TemporaryDirectory() as td:
        c = os.path.join(td, "ct.c")
        open(c, "w").write(f"void __component_type_object_force_link_{tag}(void) {{}}\n")
        o = os.path.join(td, "ct.o")
        subprocess.check_call([a.clang, f"--target={a.target}", "-O2", "-c", c, "-o", o])
        obj = open(o, "rb").read()
    secs = sections(obj)
    link = [s for s in secs if s[0] == 0 and custom_name(s[1])[0] == "linking"]
    assert link, "compiled object has no linking section"
    at = link[0][2]
    nm = name.encode()
    payload = uleb(len(nm)) + nm + body
    section = bytes([0]) + uleb(len(payload)) + payload
    out = obj[:at] + section + obj[at:]
    open(a.dst, "wb").write(out)
    print(f"{a.dst}: {len(out)} bytes, {name} ({len(body)} bytes) for {a.target}")

if __name__ == "__main__":
    main()

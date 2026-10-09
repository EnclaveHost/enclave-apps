#!/usr/bin/env python3
"""Export every repository in a depot bucket to plain bare git repositories,
without depot: the way out if the app is gone, the deployment lost, or you
just want your history back. Needs the bucket credentials and the master
key, nothing else (Python 3 with `cryptography`, and git).

    DEPOT_MASTER_KEY=… S3_ACCESS_KEY=… S3_SECRET_KEY=… \\
      python3 scripts/depot-export.py --endpoint https://<account>.r2.cloudflarestorage.com \\
        --bucket <bucket> [--prefix depot/] [--region auto] --out ./export [--repo NAME …]

Each pack is decrypted chunk by chunk (ChaCha20-Poly1305, every chunk
authenticated against its object key, position and the pack's length) into
objects/pack/, indexed with `git index-pack`, and the refs and HEAD from the
manifest are written; `git fsck` runs on the result. The format is the one
src/seal.rs and src/store.rs document.
"""
import argparse
import datetime
import hashlib
import hmac
import json
import os
import struct
import subprocess
import sys
import urllib.parse
import urllib.request
import zlib

from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

TAG = 16


def hm(key, *parts):
    m = hmac.new(key, digestmod=hashlib.sha256)
    for p in parts:
        m.update(p)
    return m.digest()


class Keys:
    def __init__(self, master):
        self.root = hm(b"depot-root-v1", master.encode())

    def derive(self, scope):
        return hm(self.root, b"depot-v1:", scope.encode())


class S3:
    def __init__(self, endpoint, region, bucket, ak, sk):
        self.endpoint, self.region, self.bucket, self.ak, self.sk = endpoint.rstrip("/"), region, bucket, ak, sk
        self.host = urllib.parse.urlparse(self.endpoint).netloc

    def get(self, key, rng=None):
        uri = "/" + urllib.parse.quote(self.bucket, safe="") + "/" + urllib.parse.quote(key, safe="/-_.~")
        now = datetime.datetime.now(datetime.timezone.utc)
        date, stamp = now.strftime("%Y%m%d"), now.strftime("%Y%m%dT%H%M%SZ")
        empty = hashlib.sha256(b"").hexdigest()
        headers = {"host": self.host, "x-amz-content-sha256": empty, "x-amz-date": stamp}
        if rng:
            headers["range"] = f"bytes={rng[0]}-{rng[1] - 1}"
        names = sorted(headers)
        creq = "\n".join(["GET", uri, "", "".join(f"{k}:{headers[k]}\n" for k in names), ";".join(names), empty])
        scope = f"{date}/{self.region}/s3/aws4_request"
        sts = "\n".join(["AWS4-HMAC-SHA256", stamp, scope, hashlib.sha256(creq.encode()).hexdigest()])
        k = hm(("AWS4" + self.sk).encode(), date.encode())
        for p in (self.region, "s3", "aws4_request"):
            k = hm(k, p.encode())
        sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
        headers["authorization"] = f"AWS4-HMAC-SHA256 Credential={self.ak}/{scope}, SignedHeaders={';'.join(names)}, Signature={sig}"
        req = urllib.request.Request(self.endpoint + uri, headers={k: v for k, v in headers.items() if k != "host"})
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return r.read()
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            raise SystemExit(f"storage GET {key}: HTTP {e.code} {e.read()[:300]!r}")


def open_sealed(key, aad, blob):
    if blob is None:
        raise SystemExit(f"missing object {aad.decode()}")
    if blob[:4] != b"DEP1":
        raise SystemExit(f"{aad.decode()} is not a depot object")
    plain = ChaCha20Poly1305(key).decrypt(blob[4:16], blob[16:], aad)
    return zlib.decompress(plain)


def export_pack(s3, keys, prefix, repo_id, pack, dest):
    okey = f"{prefix}r/{repo_id}/{pack['id']}.pack"
    total, chunk = pack["len"], pack.get("chunk", 262144)
    aead = ChaCha20Poly1305(keys.derive(f"pack:{repo_id}:{pack['id']}"))
    n = (total + chunk - 1) // chunk
    out = open(dest, "wb")
    i = 0
    step = 64  # chunks per ranged GET
    while i < n:
        j = min(n, i + step)
        a = i * (chunk + TAG)
        b = (j - 1) * (chunk + TAG) + min(chunk, total - (j - 1) * chunk) + TAG
        raw = s3.get(okey, (a, b))
        if raw is None or len(raw) != b - a:
            raise SystemExit(f"pack {pack['id']} is missing or short in storage")
        off = 0
        for k in range(i, j):
            plen = min(chunk, total - k * chunk)
            ct = raw[off:off + plen + TAG]
            off += plen + TAG
            aad = b"DEPK1" + struct.pack(">I", len(okey)) + okey.encode() + struct.pack(">QIQ", total, chunk, k)
            out.write(aead.decrypt(b"\0\0\0\0" + struct.pack(">Q", k), ct, aad))
        i = j
    out.close()
    data = open(dest, "rb").read()
    if data[:4] != b"PACK" or hashlib.sha1(data[:-20]).digest() != data[-20:]:
        raise SystemExit(f"pack {pack['id']} decrypted but its git checksum is wrong")


def git(*args, cwd=None):
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if r.returncode:
        raise SystemExit(f"git {' '.join(args)}: {r.stderr}")
    return r.stdout


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--endpoint", required=True)
    ap.add_argument("--bucket", required=True)
    ap.add_argument("--prefix", default="")
    ap.add_argument("--region", default="auto")
    ap.add_argument("--out", required=True)
    ap.add_argument("--repo", action="append", help="only these repositories (default: all)")
    a = ap.parse_args()
    master = os.environ.get("DEPOT_MASTER_KEY") or sys.exit("set DEPOT_MASTER_KEY")
    s3 = S3(a.endpoint, a.region, a.bucket, os.environ.get("S3_ACCESS_KEY", ""), os.environ.get("S3_SECRET_KEY", ""))
    keys = Keys(master)
    rkey = f"{a.prefix}registry"
    registry = json.loads(open_sealed(keys.derive("registry"), rkey.encode(), s3.get(rkey)))
    os.makedirs(a.out, exist_ok=True)
    for name, meta in sorted(registry["repos"].items()):
        if a.repo and name not in a.repo:
            continue
        rid = meta["id"]
        mkey = f"{a.prefix}r/{rid}/manifest"
        blob = s3.get(mkey)
        dest = os.path.join(a.out, name + ".git")
        if os.path.exists(dest):
            raise SystemExit(f"{dest} exists; refusing to overwrite")
        git("init", "-q", "--bare", dest)
        if blob is None:
            print(f"{name}: empty")
            continue
        m = json.loads(open_sealed(keys.derive(f"repo:{rid}"), mkey.encode(), blob))
        packdir = os.path.join(dest, "objects", "pack")
        for p in m["packs"]:
            tmp = os.path.join(packdir, f"tmp-{p['id']}.pack")
            export_pack(s3, keys, a.prefix, rid, p, tmp)
            git("index-pack", tmp, cwd=dest)
            # name the pack by its checksum, as git does
            data_sha = hashlib.sha1(open(tmp, "rb").read()[:-20]).hexdigest()
            final = os.path.join(packdir, f"pack-{data_sha}")
            os.replace(tmp, final + ".pack")
            os.replace(tmp[:-5] + ".idx", final + ".idx")
        stdin = "".join(f"create {ref} {oid}\n" for ref, oid in sorted(m["refs"].items()))
        r = subprocess.run(["git", "update-ref", "--stdin"], cwd=dest, input=stdin, capture_output=True, text=True)
        if r.returncode:
            raise SystemExit(f"writing refs: {r.stderr}")
        git("symbolic-ref", "HEAD", m["head"], cwd=dest)
        git("fsck", "--full", "--no-dangling", cwd=dest)
        print(f"{name}: {len(m['refs'])} refs, {len(m['packs'])} packs, revision {m['rev']}, fsck clean -> {dest}")


if __name__ == "__main__":
    main()

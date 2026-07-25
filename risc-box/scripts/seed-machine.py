#!/usr/bin/env python3
"""seed-machine.py — put a machine's OS images into an S3 bucket for RISC Box to
boot, and (optionally) fetch a ready-made RISC-V sample to upload.

Pure stdlib: the SigV4 signing here mirrors risc-box/src/s3.rs byte for byte, so
what seeds the bucket and what boots from it agree. No boto3, no aws CLI.

Examples
--------
Fetch the sample Buildroot/OpenSBI RISC-V images (kernel + rootfs) locally:

    ./seed-machine.py fetch-sample ./images

Upload them to your bucket (path-style; works with AWS S3, Wasabi, R2, minio):

    ./seed-machine.py put \\
        --endpoint https://s3.eu-central-1.wasabisys.com --region eu-central-1 \\
        --bucket my-bucket --access-key AKIA... --secret-key ... \\
        ./images/fw_payload.elf  images/fw_payload.elf
    ./seed-machine.py put ... ./images/rootfs.img  images/rootfs.img

Then deploy RISC Box with a config naming endpoint/bucket/kernel/fs (see README).

The sample images come from github.com/takahirox/riscv-rust (MIT); the same
emulator RISC Box vendors. A RISC-V kernel is an ELF with an SBI payload
(OpenSBI fw_payload or BBL+vmlinux); the rootfs is a raw disk image the guest
mounts as /dev/vda.
"""
import argparse, datetime, hashlib, hmac, os, sys, urllib.request, http.client
from urllib.parse import urlsplit

SAMPLE_BASE = "https://raw.githubusercontent.com/takahirox/riscv-rust/8ee69d7a5dc7ef6d8b2bda96bf86d2923f2cf176/resources/linux"
SAMPLES = {
    "fw_payload.elf": f"{SAMPLE_BASE}/opensbi/fw_payload.elf",
    "rootfs.img": f"{SAMPLE_BASE}/rootfs.img",
}

EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


def _sign_key(secret, date, region, service):
    k = ("AWS4" + secret).encode()
    for msg in (date, region, service, "aws4_request"):
        k = hmac.new(k, msg.encode(), hashlib.sha256).digest()
    return k


def put(args):
    body = open(args.local, "rb").read()
    u = urlsplit(args.endpoint)
    https = u.scheme == "https"
    host = u.hostname
    port = u.port or (443 if https else 80)
    host_header = host if ((https and port == 443) or (not https and port == 80)) else f"{host}:{port}"
    key = args.key.lstrip("/")
    # canonical URI is /bucket/key with each segment URI-encoded (slashes kept)
    def enc(s):
        out = ""
        for b in s.encode():
            c = chr(b)
            out += c if (c.isalnum() or c in "-_.~/") else f"%{b:02X}"
        return out
    canonical_uri = f"/{enc(args.bucket)}/{enc(key)}"
    payload_hash = hashlib.sha256(body).hexdigest()
    now = datetime.datetime.now(datetime.timezone.utc)
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    date = now.strftime("%Y%m%d")

    headers = {
        "host": host_header,
        "x-amz-content-sha256": payload_hash,
        "x-amz-date": stamp,
    }
    if args.session_token:
        headers["x-amz-security-token"] = args.session_token
    signed = ";".join(sorted(headers))
    canon_headers = "".join(f"{k}:{headers[k]}\n" for k in sorted(headers))
    canon_req = f"PUT\n{canonical_uri}\n\n{canon_headers}\n{signed}\n{payload_hash}"
    scope = f"{date}/{args.region}/s3/aws4_request"
    sts = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canon_req.encode()).hexdigest()}"
    sig = hmac.new(_sign_key(args.secret_key, date, args.region, "s3"), sts.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (
        f"AWS4-HMAC-SHA256 Credential={args.access_key}/{scope}, "
        f"SignedHeaders={signed}, Signature={sig}"
    )

    conn = (http.client.HTTPSConnection if https else http.client.HTTPConnection)(host, port, timeout=120)
    conn.request("PUT", canonical_uri, body=body, headers=headers)
    resp = conn.getresponse()
    data = resp.read()
    if resp.status != 200:
        sys.exit(f"PUT {key} failed: {resp.status} {data.decode(errors='replace')[:400]}")
    print(f"put {key}  ({len(body)} bytes)  -> {args.endpoint}/{args.bucket}")


def fetch_sample(args):
    os.makedirs(args.dest, exist_ok=True)
    for name, url in SAMPLES.items():
        out = os.path.join(args.dest, name)
        print(f"fetching {name} …")
        urllib.request.urlretrieve(url, out)
        print(f"  {out}  ({os.path.getsize(out)} bytes)")
    print(f"\ndone. upload with:\n  {sys.argv[0]} put --endpoint … --region … --bucket … "
          f"--access-key … --secret-key … {args.dest}/fw_payload.elf images/fw_payload.elf")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    f = sub.add_parser("fetch-sample", help="download sample RISC-V kernel + rootfs")
    f.add_argument("dest", nargs="?", default="./images")
    f.set_defaults(func=fetch_sample)

    p = sub.add_parser("put", help="SigV4 PUT a local file to s3://bucket/key")
    p.add_argument("--endpoint", required=True)
    p.add_argument("--region", default="us-east-1")
    p.add_argument("--bucket", required=True)
    p.add_argument("--access-key", required=True)
    p.add_argument("--secret-key", required=True)
    p.add_argument("--session-token", default=None)
    p.add_argument("local")
    p.add_argument("key")
    p.set_defaults(func=put)

    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()

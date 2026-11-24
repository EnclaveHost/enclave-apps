#!/usr/bin/env python3
"""End-to-end S3 check for sia-s3 (stdlib only, its own SigV4 signer).

  S3CHECK_ENDPOINT=http://127.0.0.1:18490 S3CHECK_AK=... S3CHECK_SK=... \
    python3 -I scripts/s3check.py [bucket]

Exercises exactly what s3-ipfs-adapter and rclone-style clients use: PUT (small,
multi-MiB, empty), HEAD, GET + ranges (incl. suffix and cross-part), CopyObject,
ListObjectsV2 (prefix, delimiter, pagination), multipart (create/part/complete,
abort), DELETE, and that the ETags follow S3's MD5 rules.
"""
import datetime
import hashlib
import hmac
import http.client
import os
import sys
import urllib.parse
import xml.etree.ElementTree as ET

EP = urllib.parse.urlparse(os.environ.get("S3CHECK_ENDPOINT", "http://127.0.0.1:18490"))
AK, SK = os.environ["S3CHECK_AK"], os.environ["S3CHECK_SK"]
REGION = os.environ.get("S3CHECK_REGION", "auto")
BUCKET = sys.argv[1] if len(sys.argv) > 1 else "test"
NS = "{http://s3.amazonaws.com/doc/2006-03-01/}"


def enc(s, keep_slash=False):
    return urllib.parse.quote(s, safe="-_.~" + ("/" if keep_slash else ""))


def req(method, key="", query=None, body=b"", headers=None):
    query = query or {}
    headers = dict(headers or {})
    path = "/" + enc(BUCKET) + ("/" + enc(key, True) if key else "")
    qs = "&".join(f"{enc(k)}={enc(v)}" for k, v in sorted(query.items()))
    now = datetime.datetime.now(datetime.timezone.utc)
    amz, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    host = EP.netloc
    ph = hashlib.sha256(body).hexdigest()
    headers.update({"host": host, "x-amz-date": amz, "x-amz-content-sha256": ph})
    signed = sorted(k.lower() for k in headers)
    hmap = {k.lower(): str(v).strip() for k, v in headers.items()}
    canon = "\n".join([method, path, qs, "".join(f"{h}:{hmap[h]}\n" for h in signed), ";".join(signed), ph])
    scope = f"{day}/{REGION}/s3/aws4_request"
    sts = f"AWS4-HMAC-SHA256\n{amz}\n{scope}\n{hashlib.sha256(canon.encode()).hexdigest()}"
    k = hmac.new(("AWS4" + SK).encode(), day.encode(), hashlib.sha256).digest()
    for part in (REGION, "s3", "aws4_request"):
        k = hmac.new(k, part.encode(), hashlib.sha256).digest()
    sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
    headers["Authorization"] = f"AWS4-HMAC-SHA256 Credential={AK}/{scope}, SignedHeaders={';'.join(signed)}, Signature={sig}"
    c = http.client.HTTPConnection(EP.hostname, EP.port or 80, timeout=600)
    c.request(method, path + ("?" + qs if qs else ""), body=body, headers=headers)
    r = c.getresponse()
    data = r.read()
    return r.status, {k.lower(): v for k, v in r.getheaders()}, data


fails = 0


def check(name, cond, detail=""):
    global fails
    print(("PASS " if cond else "FAIL ") + name + ("" if cond else f"  -- {detail}"))
    if not cond:
        fails += 1


def etag(h):
    return h.get("etag", "").strip('"')


P = "s3check/"
small = b"hello sia " * 100
big = os.urandom(6 << 20) + b"tail"

s, h, _ = req("PUT", P + "small.txt", body=small, headers={"content-type": "text/plain"})
check("PUT small", s == 200 and etag(h) == hashlib.md5(small).hexdigest(), (s, h))
s, h, _ = req("PUT", P + "big.bin", body=big)
check("PUT 6 MiB", s == 200 and etag(h) == hashlib.md5(big).hexdigest(), (s, h))
s, h, _ = req("PUT", P + "empty", body=b"")
check("PUT empty", s == 200 and etag(h) == hashlib.md5(b"").hexdigest(), (s, h))

s, h, _ = req("HEAD", P + "small.txt")
check("HEAD", s == 200 and h.get("content-length") == str(len(small)) and h.get("content-type") == "text/plain", (s, h))
s, h, d = req("GET", P + "small.txt")
check("GET small", s == 200 and d == small, (s, len(d)))
s, h, d = req("GET", P + "empty")
check("GET empty", s == 200 and d == b"", (s, d[:20]))
s, h, d = req("GET", P + "big.bin", headers={"range": "bytes=1048570-2097160"})
check("GET range", s == 206 and d == big[1048570:2097161] and h.get("content-range") == f"bytes 1048570-2097160/{len(big)}", (s, h.get("content-range"), len(d)))
s, h, d = req("GET", P + "big.bin", headers={"range": "bytes=-4"})
check("GET suffix range", s == 206 and d == b"tail", (s, d))
s, h, d = req("GET", P + "big.bin", headers={"range": f"bytes={len(big)}-"})
check("GET range past end -> 416", s == 416, s)
big_tag = hashlib.md5(big).hexdigest()
s, h, d = req("GET", P + "big.bin", headers={"range": "bytes=0-9", "if-match": f'"{big_tag}"'})
check("If-Match (same) -> 206", s == 206 and d == big[:10], s)
s, h, d = req("GET", P + "big.bin", headers={"range": "bytes=0-9", "if-match": '"0000"'})
check("If-Match (changed) -> 412", s == 412, s)
s, h, d = req("GET", P + "big.bin", headers={"if-none-match": f'"{big_tag}"'})
check("If-None-Match -> 304", s == 304 and d == b"", s)

s, h, d = req("PUT", P + "copy.txt", headers={"x-amz-copy-source": f"/{BUCKET}/{enc(P + 'small.txt', True)}"})
check("CopyObject", s == 200 and hashlib.md5(small).hexdigest() in d.decode(), (s, d[:200]))
s, h, d = req("GET", P + "copy.txt")
check("GET copy", s == 200 and d == small, (s, len(d)))
s, _, _ = req("DELETE", P + "small.txt")
s2, _, d = req("GET", P + "copy.txt")
check("copy survives source delete", s == 204 and s2 == 200 and d == small, (s, s2))
s, _, _ = req("GET", P + "small.txt")
check("deleted source is gone", s == 404, s)

# multipart: 3 parts (5 MiB, 5 MiB, small), complete, read across part boundaries
parts = [os.urandom(5 << 20), os.urandom(5 << 20), b"last part"]
s, h, d = req("POST", P + "multi.bin", query={"uploads": ""})
uid = ET.fromstring(d).findtext("UploadId") or ET.fromstring(d).findtext(NS + "UploadId")
check("CreateMultipartUpload", s == 200 and uid, (s, d[:200]))
tags = []
for n, body in enumerate(parts, 1):
    s, h, _ = req("PUT", P + "multi.bin", query={"partNumber": str(n), "uploadId": uid}, body=body)
    tags.append(etag(h))
    check(f"UploadPart {n}", s == 200 and etag(h) == hashlib.md5(body).hexdigest(), (s, h))
xmlbody = "<CompleteMultipartUpload>" + "".join(
    f"<Part><PartNumber>{n}</PartNumber><ETag>\"{t}\"</ETag></Part>" for n, t in enumerate(tags, 1)) + "</CompleteMultipartUpload>"
s, h, d = req("POST", P + "multi.bin", query={"uploadId": uid}, body=xmlbody.encode())
want = hashlib.md5(b"".join(bytes.fromhex(t) for t in tags)).hexdigest() + "-3"
check("CompleteMultipartUpload", s == 200 and want in d.decode(), (s, d[:300]))
whole = b"".join(parts)
s, h, d = req("GET", P + "multi.bin", headers={"range": f"bytes={(5 << 20) - 3}-{(10 << 20) + 4}"})
check("GET across part boundaries", s == 206 and d == whole[(5 << 20) - 3:(10 << 20) + 5], (s, len(d)))
s, h, d = req("GET", P + "multi.bin")
check("GET multipart whole", s == 200 and d == whole and etag(h) == want, (s, len(d), etag(h)))

s, h, d = req("POST", P + "aborted.bin", query={"uploads": ""})
uid2 = ET.fromstring(d).findtext("UploadId")
req("PUT", P + "aborted.bin", query={"partNumber": "1", "uploadId": uid2}, body=b"x" * 1000)
s, _, _ = req("DELETE", P + "aborted.bin", query={"uploadId": uid2})
s2, _, _ = req("PUT", P + "aborted.bin", query={"partNumber": "2", "uploadId": uid2}, body=b"y")
check("AbortMultipartUpload", s == 204 and s2 == 404, (s, s2))

# listing
for i in range(5):
    req("PUT", P + f"dir/f{i}", body=str(i).encode())
s, h, d = req("GET", "", query={"list-type": "2", "prefix": P, "delimiter": "/"})
root = ET.fromstring(d)
keys = [c.findtext(NS + "Key") for c in root.findall(NS + "Contents")]
cps = [c.findtext(NS + "Prefix") for c in root.findall(NS + "CommonPrefixes")]
check("ListObjectsV2 delimiter", s == 200 and P + "dir/" in cps and P + "big.bin" in keys and not any(k.startswith(P + "dir/") for k in keys), (keys, cps))
seen, token = [], None
while True:
    q = {"list-type": "2", "prefix": P + "dir/", "max-keys": "2"}
    if token:
        q["continuation-token"] = token
    s, h, d = req("GET", "", query=q)
    root = ET.fromstring(d)
    seen += [c.findtext(NS + "Key") for c in root.findall(NS + "Contents")]
    if root.findtext(NS + "IsTruncated") != "true":
        break
    token = root.findtext(NS + "NextContinuationToken")
check("ListObjectsV2 pagination", seen == [P + f"dir/f{i}" for i in range(5)], seen)

if os.environ.get("S3CHECK_KEEP") != "1":
    for k in ["big.bin", "empty", "copy.txt", "multi.bin"] + [f"dir/f{i}" for i in range(5)]:
        req("DELETE", P + k)
    s, h, d = req("GET", "", query={"list-type": "2", "prefix": P})
    check("cleanup", b"<Contents>" not in d, d[:200])

print(f"\n{'ALL PASSED' if fails == 0 else f'{fails} FAILED'}")
sys.exit(1 if fails else 0)

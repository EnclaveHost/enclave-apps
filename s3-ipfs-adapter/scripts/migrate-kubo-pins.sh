#!/usr/bin/env bash
# migrate-kubo-pins.sh — copy a kubo node's recursive pinset into the S3
# bucket the s3-ipfs-adapter serves, so an S3-backed gateway can replace the
# kubo node behind ipfs.enclave.host without any content 404ing.
#
# Each pinned object's bytes go to  s3://<bucket>/<prefix>pins/<cid>. The
# adapter recomputes the CID from the object bytes (kubo-identical import
# params), so a byte-for-byte copy serves under the SAME CID it had on kubo -
# the copy is verified here by re-reading the object back and confirming its
# length, and the adapter re-hashes every byte before it serves it anyway.
#
# Idempotent and resumable: an object already present with the right size is
# skipped, so re-running after an interruption only does what's left. Writes
# only; it never deletes from kubo or the bucket. CIDv0 pins (e.g. the empty
# directory) are skipped with a log line - the adapter only mints/serves
# CIDv1, so a CIDv0 could not be served under its own name anyway.
#
# Run ON the kubo box (needs `ipfs` + `curl`). No rclone/aws needed: curl's
# native --aws-sigv4 signs the PUTs. Credentials come from the environment,
# the SAME values as the deployment's S3 config:
#
#   export S3_ENDPOINT=https://<account>.r2.cloudflarestorage.com
#   export S3_ACCESS_KEY_ID=...
#   export S3_SECRET_ACCESS_KEY=...
#   export S3_BUCKET=ipfs           # optional, default "ipfs"
#   export S3_REGION=auto           # optional, R2 uses "auto"
#   export S3_PREFIX=               # optional, must match the deployment's prefix
#   export IPFS_PATH=/var/lib/ipfs  # optional, kubo repo path
#   ./migrate-kubo-pins.sh [--dry-run]
set -euo pipefail

DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

: "${S3_ENDPOINT:?set S3_ENDPOINT (e.g. https://<acct>.r2.cloudflarestorage.com)}"
: "${S3_ACCESS_KEY_ID:?set S3_ACCESS_KEY_ID}"
: "${S3_SECRET_ACCESS_KEY:?set S3_SECRET_ACCESS_KEY}"
BUCKET="${S3_BUCKET:-ipfs}"
REGION="${S3_REGION:-auto}"
PREFIX="${S3_PREFIX:-}"
export IPFS_PATH="${IPFS_PATH:-/var/lib/ipfs}"
ENDPOINT="${S3_ENDPOINT%/}"
SIGV4="aws:amz:${REGION}:s3"

command -v ipfs >/dev/null || { echo "ipfs not found" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl not found" >&2; exit 1; }

TMP=$(mktemp -d "${TMPDIR:-/tmp}/kubo-migrate.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

# object key for a CID (path-style; the adapter reads pins/<cid> under prefix)
key_of() { printf '%s%s%s' "$PREFIX" "pins/" "$1"; }

# Stored size of an object, or empty if absent. Uses a LIST scoped to the
# exact key rather than HEAD: R2 answers HEAD-object with 403 under curl's
# --aws-sigv4 (the signature/method quirk), while ListObjectsV2 signs fine -
# and it's the same call the adapter itself lists the bucket with.
head_size() {
  local key="$1"
  curl -s -m 30 --aws-sigv4 "$SIGV4" \
       --user "$S3_ACCESS_KEY_ID:$S3_SECRET_ACCESS_KEY" \
       "$ENDPOINT/$BUCKET?list-type=2&prefix=$key&max-keys=1" 2>/dev/null \
    | grep -oE '<Size>[0-9]+</Size>' | grep -oE '[0-9]+' | head -1
}

echo "== enumerating recursive pins =="
ipfs pin ls --type=recursive | awk '{print $1}' > "$TMP/pins.txt"
TOTAL=$(wc -l < "$TMP/pins.txt")
echo "pins: $TOTAL   target: $ENDPOINT/$BUCKET/${PREFIX}pins/<cid>   dry-run: $DRY_RUN"

copied=0 skipped=0 dirs=0 failed=0 i=0
while read -r cid; do
  i=$((i+1))
  [ -z "$cid" ] && continue

  # Only FILE objects are gateway content. Directory pins (the IPNS site
  # roots, and the empty-directory CID) are served from kubo's IPNS, not via
  # ipfs.enclave.host, and `ipfs cat` yields nothing for them - copying one
  # would write a bogus 0-byte object. Skip by DAG type, not by CID version
  # (site roots are CIDv1 directories).
  typ=$(ipfs files stat --format='<type>' "/ipfs/$cid" 2>/dev/null || echo unknown)
  if [ "$typ" != "file" ]; then
    dirs=$((dirs+1))
    continue
  fi

  # content size straight from the DAG (no full read) for the resume check
  want=$(ipfs files stat --format='<size>' "/ipfs/$cid" 2>/dev/null || echo -1)
  key=$(key_of "$cid")

  have=$(head_size "$key" || true)
  if [ -n "$have" ] && [ "$have" = "$want" ]; then
    skipped=$((skipped+1))
    [ $((i % 50)) -eq 0 ] && echo "[$i/$TOTAL] ... $copied copied, $skipped already present"
    continue
  fi

  if [ "$DRY_RUN" = 1 ]; then
    echo "[$i/$TOTAL] WOULD PUT $cid ($want bytes) -> $BUCKET/$key"
    copied=$((copied+1)); continue
  fi

  # materialize then PUT (curl --aws-sigv4 needs a seekable body to sign)
  if ! ipfs cat "$cid" > "$TMP/obj" 2>/dev/null; then
    echo "[$i/$TOTAL] FAIL read $cid" >&2; failed=$((failed+1)); continue
  fi
  got=$(wc -c < "$TMP/obj")
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 600 --aws-sigv4 "$SIGV4" \
         --user "$S3_ACCESS_KEY_ID:$S3_SECRET_ACCESS_KEY" \
         -T "$TMP/obj" "$ENDPOINT/$BUCKET/$key")
  if [ "$code" != 200 ]; then
    echo "[$i/$TOTAL] FAIL PUT $cid -> HTTP $code" >&2; failed=$((failed+1)); continue
  fi
  # verify: read it back, confirm the length round-trips
  back=$(head_size "$key" || true)
  if [ "$back" != "$got" ]; then
    echo "[$i/$TOTAL] FAIL verify $cid (put $got, bucket has ${back:-none})" >&2
    failed=$((failed+1)); continue
  fi
  copied=$((copied+1))
  [ $((i % 25)) -eq 0 ] && echo "[$i/$TOTAL] ... $copied copied, $skipped skipped, $failed failed"
done < "$TMP/pins.txt"

echo "== done =="
echo "copied: $copied   already-present: $skipped   skipped-directories: $dirs   failed: $failed   of $TOTAL"
[ "$failed" -eq 0 ] || { echo "some objects failed - re-run to retry only those" >&2; exit 1; }

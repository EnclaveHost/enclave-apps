#!/usr/bin/env bash
# Generate EnclaveDNS's two deployment secrets into an env file (mode 0600):
#   ENCLAVEDNS_ZONE_SEED    derives every hosted zone's DNSSEC signing key; the
#                           DS records you publish at registrars depend on it
#   ENCLAVEDNS_ADMIN_TOKEN  authorizes POST /api/flush (send it as X-Api-Key)
# Keep a copy of the seed outside the platform: a new seed means new keys, and
# every DS you published stops matching until you replace it.
set -euo pipefail
out="${1:-enclavedns-secrets.env}"
if [ -e "$out" ]; then echo "$out exists; refusing to overwrite" >&2; exit 1; fi
umask 077
{
  echo "ENCLAVEDNS_ZONE_SEED=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  echo "ENCLAVEDNS_ADMIN_TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
} > "$out"
echo "wrote $out (0600). Back up ENCLAVEDNS_ZONE_SEED before you publish a DS record."

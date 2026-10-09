#!/usr/bin/env bash
# Write a fresh secrets file for `enclave deploy depot --secrets-file <file>`:
# a new master key and admin token, plus the R2 (and witness) credentials you
# paste in.
#
#   scripts/new-secrets.sh depot-secrets.env
#
# The master key is the only key to everything depot stores. Copy it to your
# password manager BEFORE the first push: lose it and the bucket is
# unreadable, by design. The file is created 0600; delete it after deploying.
set -euo pipefail
out=${1:?usage: new-secrets.sh <file>}
if [ -e "$out" ]; then
  echo "$out exists; refusing to overwrite a key that may already protect data" >&2
  exit 1
fi
rand() { od -An -tx1 -N"$1" /dev/urandom | tr -d ' \n'; }
read -r -p "R2 access key id: " ak
read -r -s -p "R2 secret access key: " sk
echo
read -r -p "witness access key id (another provider; empty to skip): " wak
wsk=
if [ -n "$wak" ]; then
  read -r -s -p "witness secret access key: " wsk
  echo
fi
umask 077
cat >"$out" <<EOF
DEPOT_MASTER_KEY=$(rand 32)
DEPOT_ADMIN_TOKEN=$(rand 24)
DEPOT_R2_ACCESS_KEY=$ak
DEPOT_R2_SECRET_KEY=$sk
EOF
if [ -n "$wak" ]; then
  printf 'DEPOT_WITNESS_ACCESS_KEY=%s\nDEPOT_WITNESS_SECRET_KEY=%s\n' "$wak" "$wsk" >>"$out"
fi
echo "wrote $out (0600)"
echo "now: copy DEPOT_MASTER_KEY and DEPOT_ADMIN_TOKEN into your password manager"

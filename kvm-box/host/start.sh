#!/bin/bash
set -euo pipefail
# Usage: start.sh RELEASE WORK ENCLAVE_REPO VCEK AMD_CHAIN MIN_TCB (--deployment-id ID | --development-unbound)
release=$(realpath "${1:?release directory}"); work=$(realpath "${2:?private work directory}"); enclave=$(realpath "${3:?enclave repository}")
vcek=$(realpath "${4:?VCEK file}"); chain=$(realpath "${5:?AMD chain PEM}"); tcb=$(realpath "${6:?minimum TCB JSON}")
here=$(cd "$(dirname "$0")" && pwd)
shift 6
binding=("$@")
if ! { [ "${#binding[@]}" -eq 1 ] && [ "${binding[0]}" = --development-unbound ]; } && ! { [ "${#binding[@]}" -eq 2 ] && [ "${binding[0]}" = --deployment-id ]; }; then
 echo 'Choose --deployment-id ID or explicitly --development-unbound' >&2; exit 2
fi
node "$here/deployment-binding.mjs" "${binding[@]}"
[ -r "$work/owner-password" ] || { echo 'Build first: missing owner-password' >&2; exit 1; }
for unit in kvm-box-broker-dev kvm-box-connector-dev; do
 if systemctl --user is-active --quiet "$unit"; then echo "$unit is already running; stop it before changing releases" >&2; exit 1; fi
done
common=(--release "$release" --work "$work" --enclave "$enclave" --vcek "$vcek" --chain "$chain" --min-tcb "$tcb" "${binding[@]}")
systemd-run --user --unit=kvm-box-broker-dev --collect -p Restart=no \
 python3 "$here/broker.py" "${common[@]}" --parent-unit kvm-box-broker-dev.service
systemd-run --user --unit=kvm-box-connector-dev --collect -p BindsTo=kvm-box-broker-dev.service \
 node "$here/connect.mjs" "${common[@]}" --password-file "$work/owner-password" --listen-port "${KVMBOX_LOCAL_PORT:-0}"
echo "The local console URL is recorded in $work/connector.json. Both guests require verified SNP attestation."

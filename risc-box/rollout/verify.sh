#!/bin/bash
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
log="$root/../../work/snapshot-validation"
mkdir -p "$log"
read -r available blocksize < <(stat -f -c "%a %S" /)
(( available * blocksize >= 48 * 1024 * 1024 * 1024 )) || { echo "Root free space is below 48 GiB" >&2; exit 1; }
unit="enclave-snapshot-page-tests-$(date +%s)"
exec systemd-run --user --wait --pipe --unit="$unit" -p MemoryMax=256M -p MemorySwapMax=0 -p CPUQuota=100% -p RuntimeMaxSec=60 -p KillMode=control-group -p TimeoutStopSec=5 node /home/steven/Documents/Codex/2026-10-18/wo/work/enclave/test/helpers/memory-guard.mjs --max-rss-mib 200 --floor-available-mib 16384 --log "$log/$unit.guard.log" -- python3 /home/steven/Documents/Codex/2026-10-18/wo/work/claude-test-queue.py --mib 256 --log "$log/$unit.queue.log" -- bash -c 'set -euo pipefail; ulimit -f 2048; for test_file in "$@"; do node --experimental-test-isolation=none --test --test-concurrency=1 "$test_file"; done' snapshot-tests "$root/test/rollout.test.mjs" "$root/test/private.test.mjs" "$root/test/migrate.test.mjs" "$root/test/update.test.mjs"

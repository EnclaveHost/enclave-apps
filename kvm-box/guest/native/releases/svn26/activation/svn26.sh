#!/bin/bash
# Move the relay and the NucBox to the native desktop release SVN26 (07c4efd2...): SVN25 with the 27B model added to
# the model volume, and nothing else. README.md one folder up says what the image is. Made after
# native-shield-image-v22/activation/svn25.sh; the NucBox side of each step is svn26-box.ps1.
#
#   svn26.sh status         read only: the relay's policy, the NucBox's bundle, hooks, policy, manager, node
#   svn26.sh stage          copy the release files to the NucBox and check them there; nothing running changes
#   svn26.sh relay          control VM: the Shield policy, then one relay restart
#   svn26.sh box            NucBox: the two local hooks and the box's policy to SVN26, then host bundle 2026.11.20 again
#   svn26.sh retry          ask the NucBox to claim 0xf9bb979a now, and watch the launch
#   svn26.sh launch         read only: the desktop VM and what the node last logged for the deployment
#   svn26.sh box-revert     NucBox: hooks and policy back to SVN25, then host bundle 2026.11.20 again
#   svn26.sh relay-revert   control VM: the previous policy back, then one relay restart
#
# Order: stage, relay, box, retry. The relay's code, the release schema, the controller, its CID and the catalog
# version (2, label 0.9.2) are SVN25's, so no owner signature and no host code changes; only the policy's desktop
# image entry and release block move (measurement, image and profile hashes, SVN floor 26).
#
# What it costs. relay: the API relay restarts once (host tunnels drop for some seconds). From then on it admits only
# the SVN26 desktop, so KVM Box is down from here until the SVN26 desktop has launched and enrolled. box: a full
# install of the host bundle that is already installed, as on 2026-11-18, 2026-11-20 and twice on 2026-11-24: Shield,
# Node, Network and Card restart, the manager recovers the running partitions and the node retires and starts them
# again: about seventeen minutes for the apps. The desktop's app identity moves with the image (97044489 to
# 7d4c0f91), so the node gives the old desktop up; retry sends one claim hint, paid from the app's gas reserve.
# New this time: the model disk is 20.8 GB instead of 2.5, staged with the other media (stage skips the upload when
# the file is already in the release folder) and hashed by the manager at every desktop launch.
# SVN26 has not run on the NucBox's hardware: its root image and model volume booted in the isolated KVM probe.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
EXPORT=${EXPORT:-/home/steven/Documents/Codex/2026-10-23/it/outputs/kvm-box/native-platform-v23-27b-models}
CVM=${CVM:-/vm/enclave-volumes/rpc-snp-release-r14/cvm.sh}
export CVM_IP=${CVM_IP:-172.25.202.35}   # the control VM's address since the NucBox restart of 2026-11-20 08:45Z
NEW=07c4efd2fe3db9bc906e0d89d4db62284ed5747d534d204efbd4e758cbbc24a1
OLD_POLICY=ae6326749c2b30b6317f41487bff932c448d560b3ba4e3e694a235a3226e31dc
NEW_POLICY=866bc49effcf7fb588af0f2b98feb4b064b3c7a8bf2afb4a8893294242d6bfe1
DEST="C:/ProgramData/Enclave/native-desktop/releases/$NEW"
KVM=0xf9bb979ae685c7f16f6c49df3f35f76ff38aca86b5ce3e76e02b83115360dd58
HINT=${HINT:-https://api.enclave.host/v1/claim-hint}
LOG=$here/host-switch.log
OUT=$(mktemp); trap 'rm -f "$OUT" "$OUT.have"' EXIT
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=15 minipc)
say() { printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$LOG"; }
stop() { say "STOP: $*"; exit 1; }
quiet() { sed '/post-quantum\|store now\|openssh.com\/pq/d'; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
box() {
  { printf "\$mode='%s'\n%s\n" "$1" "${2:-}"; cat "$here/svn26-box.ps1"; } |
    timeout "${BOX_TIMEOUT:-0}" "${SSH[@]}" 'powershell -NoProfile -ExecutionPolicy Bypass -Command "& ([scriptblock]::Create([Console]::In.ReadToEnd()))"' 2>&1 |
    tr -d '\r' | quiet | tee "$OUT" | tee -a "$LOG" || true
}
did() { grep -q "^$1" "$OUT"; }

# the files prepared.json names, from this folder or the export, each checked by size and hash
local_files() {
  python3 -I - "$here" "$EXPORT" <<'PY'
import hashlib, json, os, sys
act, export = sys.argv[1:]
for name, want in json.load(open(os.path.join(act, "prepared.json")))["stage"].items():
    if "copyFrom" in want: continue
    p = os.path.join(act if os.path.exists(os.path.join(act, name)) else export, name)
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""): h.update(b)
    if os.path.getsize(p) != want["bytes"] or h.hexdigest() != want["sha256"]: sys.exit("%s is not the prepared file" % p)
    print(p)
PY
}

stage() {
  local files f; files=$(local_files) || stop "the local release files do not match prepared.json; nothing copied"
  BOX_TIMEOUT=900 box verify
  if did 'DONE staged'; then say "already staged"; return; fi
  BOX_TIMEOUT=120 box prepare; did 'DONE prepare' || stop "the release folder could not be prepared"
  cp "$OUT" "$OUT.have"
  for f in $files; do
    if grep -q "^HAVE $(basename "$f") $(stat -c %s "$f")\$" "$OUT.have"; then say "$(basename "$f") is already in the release folder (same length); not copied again"; continue; fi
    say "copying $(basename "$f") ($(stat -c %s "$f") bytes)"
    scp -q -o BatchMode=yes -o ConnectTimeout=15 "$f" "minipc:$DEST/$(basename "$f")" 2>&1 | quiet | tee -a "$LOG"
  done
  BOX_TIMEOUT=900 box verify; did 'DONE staged' || stop "the staged files did not verify on the NucBox"
  say "staged; nothing running was changed"
}

relay() {
  local mode=$1 p=
  [ "$(sha "$here/shield-policy.json")" = "$NEW_POLICY" ] || stop "the prepared policy is not the one this script names; nothing changed"
  if [ "$mode" = apply ]; then p=$(base64 -w0 "$here/shield-policy.json"); fi
  { printf 'MODE = "%s"\nB64 = "%s"\nOLD = "%s"\nNEW = "%s"\n' "$mode" "$p" "$OLD_POLICY" "$NEW_POLICY"
    cat <<'PY'; } | CVM_TIMEOUT=600 "$CVM" 'sudo -n python3 -I -' 2>&1 | quiet | tee "$OUT" | tee -a "$LOG" || true
import base64, hashlib, json, os, subprocess, sys, time, urllib.request
F = "/etc/nan-relay/shield-policy.json"
BAK = ".before-native-svn26-20261124"
ALLOW = "/etc/enclave-control/ALLOW_CONTROL_START"; UNIT = "enclave-api-relay"
sha = lambda b: hashlib.sha256(b).hexdigest()
def health():
    try: return json.load(urllib.request.urlopen("http://127.0.0.1:8100/health", timeout=5))
    except Exception: return None
def line(h): return "no answer" if h is None else "ok=%s enclaves=%s of %s" % (h.get("ok"), h.get("enclaves"), h.get("of"))
def wait(seconds):
    h = None
    for _ in range(seconds // 5):
        time.sleep(5); h = health()
        if h and h.get("ok") is True: break
    return h
def put(data):
    st = os.stat(F); tmp = F + ".new"
    with open(tmp, "wb") as o: o.write(data)
    os.chmod(tmp, st.st_mode & 0o777); os.chown(tmp, st.st_uid, st.st_gid); os.replace(tmp, F)
def stop(why): print("STOP: " + why); sys.exit(0)

live = open(F, "rb").read(); before = health()
state = "new" if sha(live) == NEW else "old" if sha(live) == OLD else "other " + sha(live)[:16]
print("relay before: %s; policy %s" % (line(before), state))
if MODE == "status": print("DONE status"); sys.exit(0)
source, target = ("old", "new") if MODE == "apply" else ("new", "old")
if state == target: print("DONE relay (the policy is already this one; relay %s)" % line(before)); sys.exit(0)
if state != source: stop("the live policy is not the %s one; nothing changed" % ("SVN25" if MODE == "apply" else "SVN26"))
if not os.path.exists(ALLOW): stop("the control VM does not allow the relay to start (%s is missing); nothing changed" % ALLOW)
if not (before and before.get("ok") is True): stop("the relay is not healthy now; nothing changed")
if MODE == "apply":
    data = base64.b64decode(B64)
    if sha(data) != NEW: stop("the policy that arrived is not the prepared one; nothing changed")
    if not os.path.exists(F + BAK):
        with open(F + BAK, "wb") as o: o.write(live)
        os.chmod(F + BAK, 0o644)
    if sha(open(F + BAK, "rb").read()) != OLD: stop("the kept copy %s is not the previous policy; nothing changed" % (F + BAK))
else:
    if not os.path.exists(F + BAK): stop("no previous policy was kept; nothing changed")
    data = open(F + BAK, "rb").read()
    if sha(data) != OLD: stop("the kept copy of the policy is not the previous one; nothing changed")
put(data)
subprocess.run(["systemctl", "restart", UNIT], check=True)
after = wait(180)
print("relay after the restart: " + line(after))
if after and after.get("ok") is True:
    print("DONE relay %s (previous policy kept beside it as *%s)" % (sha(data)[:16], BAK)); sys.exit(0)
print("the relay is not healthy on this policy: putting the previous one back")
put(live)
subprocess.run(["systemctl", "restart", UNIT], check=True)
again = wait(180)
log = subprocess.run(["journalctl", "-u", UNIT, "-n", "30", "--no-pager", "-o", "cat"], capture_output=True, text=True).stdout
for l in log.splitlines():
    if not any(w in l.lower() for w in ("key", "secret", "token", "password")): print("  " + l[:220])
stop("nothing was changed; the policy is back as it was and the relay reports " + line(again))
PY
  [ "$mode" = status ] || did 'DONE relay' || stop "the relay step did not finish; see the lines above"
}

answers() {
  local a
  for a in ef514ed3 7ae476a3 d9798e4c a77d0c57 a69dcbba 59f1273d c13d4b6a 38f368d6 "${KVM:2:8}"; do
    say "  $a.app.enclave.host: $(curl -s -m 20 -o /dev/null -w '%{http_code} in %{time_total} s' "https://$a.app.enclave.host/" || true)"
  done
}

case "${1:-}" in
  status) relay status; BOX_TIMEOUT=300 box status ;;
  stage) stage ;;
  answers) answers ;;
  relay)
    BOX_TIMEOUT=300 box verify; did 'DONE staged' || stop "the release is not staged on the NucBox: run '$0 stage' first (nothing changed)"
    say "relay: SVN26 policy"; relay apply ;;
  relay-revert) say "relay: back to the SVN25 policy"; relay revert ;;
  box)
    relay status; grep -q 'policy new' "$OUT" || stop "the relay is not on the SVN26 policy: run '$0 relay' first (nothing changed)"
    BOX_TIMEOUT=300 box verify; did 'DONE staged' || stop "the staged release folder does not verify on the NucBox; nothing changed"
    say "NucBox: hooks and policy to SVN26, then host bundle 2026.11.20 again (about seventeen minutes for the apps to come back)"
    BOX_TIMEOUT=2400 box apply; did 'DONE box' || stop "the NucBox step did not finish; see the lines above. '$0 status' reads where it stands"
    answers ;;
  box-revert)
    say "NucBox: hooks and policy back to SVN25, then host bundle 2026.11.20 again"
    BOX_TIMEOUT=2400 box revert; did 'DONE box' || stop "the NucBox revert did not finish; see the lines above"
    answers ;;
  retry)
    BOX_TIMEOUT=180 box status; grep -q "^manager: .*igvm=${NEW:0:12}" "$OUT" || stop "the NucBox manager does not report SVN26: run '$0 box' first (nothing sent)"
    since=$(date -u +%H:%M:%S)
    a=$(curl -s -m 90 -X POST -H 'content-type: application/json' -d "{\"id\":\"$KVM\",\"enclave\":\"nucbox-k11-4\",\"force\":true}" "$HINT" | tr -d '\n' | cut -c1-500) || true
    say "claim hint sent; the relay answered: ${a:-nothing}"
    for i in $(seq 1 30); do
      sleep 20; BOX_TIMEOUT=100 box launch "\$since='$since'"
      if did 'DONE launch running'; then say "KVM Box runs on the NucBox"; answers; exit 0; fi
      if did 'DONE launch refused'; then stop "the node refused the deployment; its line is above"; fi
    done
    stop "no running desktop VM within ten minutes; '$0 launch' reads the state again" ;;
  launch) BOX_TIMEOUT=100 box launch "\$since='${SINCE:-00:00:00}'" ;;
  *) sed -n '2,14p' "$0"; exit 2 ;;
esac

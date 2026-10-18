#!/usr/bin/env bash
# Install EnclaveDNS Local: open .eth sites in the browser through this
# machine's own IPFS (kubo), Swarm (Bee), Arweave (AR.IO) and Tor, with HTTPS
# from a local CA that can only vouch for .eth/.ens names.
#
# Needs the EnclaveDNS resolver setup first (dnscrypt-proxy on 127.0.0.1/.2,
# enclavedns-resolver/setup.sh). Run: sudo bash install.sh
# Undo: sudo bash /etc/dnscrypt-proxy/undo-enclavedns-local.sh
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/../target/release/enclavedns-local"
[ -x "$BIN" ] || { echo "build it first: (cd $HERE/.. && cargo build --release)"; exit 1; }
[ -f /etc/dnscrypt-proxy/lan-forwarding.txt ] || { echo "run the EnclaveDNS resolver setup first"; exit 1; }
ANCHOR=/etc/ca-certificates/trust-source/anchors/enclavedns-local-eth.crt

echo "== 1/5 binary and units"
install -m 0755 "$BIN" /usr/local/bin/enclavedns-local
# this machine's kubo, from the invoking user's config (defaults otherwise)
ARGS=""
U="${SUDO_USER:-}"
if [ -n "$U" ]; then
  CFG="$(getent passwd "$U" | cut -d: -f6)/.ipfs/config"
  if [ -f "$CFG" ]; then
    ARGS="$(python3 - "$CFG" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))["Addresses"]
def url(m):
    m = m[0] if isinstance(m, list) else m
    p = m.split("/")   # /ip4/127.0.0.1/tcp/8081
    return f"http://{p[2]}:{p[4]}" if len(p) >= 5 and p[1] in ("ip4", "ip6") and p[3] == "tcp" else ""
g, a = url(c.get("Gateway", "")), url(c.get("API", ""))
print(" ".join(x for x in [f"--ipfs {g}" if g else "", f"--ipfs-api {a}" if a else ""] if x))
PY
)"
  fi
fi
install -m 0644 "$HERE/enclavedns-local.socket" /etc/systemd/system/enclavedns-local.socket
sed "s|@ARGS@|$ARGS|" "$HERE/enclavedns-local.service" > /etc/systemd/system/enclavedns-local.service
install -m 0755 "$HERE/undo.sh" /etc/dnscrypt-proxy/undo-enclavedns-local.sh
# the resolver's own undo removes this first
R=/etc/dnscrypt-proxy/undo-enclavedns.sh
if [ -f $R ] && ! grep -q undo-enclavedns-local $R; then
  sed -i '0,/^\[ "\$(id -u)" = 0 \].*$/s||&\n[ -x /etc/dnscrypt-proxy/undo-enclavedns-local.sh ] \&\& bash /etc/dnscrypt-proxy/undo-enclavedns-local.sh|' $R
fi
echo "   ExecStart: $(grep ^ExecStart /etc/systemd/system/enclavedns-local.service)"

echo "== 2/5 start it (it makes its .eth CA on first start)"
systemctl daemon-reload
systemctl enable enclavedns-local.socket enclavedns-local.service
systemctl restart enclavedns-local.socket
systemctl restart enclavedns-local.service
for i in $(seq 1 20); do [ -f /var/lib/private/enclavedns-local/ca.crt ] && break; sleep 0.5; done
CA=/var/lib/private/enclavedns-local/ca.crt
[ -f "$CA" ] || { echo "FAILED: no CA was made (journalctl -u enclavedns-local)"; exit 1; }
dig @127.0.0.3 +time=20 +tries=1 playhh.eth A | grep -q "127.0.0.3" || { echo "FAILED: 127.0.0.3 does not answer .eth (journalctl -u enclavedns-local)"; exit 1; }
echo "   127.0.0.3 answers .eth"

echo "== 3/5 trust the CA (name-constrained to .eth and .ens)"
openssl x509 -in "$CA" -noout -ext nameConstraints | grep -q "DNS:eth" || { echo "FAILED: the CA has no .eth name constraint; not trusting it"; exit 1; }
install -m 0644 "$CA" "$ANCHOR"
update-ca-trust
trust list --filter=ca-anchors | grep -q "EnclaveDNS Local CA" && echo "   trusted system-wide (Chromium and Firefox read it through p11-kit)"

echo "== 4/5 send .eth and .ens names from both DNS proxies to it"
F=/etc/dnscrypt-proxy/lan-forwarding.txt
grep -qE '^eth\s' $F || printf 'eth                       127.0.0.3\n' >> $F
grep -qE '^ens\s' $F || printf 'ens                       127.0.0.3\n' >> $F
systemctl restart dnscrypt-proxy.service dnscrypt-proxy-fallback.service
for i in $(seq 1 30); do getent hosts playhh.eth | grep -q 127.0.0.3 && break; sleep 1; done
getent hosts playhh.eth | grep -q 127.0.0.3 || { echo "FAILED: playhh.eth does not resolve to 127.0.0.3 through the system"; exit 1; }
echo "   playhh.eth -> 127.0.0.3 through the system resolver"

echo "== 5/5 check"
curl -s -m 90 -o /dev/null -w "   https://vitalik.eth/ -> HTTP %{http_code} (certificate verified against the system store)\n" https://vitalik.eth/ || true
echo "DONE. Restart Chromium so it loads the new CA, then open playhh.eth"

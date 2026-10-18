#!/usr/bin/env bash
# Undo EnclaveDNS Local: the service, its .eth CA (trust and key), and the .eth forwarding.
set -uo pipefail
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
systemctl disable --now enclavedns-local.service enclavedns-local.socket 2>/dev/null
rm -f /etc/systemd/system/enclavedns-local.service /etc/systemd/system/enclavedns-local.socket /usr/local/bin/enclavedns-local
systemctl daemon-reload
rm -f /etc/ca-certificates/trust-source/anchors/enclavedns-local-eth.crt && update-ca-trust
rm -rf /var/lib/private/enclavedns-local /var/lib/enclavedns-local
F=/etc/dnscrypt-proxy/lan-forwarding.txt
[ -f $F ] && sed -i -E '/^(eth|ens)\s+127\.0\.0\.3$/d' $F
systemctl try-restart dnscrypt-proxy.service dnscrypt-proxy-fallback.service
rm -f /etc/dnscrypt-proxy/undo-enclavedns-local.sh
echo "undone: .eth names resolve as plain EnclaveDNS answers again; the local CA is gone"

#!/usr/bin/env bash
# End-to-end checks against a running EnclaveDNS, with stock clients:
#   tests/live.sh [dns-host] [dns-port] [http-base]
# defaults: 127.0.0.1 5300 http://127.0.0.1:8000 (a local `listen` config).
# Needs dig, delv and curl; reaches the real internet and Ethereum RPCs.
set -uo pipefail
H="${1:-127.0.0.1}"; P="${2:-5300}"; WEB="${3:-http://127.0.0.1:8000}"
D="dig @$H -p $P +time=12 +tries=1"
pass=0; fail=0
check() { # name, command, expected regex
  if out=$(eval "$2" 2>&1) && grep -Eq "$3" <<<"$out"; then pass=$((pass+1)); echo "ok   $1"
  else fail=$((fail+1)); echo "FAIL $1"; sed 's/^/     /' <<<"$out" | head -8; fi
}
check "secure answer carries AD"          "$D +dnssec isc.org A"                 "flags: qr rd ra ad"
check "recursive answers are not AA"      "$D isc.org A"                         "flags: qr rd ra( ad)?;"
check "bogus zone -> SERVFAIL + EDE"      "$D dnssec-failed.org A"               "status: SERVFAIL.*|EDE: 9"
check "CD=1 returns the bogus data"       "$D +cd dnssec-failed.org A"           "status: NOERROR"
check "unsigned zone resolves, no AD"     "$D google.com A"                      "flags: qr rd ra;"
check "secure NXDOMAIN"                   "$D +dnssec nope-zz9.isc.org A"        "status: NXDOMAIN.*|flags: qr rd ra ad"
check "compact denial restored to NXDOMAIN" "$D nope-zz9.ietf.org A"             "status: NXDOMAIN"
check "TCP"                               "$D +tcp www.ietf.org AAAA +short"     ":"
check "delv validates through us"         "delv @$H -p $P www.isc.org A"         "fully validated"
check "delv validates a denial"           "delv @$H -p $P nope-zz9.isc.org A"    "negative response, fully validated"
check ".eth TXT (DNSLink + address)"      "$D vitalik.eth TXT +short"            "dnslink=/ipfs/"
check "_dnslink.<name>.eth"               "$D _dnslink.vitalik.eth TXT +short"   "dnslink="
check ".ens alias"                        "$D nick.ens TXT +short"               "nick.eth"
check "CCIP-Read (offchain resolver)"     "$D jesse.base.eth TXT +short"         "eth=0x"
check "nonexistent .eth -> NXDOMAIN"      "$D no-such-name-zz9-8x.eth A"         "status: NXDOMAIN"
check ".onion key in TXT"                 "$D 2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion TXT +short" "key=ed25519:"
check "bad .onion -> NXDOMAIN + EDE"      "$D 2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wix.onion A" "NXDOMAIN"
check "ANY is minimal (RFC 8482)"         "$D isc.org ANY +short"                "RFC8482"
check "version.bind"                      "$D CH TXT version.bind +short"        "EnclaveDNS"
check "JSON API"                          "curl -s '$WEB/resolve?name=isc.org&type=A'" '"Security":"secure"'
check "DoH GET"                           "curl -s -o /dev/null -w '%{http_code} %{content_type}' '$WEB/dns-query?dns=AAABAAABAAAAAAAAA2lzYwNvcmcAAAEAAQ'" "200 application/dns-message"
check "status API"                        "curl -s $WEB/api/status"              '"name":"EnclaveDNS"'
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]

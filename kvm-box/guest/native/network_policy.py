#!/usr/bin/env python3
"""Generate the measured desktop domain's fixed nftables policy.

This emits text only; the privileged domain launcher installs it in its new
network namespace BEFORE any workload. It never flushes existing tables.
The raw worker channel is a peer-UID checked Unix socket for UID 1001, not TCP.
"""
import argparse


def policy(runtime_uid):
    if type(runtime_uid) is not int or not 5001 <= runtime_uid < 1048576:
        raise ValueError('runtime UID outside the reserved domain range')
    front_uid = runtime_uid + 1048576
    return f'''table inet native_desktop {{
    chain output {{
        type filter hook output priority -10; policy accept;
        # Check BOTH endpoint directions before established-flow handling.
        # A different UID binding a stopped service port cannot impersonate it.
        tcp sport 8000 meta skuid != {runtime_uid} reject
        tcp sport 19080 meta skuid != 1002 reject
        tcp sport 18080 meta skuid != 1003 reject
        tcp sport {{ 6080, 8765 }} meta skuid != 1003 reject
        tcp sport {{ 5900, 47984, 47989, 47990, 48010 }} meta skuid != 1000 reject
        udp sport {{ 47998, 47999, 48000 }} meta skuid != 1000 reject
        tcp sport 1080 meta skuid != {front_uid} reject
        ip saddr 127.0.0.2 meta l4proto {{ tcp, udp }} th sport 53 meta skuid != {front_uid} reject
        ip daddr 127.0.0.3 meta skuid != 1001 reject
        ip saddr 127.0.0.3 meta skuid != 1001 reject
        ip daddr 127.0.0.1 tcp dport 8000 meta skuid {front_uid} accept
        tcp dport 8000 reject
        ip daddr 127.0.0.1 tcp dport 19080 meta skuid {runtime_uid} accept
        tcp dport 19080 reject
        ip daddr 127.0.0.1 tcp dport 18080 meta skuid 1002 accept
        tcp dport 18080 reject
        # Desktop CLI tools may start private HTTP helpers on ephemeral ports.
        # Tag only desktop-initiated loopback TCP, then require the same UID on
        # replies BEFORE established handling; another role cannot impersonate
        # that helper. Fixed control ports and private service IPs stay guarded.
        ct mark 0x4b564d01 meta skuid != 1000 reject
        meta skuid 1000 ip saddr 127.0.0.1 ip daddr 127.0.0.1 meta l4proto tcp ct direction original ct state new ct mark set 0x4b564d01 accept
        ct state established,related accept
        # Trusted native masking service's private llama HTTP helper only.
        meta skuid 1001 ip daddr 127.0.0.3 meta l4proto tcp accept
        # Inner desktop gateway: fixed display, UI and Sunshine destinations.
        meta skuid 1003 ip daddr 127.0.0.1 tcp dport {{ 5900, 6080, 8765, 47984, 47989, 47990, 48010 }} accept
        meta skuid 1003 ip daddr 127.0.0.1 udp dport {{ 47998, 47999, 48000 }} accept
        # Desktop Internet access goes through the measured front's public-web
        # SOCKS/DNS service. No NIC or host route exists in this namespace.
        meta skuid 1000 ip daddr 127.0.0.2 tcp dport 1080 accept
        meta skuid 1000 ip daddr 127.0.0.2 udp dport 53 accept
        meta skuid 1000 ip daddr 127.0.0.2 tcp dport 53 accept
        # Display/session, masking service, gateways and Wasm have no other IP
        # egress. Unix socket peer checks and seccomp mediate the worker route.
        # Only the measured root platform and per-domain front have general
        # egress. Every unlisted UID fails closed, including helper daemons.
        meta skuid {{ 0, {front_uid} }} accept
        reject
    }}
}}
'''


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime-uid', required=True, type=int)
    args = parser.parse_args()
    print(policy(args.runtime_uid), end='')

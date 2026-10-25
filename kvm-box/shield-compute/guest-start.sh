#!/bin/sh
set -eu
# Called as root before starting desktop processes. Failure stops guest boot.
mkdir -p /models /run/shield /run/shield/jobs
/shield/mount-models.sh
mount -t proc -o remount,hidepid=2 proc /proc
nft -f /shield/guest-firewall.nft
chown 1001:1000 /run/shield
chmod 0750 /run/shield
chown 1001:1001 /run/shield/jobs
chmod 0700 /run/shield/jobs
# The service's primary group permits desktop clients to use its local socket.
su -s /bin/sh shield -c 'exec /usr/local/bin/shield-sandbox /usr/bin/python3 -I /shield/service.py --config /shield/service.json' > /run/shield/service.log 2>&1 &
for i in $(seq 1 300); do
    if [ -S /run/shield/compute.sock ]; then
        su -s /bin/sh desktop -c 'exec /usr/local/bin/shield-sandbox /usr/bin/python3 -I /shield/boundary-check.py' > /run/shield/boundary.json
        chmod 0644 /run/shield/boundary.json
        exit 0
    fi
    sleep .1
done
echo 'Shield service did not become ready' >&2
exit 1

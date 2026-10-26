#!/bin/sh
set -eu
busybox hostname kvm-box
busybox ip link set lo up
# Outbound-only user networking. No host filesystem or host display is mounted.
for p in /sys/class/net/*; do
 n=${p##*/}; [ "$n" = lo ] && continue
 busybox ip link set "$n" up
 busybox ip addr add 10.0.2.15/24 dev "$n"
 busybox ip route add default via 10.0.2.2
 break
done
printf 'nameserver 10.0.2.3\n' > /etc/resolv.conf
if [ -d /shield ]; then /shield/guest-start.sh; fi
mkdir -p /run/dbus /run/user/1000 /home/desktop/.fluxbox
chown 1000:1000 /run/user/1000
chmod 0700 /run/user/1000
# These are guest-only virtual input devices, never the host's devices.
/usr/lib/systemd/systemd-udevd --daemon
udevadm trigger
udevadm settle
chmod 0660 /dev/uhid
chown 1000:1000 /dev/uhid
chmod 0660 /dev/uinput
chown 1000:1000 /dev/uinput
busybox head -c 32 /dev/urandom | busybox base64 > /run/sunshine-password
chmod 0600 /run/sunshine-password
HOME=/home/desktop sunshine /sunshine.conf --creds owner "$(cat /run/sunshine-password)" > /tmp/sunshine-setup.log 2>&1
chown -R 1000:1000 /home/desktop
chown 1000:1000 /tmp/sunshine-internal.log
chmod 0700 /home/desktop
dbus-uuidgen > /etc/machine-id
nice -n -5 Xorg :0 -config /xorg.conf -noreset -nolisten tcp -novtswitch -sharevts -ac > /tmp/xvfb.log 2>&1 &
for i in $(seq 1 100); do DISPLAY=:0 xdpyinfo >/dev/null 2>&1 && break; sleep .1; done
if [ -d /shield ]; then
 su -s /bin/sh desktop -c 'export DISPLAY=:0 HOME=/home/desktop; exec /usr/local/bin/shield-sandbox dbus-run-session -- /desktop-session.sh' > /tmp/session.log 2>&1 &
else
 su -s /bin/sh desktop -c 'export DISPLAY=:0 HOME=/home/desktop; exec dbus-run-session -- /desktop-session.sh' > /tmp/session.log 2>&1 &
fi
x11vnc -display :0 -localhost -forever -shared -nopw -rfbport 5900 -quiet > /tmp/vnc.log 2>&1 &
websockify --web=/usr/share/novnc 127.0.0.1:6080 127.0.0.1:5900 > /tmp/websockify.log 2>&1 &
node /compute/compute.mjs > /tmp/compute.log 2>&1 &
/helper gateway &
# Native desktop image identity is bound by its measured image and app.sha256.
exec /front -snp -port 443 -upstream 127.0.0.1:8000 -app-mode run -runtime-identity /no-wasm-runtime.json

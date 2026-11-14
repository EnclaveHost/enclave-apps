#!/bin/sh
# Runs inside the sandbox: its own loopback, the guest image's root read-only.
set -u
export HOME=/home/desktop PATH=/usr/bin:/bin LANG=C.UTF-8
umask 077
mkdir -p /home/desktop/.config/sunshine
cred=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
printf '%s' "$cred" > /run/cred
/usr/bin/sunshine /sunshine.conf --creds owner "$cred" > /run/creds.log 2>&1
echo "creds exit $?" > /media/run.log
/usr/bin/Xvfb :0 -screen 0 1280x720x24 -nolisten tcp > /media/xvfb.log 2>&1 &
xv=$!
n=0; while [ ! -S /tmp/.X11-unix/X0 ] && [ $n -lt 100 ]; do n=$((n+1)); /usr/bin/sleep 0.1; done
export DISPLAY=:0
/usr/bin/sunshine /sunshine.conf > /media/sunshine.log 2>&1 &
sun=$!
KVMBOX_LIVE_SOCKET_DIR=/run/auto KVMBOX_LIVE_STATE=/home/desktop/.config/sunshine/sunshine_state.json \
  KVMBOX_LIVE_STOP=/run/stop /mnt/automation.test -test.run 'TestLiveServePaired$' -test.v > /media/automation.log 2>&1 &
auto=$!
KVMBOX_LIVE_CREDENTIAL=/run/cred KVMBOX_LIVE_SOCKET_DIR=/run/auto \
  /mnt/inner.test -test.run 'TestLiveSunshinePairAgain$' -test.v -test.timeout 240s > /media/inner.log 2>&1
echo "inner exit $?" >> /media/run.log
: > /run/stop
wait $auto
echo "automation exit $?" >> /media/run.log
kill $sun $xv 2>/dev/null
cp /tmp/sunshine-internal.log /media/sunshine-internal.log 2>/dev/null
echo done >> /media/run.log

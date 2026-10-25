#!/bin/sh
set -eu
export XDG_RUNTIME_DIR=/run/user/1000
pulseaudio --start --exit-idle-time=-1
pactl load-module module-null-sink sink_name=kvm_box sink_properties=device.description=KVM_Box >/dev/null
pactl set-default-sink kvm_box
sunshine /sunshine.conf > /tmp/sunshine.log 2>&1 &
xset s off
xset s noblank
xset -dpms || true
xsetroot -solid '#1a1a2e'
# Firefox and Doom start on demand from the original lightweight desktop menu.
exec fluxbox

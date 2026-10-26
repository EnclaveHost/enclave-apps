#!/bin/sh
set -eu
busybox ip link set lo up
/helper bridge &
/rt/ld-linux-x86-64.so.2 --library-path /rt /rt/wasmtime run -S cli -S tcp -S inherit-network -C cache=n --env "KVMBOX_ACCESS_SHA256=$(cat /access.sha256)" /app.wasm &
exec /front -snp -port 443 -upstream 127.0.0.1:8000 -app-mode run

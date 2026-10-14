#!/bin/bash
# Write this directory's files/ (plus Alpine's libjemalloc) into an ext2 RISC Box image, and drop Firefox's
# CRLite filters from the profile. Usage: apply-overlay.sh IN.ext2 OUT.ext2 [libjemalloc.so.2]
# IN is rootfs-badwolf-20261002 with the Firefox ESR JIT build installed (see README.md). Needs debugfs, e2fsck.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
in=$1 out=$2 jemalloc=${3:-}
P=/root/.mozilla/firefox/2xnqbvgz.default-release
T=0x6ac8b451 # fixed mtime: the same overlay gives the same image bytes
cp --sparse=always "$in" "$out"
dfs() { debugfs -w -f - "$out" >/dev/null; }
mkd() { # parent name
  printf 'cd %s\nmkdir %s\nsif %s uid 0\nsif %s gid 0\nsif %s mode 040755\nsif %s mtime %s\n' "$1" "$2" "$2" "$2" "$2" "$2" "$T" | dfs
}
put() { # local dest-dir name mode
  printf 'cd %s\nrm %s\nwrite %s %s\nsif %s uid 0\nsif %s gid 0\nsif %s mode %s\nsif %s mtime %s\nsif %s ctime %s\nsif %s atime %s\n' \
    "$2" "$3" "$1" "$3" "$3" "$3" "$3" "$4" "$3" "$T" "$3" "$T" "$3" "$T" | dfs
}
# directories the base image may lack (mkdir of an existing one is a no-op error debugfs ignores)
mkd /root .themes; mkd /root/.themes RiscBox; mkd /root/.themes/RiscBox gtk-3.0
mkd "$P" chrome; mkd /usr/lib/firefox-esr distribution
cd "$here/files"
find . -type f | sort | while read -r f; do
  rel=${f#.}
  mode=0100644; [ -x "$f" ] && mode=0100755
  put "$here/files$rel" "$(dirname "$rel")" "$(basename "$rel")" "$mode"
done
[ -n "$jemalloc" ] && put "$jemalloc" /usr/lib libjemalloc.so.2 0100755
# CRLite: ~22 MB of filters decoded at every Firefox start although CRLite is off in user.js (~25 s cold)
debugfs -R "ls $P/security_state" "$out" 2>/dev/null | tr ' ' '\n' | grep -E '\.filter(\.delta)?$' \
  | sed "s|^|rm $P/security_state/|" | dfs || true
e2fsck -fn "$out" | tail -1

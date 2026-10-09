#!/usr/bin/env bash
# Mirror a repository's branches and tags into depot (from GitHub, or any
# git URL or local path). Safe to re-run: each run sends only what is new.
#
#   DEPOT_TOKEN=dpt_… scripts/mirror.sh <source> https://<depot-host>/<repo>.git [--prune]
#
# - Only refs/heads/* and refs/tags/* move (a GitHub clone's refs/pull/* and
#   anything else stay behind). --prune also deletes branches and tags that
#   no longer exist at the source (protected refs refuse, by design).
# - A repository whose pack would exceed the server's push limit
#   (DEPOT_PUSH_LIMIT_MB, default 900) first goes up in slices of its default
#   branch's first-parent history, so no single push is too large.
# - The token travels in X-Api-Key (the Enclave app gateway removes
#   Authorization). It is passed through git's environment config, so it does
#   not appear in the process list.
set -euo pipefail

if [ $# -lt 2 ]; then
  sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi
src=$1
dest=$2
prune=()
[ "${3:-}" = "--prune" ] && prune=(--prune)
: "${DEPOT_TOKEN:?set DEPOT_TOKEN to a token that may write the destination}"
limit_mb=${DEPOT_PUSH_LIMIT_MB:-900}

export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader GIT_CONFIG_VALUE_0="X-Api-Key: $DEPOT_TOKEN"
export GIT_TERMINAL_PROMPT=0

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
echo "== fetching $src"
git clone --quiet --bare "$src" "$work/src.git"
cd "$work/src.git"

size_mb=$(( $(git count-objects -v | awk '/size-pack/ {print $2}') / 1024 ))
head=$(git symbolic-ref --short HEAD 2>/dev/null || echo main)
echo "== $(git for-each-ref refs/heads refs/tags | wc -l) refs, ${size_mb} MiB packed, default branch $head"

if [ "$size_mb" -gt "$limit_mb" ] && git rev-parse -q --verify "refs/heads/$head" >/dev/null; then
  total=$(git rev-list --first-parent --count "$head")
  slices=$(( size_mb / limit_mb + 2 ))
  step=$(( total / slices + 1 ))
  echo "== larger than ${limit_mb} MiB: sending $head in about $slices slices of $step commits"
  git rev-list --first-parent --reverse "$head" | awk -v s="$step" 'NR % s == 0' | while read -r c; do
    echo "-- $head at $c"
    git push --quiet "$dest" "$c:refs/heads/$head"
  done
fi

echo "== pushing branches and tags to $dest"
git push "${prune[@]}" "$dest" 'refs/heads/*:refs/heads/*' 'refs/tags/*:refs/tags/*'
echo "== verifying"
want=$(git for-each-ref --format='%(objectname) %(refname)' refs/heads refs/tags | sort)
have=$(git ls-remote "$dest" 'refs/heads/*' 'refs/tags/*' | grep -v '\^{}$' | awk '{print $1" "$2}' | sort)
if [ "$want" = "$have" ]; then
  echo "== in sync: $(echo "$want" | wc -l) refs match"
else
  diff <(echo "$want") <(echo "$have") | head -20
  echo "!! refs differ (protected refs refuse non-fast-forward updates and deletes)" >&2
  exit 1
fi

#!/bin/sh
# Holds this directory against a checkout of upstream at the commit of UPSTREAM (PATCHES.md).
#
#   upstream_diff.sh <checkout of rav1d> [<file for the diff>]
#
# Every file here is one of three kinds, and the script fails on a file that is none of them:
#   - a file of upstream, byte for byte;
#   - a file of upstream that rav1g changed: one of CHANGED below, which is the list of PATCHES.md;
#   - a file of rav1g: one of ADDED below.
# The diff of the changed files against upstream (what a reviewer or an update has to look at) goes to
# the file named second, or is counted only.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
upstream=${1:?usage: upstream_diff.sh <checkout of rav1d> [<file for the diff>]}
out=${2:-}

CHANGED="Cargo.toml build.rs src/cpu.rs src/decode.rs src/internal.rs src/lib.rs src/msac.rs src/obu.rs src/picture.rs
src/recon.rs src/refmvs.rs src/tables.rs src/thread_task.rs"
ADDED="PATCHES.md UPSTREAM upstream_diff.sh src/parse.rs"

commit=$(awk '$1 == "commit" { print $2 }' "$here/UPSTREAM")
head=$(git -C "$upstream" rev-parse HEAD)
if [ "$head" != "$commit" ]; then
  echo "FAIL: $upstream is at $head, UPSTREAM names $commit"
  exit 1
fi

is_in() {
  for name in $2; do
    [ "$name" = "$1" ] && return 0
  done
  return 1
}

status=0
same=0
changed=0
added=0
diff_file=$(mktemp)
trap 'rm -f "$diff_file"' EXIT
for file in $(cd "$here" && find . -type f | sed 's|^\./||' | sort); do
  if is_in "$file" "$ADDED"; then
    if [ -e "$upstream/$file" ]; then
      echo "FAIL: $file is listed as a file of rav1g, and upstream has one of that name"
      status=1
    fi
    added=$((added + 1))
  elif [ ! -f "$upstream/$file" ]; then
    echo "FAIL: $file is neither a file of upstream nor listed as a file of rav1g"
    status=1
  elif cmp -s "$here/$file" "$upstream/$file"; then
    if is_in "$file" "$CHANGED"; then
      echo "FAIL: $file is listed as changed and is the file of upstream"
      status=1
    fi
    same=$((same + 1))
  elif is_in "$file" "$CHANGED"; then
    changed=$((changed + 1))
    diff -u --label "upstream/$file" --label "rav1g/$file" "$upstream/$file" "$here/$file" >> "$diff_file" || true
  else
    echo "FAIL: $file differs from upstream and PATCHES.md (CHANGED of this script) does not list it"
    status=1
  fi
done

plus=$(grep -c '^+[^+]' "$diff_file" || true)
minus=$(grep -c '^-[^-]' "$diff_file" || true)
upstream_files=$(git -C "$upstream" ls-files | wc -l)
echo "rav1d at $commit: $same files as upstream, $changed changed (+$plus -$minus lines), $added of rav1g;" \
  "upstream has $upstream_files files"
if [ -n "$out" ]; then
  cp "$diff_file" "$out"
fi
if [ "$status" = 0 ]; then
  echo "PASS: the directory is upstream at the commit of UPSTREAM with the changes PATCHES.md lists"
fi
exit "$status"

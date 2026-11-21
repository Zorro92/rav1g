#!/bin/sh
# Compare direct NV12/P010 copy with a per-pixel CPU reference on a Linux host.
# Pass --mutate to also verify that deliberate errors produce pixel mismatches.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
shaders="$here/../../libav1/dx/shaders"
sync="$here/../sync_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

python3 "$sync/to_cpp.py" "$shaders/output_sp.h" "$work/output_sp.gen.h"
name=copy_plane_sp_tex_v2
python3 "$sync/to_cpp.py" "$shaders/$name.hlsl" "$work/$name.gen.h" "$work/$name.entry.h"

build() {
  "${CXX:-c++}" -std=c++17 -O2 -fwrapv -I "$sync" -I "$work" "$here/copy_check.cpp" -o "$work/check"
}
build
timeout 120 "$work/check"

expect_rejected() {
  shader="$work/copy_plane_sp_tex_v2.gen.h"
  cp "$shader" "$shader.original"
  sed -i "$2" "$shader"
  build
  status=0
  timeout 120 "$work/check" > "$work/mutant.log" 2>&1 || status=$?
  mv "$shader.original" "$shader"
  if [ "$status" != 1 ] || ! grep -q '^MISMATCH ' "$work/mutant.log"; then
    cat "$work/mutant.log" >&2
    echo "copy_check: expected a mismatch for $1" >&2
    exit 1
  fi
  echo "copy_check: rejected $1"
}

if [ "${1:-}" = "--mutate" ]; then
  expect_rejected 'missing odd column replication' 's/a.y = a.x/a.y = a.z/'
  expect_rejected 'missing odd row replication' 's/y == height - 1/y == height - 2/'
  expect_rejected 'P010 shift' 's/cb_hbd ? 6 : 0/cb_hbd ? 5 : 0/'
  expect_rejected 'UV component order' 's/uint2(u\[i\], v\[i\])/uint2(v[i], u[i])/'
  expect_rejected 'UV tile mapping' 's/(group.x \& 1)/(group.x \& 0)/'
  expect_rejected 'monochrome fill' 's/cb_hbd ? 512 : 128/cb_hbd ? 513 : 129/'
fi

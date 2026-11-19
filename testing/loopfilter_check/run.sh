#!/bin/sh
# Compare the new and legacy HLSL on the CPU. Run with --mutate to exercise negative controls.
# TMPDIR controls where the translated sources and objects go. Requires a Linux C++20 compiler.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
shaders="$here/../../libav1/dx/shaders"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
cxx=${CXX:-c++}

translate() {
  sed -E \
    -e 's/\[numthreads\([^]]*\)\]//' \
    -e 's/\[(unroll|loop|branch)\]//g' \
    -e 's/: *SV_[A-Za-z]+//g' \
    -e 's/: *register\([a-z0-9]+\)//g' \
    -e 's/^cbuffer +[A-Za-z_0-9]+ *\{/namespace {/' \
    -e 's/void main\(/void shader_main(/' \
    -e 's/pixels\.yz/uint2(pixels.y, pixels.z)/g' \
    -f "$here/../common/gs_groupshared.sed" "$1"
}

build() {
  "$cxx" -std=c++20 -O2 -fwrapv -I"$work" -I"$here" -c \
    "-DSHADER_SOURCE=\"$1.hlsl\"" "-DENTRY=$1" "$here/shader_tu.cpp" -o "$work/$1.o"
}

link_and_run() {
  "$cxx" "$work"/*.o -o "$work/check"
  timeout 60 "$work/check"
}

for source in "$shaders"/loopfilter_*.h "$shaders"/loopfilter_vert_hbd*.hlsl; do
  translate "$source" > "$work/${source##*/}"
done
"$cxx" -std=c++20 -O2 -fwrapv -c "$here/loopfilter_check.cpp" -o "$work/check.o"
build loopfilter_vert_hbd_legacy
build loopfilter_vert_hbd
link_and_run

if [ "${1:-}" = "--mutate" ]; then
  sed -i 's/remaining &= remaining - 1;/remaining \&= remaining - 1; remaining \&= remaining - 1;/' "$work/loopfilter_vert_main.h"
  build loopfilter_vert_hbd
  if link_and_run > "$work/mutant.log" 2>&1 || ! grep -q '^FAIL: vertical' "$work/mutant.log"; then
    cat "$work/mutant.log" >&2
    echo "The vertical check failed to reject a skipped edge" >&2
    exit 1
  fi
  cat "$work/mutant.log"
  echo "PASS: rejected the skipped-edge mutation"
fi

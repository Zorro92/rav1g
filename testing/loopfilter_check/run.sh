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
    -e 's/uint inter_tx_size\[4\]\)/const uint inter_tx_size[4])/' \
    -e 's/pixels\.yz/uint2(pixels.y, pixels.z)/g' \
    -f "$here/../common/gs_groupshared.sed" "$1"
}

build() {
  name="$1"
  shift
  "$cxx" "$@" -std=c++20 -O2 -fwrapv -I"$work" -I"$here" -c \
    "-DSHADER_SOURCE=\"$name.hlsl\"" "-DENTRY=$name" "$here/shader_tu.cpp" -o "$work/$name.o"
}

link_and_run() {
  "$cxx" "$work"/*.o -o "$work/check"
  timeout 60 "$work/check"
}

{ printf '#pragma once\n'; cat "$shaders/mode_info.h"; } > "$work/mode_info.h"
for source in "$shaders"/loopfilter_*.h "$shaders"/loopfilter_vert_hbd*.hlsl "$shaders"/gen_lf_blocks*; do
  translate "$source" > "$work/${source##*/}"
done
"$cxx" -std=c++20 -O2 -fwrapv -I"$work" -c "$here/loopfilter_check.cpp" -o "$work/check.o"
for direction in vert hor; do
  build "gen_lf_blocks_$direction" -DGEN_SHADER
  build "gen_lf_blocks_${direction}_legacy" -DGEN_SHADER
done
build loopfilter_vert_hbd_legacy
build loopfilter_vert_hbd
link_and_run

if [ "${1:-}" = "--mutate" ]; then
  cp "$work/gen_lf_blocks.h" "$work/gen_lf_blocks.ref"
  sed -i 's/previous = preceding;/previous = current;/' "$work/gen_lf_blocks.h"
  build gen_lf_blocks_vert -DGEN_SHADER
  if link_and_run > "$work/mutant.log" 2>&1 || ! grep -q '^FAIL: gen_lf_blocks_vert' "$work/mutant.log"; then
    cat "$work/mutant.log" >&2
    echo "The generator check failed to reject incorrect preceding mode info" >&2
    exit 1
  fi
  cat "$work/mutant.log"
  echo "PASS: rejected the preceding-MI mutation"
  mv "$work/gen_lf_blocks.ref" "$work/gen_lf_blocks.h"
  build gen_lf_blocks_vert -DGEN_SHADER

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

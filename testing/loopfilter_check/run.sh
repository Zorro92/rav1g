#!/bin/sh
# Compare the deblocking shaders with scalar AV1 models. Run with --mutate for negative controls.
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
    -e 's/inout ([A-Za-z_0-9]+) /\1 \&/g' \
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
for source in "$shaders"/loopfilter_*.h "$shaders"/loopfilter_*.hlsl "$shaders"/gen_lf_blocks*; do
  translate "$source" > "$work/${source##*/}"
done
"$cxx" -std=c++20 -O2 -fwrapv -I"$work" -c "$here/loopfilter_check.cpp" -o "$work/check.o"
for direction in vert hor; do
  build "gen_lf_blocks_$direction" -DGEN_SHADER
  for depth in '' _hbd; do
    build "loopfilter_${direction}${depth}"
  done
done
link_and_run

expect_rejected() {
  description="$1"
  mismatch="$2"
  if link_and_run > "$work/mutant.log" 2>&1 || ! grep -m 1 "^FAIL: $mismatch" "$work/mutant.log"; then
    cat "$work/mutant.log" >&2
    echo "The check failed to reject the $description mutation" >&2
    exit 1
  fi
  echo "PASS: rejected the $description mutation"
}

if [ "${1:-}" = "--mutate" ]; then
  for axis in vert hor; do
    source="$work/loopfilter_${axis}_main.h"
    cp "$source" "$source.ref"
    sed -i 's/GroupMemoryBarrierWithGroupSync();/GroupMemoryBarrier();/' "$source"
    build "loopfilter_$axis"
    expect_rejected "$axis group sync" 'group scheduling or shared memory'
    mv "$source.ref" "$source"
    build "loopfilter_$axis"
  done

  cp "$work/gen_lf_blocks.h" "$work/gen_lf_blocks.ref"
  sed -i 's/previous = preceding;/previous = current;/' "$work/gen_lf_blocks.h"
  build gen_lf_blocks_vert -DGEN_SHADER
  expect_rejected "preceding-MI" gen_lf_blocks_vert
  mv "$work/gen_lf_blocks.ref" "$work/gen_lf_blocks.h"
  build gen_lf_blocks_vert -DGEN_SHADER

  cp "$work/loopfilter_hor_main.h" "$work/loopfilter_hor_main.ref"
  sed -i 's/dst_frame.Store(offset + stride, out_q0.y);/dst_frame.Store(offset + stride, out_q0.x);/' \
    "$work/loopfilter_hor_main.h"
  build loopfilter_hor
  expect_rejected "horizontal row-pack" 'horizontal (8-bit)'
  mv "$work/loopfilter_hor_main.ref" "$work/loopfilter_hor_main.h"
  build loopfilter_hor

  cp "$work/loopfilter_vert_main.h" "$work/loopfilter_vert_main.ref"
  sed -i 's/remaining &= remaining - 1;/remaining \&= remaining - 1; remaining \&= remaining - 1;/' "$work/loopfilter_vert_main.h"
  build loopfilter_vert_hbd
  expect_rejected "skipped-edge" 'vertical .* (10-bit)'
  mv "$work/loopfilter_vert_main.ref" "$work/loopfilter_vert_main.h"

  # Model the console substitution documented in the high-bit-depth vertical shader.
  sed -i 's/abs(q0.x - q0.w) <= Thresh10/abs(q0.y - q0.z) <= Thresh10/' "$work/loopfilter_vert_hbd.hlsl"
  build loopfilter_vert_hbd
  expect_rejected "crossed-extraction" 'vertical .* (10-bit)'
fi

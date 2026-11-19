#!/bin/sh
# Compare the original and optimized direct-texture film grain HLSL on a Linux host.
# Requires a C++17 compiler and python3. Pass --mutate to check that deliberate errors are rejected.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
shaders="$here/../../libav1/dx/shaders"
sync="$here/../sync_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

for header in film_grain_const film_grain_apply output_sp; do
  python3 "$sync/to_cpp.py" "$shaders/$header.h" "$work/$header.gen.h"
done
# The two apply shaders use the same layout types. The resource declarations and entry points
# are included in separate namespaces.
for header in film_grain_const film_grain_apply; do
  sed -i '1i #pragma once' "$work/$header.gen.h"
done
for shader in film_grain_filter film_grain_filter_sp_tex_v2 copy_plane_sp; do
  python3 "$sync/to_cpp.py" "$shaders/$shader.hlsl" "$work/$shader.gen.h" "$work/$shader.entry.h"
done

build() {
  "${CXX:-c++}" -std=c++17 -O2 -fwrapv -Wall -Wextra -Wno-unused-function -Wno-unused-parameter \
    -I "$work" -I "$sync" "$here/film_grain_check.cpp" -o "$work/check"
}
build
timeout 120 "$work/check"

expect_rejected() {
  description="$1"
  edit="$2"
  shader="$work/film_grain_filter_sp_tex_v2.gen.h"
  cp "$shader" "$shader.original"
  sed -i "$edit" "$shader"
  build
  if timeout 120 "$work/check" > "$work/mutant.log" 2>&1 || ! grep -m 1 '^FAIL: pixel mismatch' "$work/mutant.log"; then
    cat "$work/mutant.log" >&2
    echo "The check failed to reject the $description mutation" >&2
    exit 1
  fi
  mv "$shader.original" "$shader"
  echo "PASS: rejected the $description mutation"
}

if [ "${1:-}" = "--mutate" ]; then
  expect_rejected "upper-left blend" 's/blend_luma_x(above_left, above.xy, center)/blend_luma_x(above.xy, above_left, center)/'
  expect_rejected "grained luma for chroma scaling" 's/int2(input.x + input.y + 1, input.z + input.w + 1)/int2(output.x + output.y + 1, output.z + output.w + 1)/'
  expect_rejected "restricted luma range" 's/max_luma_legal_range << bit_shift/max_chroma_legal_range << bit_shift/'
  expect_rejected "missing last column" 's/pixel.x >= data.width/pixel.x >= data.width - 1/'
fi

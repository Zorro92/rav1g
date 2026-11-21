#!/bin/sh
# Compare the small inverse transform shaders with a scalar AV1 reference on Linux.
# Pass --mutate to also verify that deliberate errors produce mismatches.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
libav1="$here/../../libav1"
shaders="$libav1/dx/shaders"
sync="$here/../sync_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

for name in idct_shader_common idct_small; do
  python3 "$sync/to_cpp.py" "$shaders/$name.h" "$work/$name.gen.h"
done

build() {
  name="idct$1"
  "${CXX:-c++}" -std=c++20 -O2 -fwrapv -I "$sync" -I "$work" \
    -DCANDIDATE="\"${name}_v2.gen.h\"" -DCANDIDATE_ENTRY="\"${name}_v2.entry.h\"" \
    -DSCANS="\"$name.scans.h\"" "$here/idct_check.cpp" -o "$work/check"
}

for size in 4x4 4x8 8x4 8x8; do
  name="idct${size}_v2"
  python3 "$sync/to_cpp.py" "$shaders/$name.hlsl" "$work/$name.gen.h" "$work/$name.entry.h"
  # Use the AV1 scan tables from libaom.
  sed -nE "/^DECLARE_ALIGNED\(16, static const int16_t, (default|mrow|mcol)_scan_$size\[/,/^};/ {
    s/^DECLARE_ALIGNED\(16, static const int16_t, (.*)\)/static const int \1/
    p
  }" "$libav1/av1/common/scan.c" > "$work/idct$size.scans.h"
  printf 'static const int *const kScans[] = {default_scan_%s, mrow_scan_%s, mcol_scan_%s};\n' "$size" "$size" "$size" \
    >> "$work/idct$size.scans.h"
  build "$size"
  timeout 120 "$work/check"
done

expect_rejected() {
  shader="$work/idct_small.gen.h"
  cp "$shader" "$shader.original"
  sed -i "$3" "$shader"
  build "$1"
  status=0
  timeout 120 "$work/check" > "$work/mutant.log" 2>&1 || status=$?
  mv "$shader.original" "$shader"
  if [ "$status" != 1 ] || ! grep -q "$4" "$work/mutant.log"; then
    cat "$work/mutant.log" >&2
    echo "idct_check: expected a mismatch for $2" >&2
    exit 1
  fi
  echo "idct_check: rejected $2"
}

if [ "${1:-}" = "--mutate" ]; then
  expect_rejected 4x4 'default scan' 's/DefaultScan\[i\]/DefaultScan[i ^ 1]/' '^MISMATCH residual'
  expect_rejected 4x8 'row/column scan' 's/scan == 1/scan == 2/' '^MISMATCH residual'
  expect_rejected 8x4 'rectangular scaling' 's/value \* NewInvSqrt2/value * NewSqrt2/' '^MISMATCH residual'
  expect_rejected 8x8 'horizontal shift' 's/TX_HEIGHT == 8 ? 1 : 0/TX_HEIGHT == 8 ? 0 : 1/' '^MISMATCH residual'
  expect_rejected 8x8 'vertical clamp' 's/int2(ClampMinValue16, ClampMaxValue16)/int2(-256, 255)/' '^MISMATCH residual'
  expect_rejected 8x4 'horizontal flip' 's/(block.x \& IdctType_H_Flip) != 0/false/' '^MISMATCH residual'
  expect_rejected 4x8 'vertical flip' 's/(block.x \& IdctType_V_Flip) != 0/false/' '^MISMATCH residual'
  expect_rejected 4x4 'missing coefficient clear' '/buf_input.Store4(address/d' '^MISMATCH coefficient'
fi

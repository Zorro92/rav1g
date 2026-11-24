#!/bin/sh
# display_pass.hlsl as C++ on test frames (display_check.cpp), each display image held against the
# float32 model of the pass (model.py, numpy). Runs on a Linux host: a C++17 compiler, python3 with numpy
# (or uv, which fetches it), git. Pass --mutate to check that deliberate errors are rejected.
#
# The identity check: the same program built on the shader of REFERENCE (below, read from git) runs the
# cases and the identity matrix (display_check --matrix), and every display image of this shader has to be
# the reference's byte for byte. A change that only makes the pass faster keeps the pin; a change that means
# to alter the image moves it to the commit that makes it (and says why there). A shallow clone fetches the
# tag first (git fetch --depth 1 origin tag <REFERENCE>).
set -eu

# The release whose display images the pass keeps: v0.8.0-alpha.1 (interface version 3, the Catmull-Rom
# input chroma).
REFERENCE=v0.8.0-alpha.1

here=$(cd "$(dirname "$0")" && pwd)
shaders="$here/../../libav1/dx/shaders"
sync="$here/../sync_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

if python3 -c 'import numpy' 2> /dev/null; then
  py() { python3 "$@"; }
elif command -v uv > /dev/null 2>&1; then
  py() { uv run --quiet --with numpy python3 "$@"; }
else
  echo "display_pass check: SKIP: no numpy for python3 and no uv" >&2
  exit 0
fi

python3 "$sync/to_cpp.py" "$shaders/output_sp.h" "$work/output_sp.gen.h"
python3 "$sync/to_cpp.py" "$shaders/display_pass.hlsl" "$work/display_pass.gen.h" "$work/display_pass.entry.h"

# build [<dir of the generated headers> <program>]: this shader by default.
build() {
  "${CXX:-c++}" -std=c++17 -O2 -fwrapv -DDISPLAY_MODE_FROM_FLAGS -ffp-contract=off -Wall -Wno-unused-function -Wno-unused-variable \
    -I "$here" -I "$sync" -I "${1:-$work}" "$here/display_check.cpp" -o "${2:-$work/check}"
}

# Runs the shader and the model on every case; exit 1 when a case fails either (with "first", at the first
# that fails: a mutant needs one). At most 2% of a plane's codes may differ (most cases: none, a few tenths
# of a percent at most), which a dither that is not there (half of the codes one off) does not pass.
check() {
  mkdir -p "$work/out"
  timeout 600 "$work/check" "$work/out" > "$work/cases.txt" || return 1
  status=0
  while read -r name frames display args; do
    # shellcheck disable=SC2086
    if ! py "$here/model.py" "$frames" "$display" $args --max-differ 0.02 > "$work/model.log" 2>&1; then
      echo "MISMATCH $name"
      tail -n 3 "$work/model.log"
      status=1
      [ "${1:-}" = first ] && break
    else
      echo "$name: $(sed -n "s/^frame 0: //p" "$work/model.log"); $(tail -n 1 "$work/model.log")"
    fi
  done < "$work/cases.txt"
  return "$status"
}

# The model's own rules first: the source peak from the HDR metadata, the chroma filter's weights.
py "$here/model.py" --self-test
build
check

# The identity check against the reference shader (above): the cases and the identity matrix.
repo=$(cd "$here/../.." && pwd)
mkdir -p "$work/ref"
for f in display_pass.hlsl output_sp.h; do
  if ! git -C "$repo" show "$REFERENCE:libav1/dx/shaders/$f" > "$work/ref/$f" 2> /dev/null; then
    echo "display_pass check: the reference $REFERENCE is not in this clone (git fetch --depth 1 origin tag $REFERENCE)" >&2
    exit 1
  fi
done
python3 "$sync/to_cpp.py" "$work/ref/output_sp.h" "$work/ref/output_sp.gen.h"
python3 "$sync/to_cpp.py" "$work/ref/display_pass.hlsl" "$work/ref/display_pass.gen.h" "$work/ref/display_pass.entry.h"
build "$work/ref" "$work/check_ref"
for side in new ref; do
  program="$work/check"
  [ "$side" = ref ] && program="$work/check_ref"
  mkdir -p "$work/id-$side"
  timeout 600 "$program" "$work/id-$side" > "$work/id-$side/cases.txt"
  timeout 600 "$program" "$work/id-$side" --matrix >> "$work/id-$side/cases.txt"
done
compared=0
while read -r name frames display args; do
  for f in "$frames" "$display"; do
    if ! cmp -s "$f" "$work/id-ref/${f#"$work/id-new/"}"; then
      echo "display_pass check: $name: $(basename "$f") differs from the shader of $REFERENCE" >&2
      exit 1
    fi
  done
  compared=$((compared + 1))
done < "$work/id-new/cases.txt"
[ "$(wc -l < "$work/id-ref/cases.txt")" -eq "$compared" ] || { echo "display_pass check: the case lists differ" >&2; exit 1; }
echo "display_pass check: $compared cases, every display image identical to the shader of $REFERENCE"

expect_rejected() {
  shader="$work/display_pass.gen.h"
  cp "$shader" "$shader.original"
  sed -i "$2" "$shader"
  if cmp -s "$shader" "$shader.original"; then
    echo "display_pass check: the mutation for $1 did not apply" >&2
    exit 1
  fi
  build
  if check first > "$work/mutant.log" 2>&1; then
    cat "$work/mutant.log" >&2
    echo "display_pass check: expected a mismatch for $1" >&2
    exit 1
  fi
  mv "$shader.original" "$shader"
  echo "display_pass check: rejected $1"
}

if [ "${1:-}" = "--mutate" ]; then
  expect_rejected 'chroma siting of the quad path' 's/const float4 w = (k \& 1u) != 0 ? f.odd : f.even;/const float4 w = (k \& 1u) != 0 ? f.even : f.odd;/'
  expect_rejected 'chroma siting of the area path' 's/const float4 w = (p.y \& 1) != 0 ? f.odd : f.even;/const float4 w = f.odd;/'
  expect_rejected 'chroma siting of the block paths' 's/const float4 w = (y \& 1) != 0 ? f.odd : f.even;/const float4 w = f.even;/'
  expect_rejected 'the taps of a row' 's/const int r = ((y + 1) >> 1) - 2;  \/\/ the row/const int r = (y >> 1) - 2;  \/\/ the row/'
  expect_rejected 'the taps of a row of the block paths' 's/const int r0 = ((y + 1) >> 1) - 2;  \/\/ the row/const int r0 = (y >> 1) - 2;  \/\/ the row/'
  expect_rejected 'the taps of a row of the area path' 's/const int r0 = ((p.y + 1) >> 1) - 2;/const int r0 = (p.y >> 1) - 1;/'
  expect_rejected 'the fifth chroma column' 's/dot4(f.h, float4(r.s.y, r.s.z, r.s.w, r.t))/dot4(f.h, float4(r.s.y, r.s.z, r.s.w, r.s.w))/'
  expect_rejected 'the Catmull-Rom weights across' 's/float4(-0.0625f, 0.5625f, 0.5625f, -0.0625f)/float4(0.0f, 0.5f, 0.5f, 0.0f)/'
  expect_rejected 'the Catmull-Rom weights down' 's/float4(-0.0234375f, 0.2265625f, 0.8671875f, -0.0703125f)/float4(0.0f, 0.25f, 0.75f, 0.0f)/'
  expect_rejected 'the colocated rows' 's/f.even = float4(0.0f, 0.0f, 1.0f, 0.0f);/f.even = float4(0.0f, 1.0f, 0.0f, 0.0f);/'
  expect_rejected 'the clamp of the cubic' 's/const float hi = bl || full_range() ? top : 240.0f \* unit;/const float hi = top;/'
  expect_rejected 'the clamp of the cubic in full range' 's/const float lo = bl || full_range() ? 0.0f : 16.0f \* unit;/const float lo = -1024.0f;/'
  expect_rejected 'the bilinear switch' 's/bool bilinear_chroma() { return (cb_flags \& 2048u) != 0; }/bool bilinear_chroma() { return false; }/'
  expect_rejected 'the left chroma edge' 's/  if (c == 0) r.s.x = r.s.y;/  if (false) r.s.x = r.s.y;/'
  expect_rejected 'the right chroma edge' 's/  if (c + 2 > last) r.s.w = r.s.z;/  if (false) r.s.w = r.s.z;/'
  expect_rejected 'the odd chroma edge' 's/  if (c + 1 > last) r.s.z = r.s.y;/  if (false) r.s.z = r.s.y;/'
  expect_rejected 'the last chroma column' 's/const int last = (int)((cb_src_w + 1u) \/ 2u) - 1;  \/\/ (w + 1) \/ 2/const int last = (int)(cb_src_w \/ 2u) - 1;  \/\/ (w + 1) \/ 2/'
  expect_rejected 'the chroma rows below the frame' 's/const int row = clamp(r + j, 0, last_row);/const int row = clamp(r + j, 0, last_row + 1);/'
  expect_rejected 'the 8-bit chroma window' 's/  const uint sh = (at \& 3u) \* 8u;/  const uint sh = 0u;/'
  expect_rejected 'the 16-bit chroma window of the quad path' 's/r.s = float4((float)(w.x >> 16), (float)(w.y \& 0xffffu)/r.s = float4((float)(w.x \& 0xffffu), (float)(w.y \& 0xffffu)/'
  expect_rejected 'the left chroma neighbour' 's/ex\[((uint)q \& 1u) \* THREADS + lane - 1u\]/ex[((uint)q \& 1u) * THREADS + lane]/'
  expect_rejected 'the edge column' 's/edge\[2u \* lane\] = x0 > 0 ? v.v10 : v.v00;/edge[2u * lane] = v.v00;/'
  expect_rejected 'the line of the gain table above 1200 nits' 's/const float t = clamp(cb_gain_s1 >= cb_gain_s2 ? min(a, b) : max(a, b),/const float t = clamp(min(a, b),/'
  expect_rejected 'the interpolation of the tables' 's/return mad(t, mad(t, mad(t, c.w, c.z), c.y), c.x);/return c.x;/'
  expect_rejected 'the exact peak below 1e-6 nits' 's/peak = pq_to_nits(emax);/peak = peak;/'
  expect_rejected 'the output depth' 's/if (nv12_out()) return/if (eight_bit()) return/'
  expect_rejected 'the NV12 chroma codes' 's/floor(128.0f + 224.0f \* cr + tr)/floor(128.0f + 224.0f * cb + tr)/'
  expect_rejected 'the chroma columns of 3:2' 's/const bool next = n == 3u \&\& (sx \& 1) != 0;/const bool next = false;/'
  expect_rejected 'the chroma columns of 1:1' 's/const float u = k == 0u ? (next ? cu.y : cu.x) : k == 1u ? (next ? cu.z : cu.y)/const float u = k == 0u ? (next ? cu.y : cu.x) : k == 1u ? (next ? cu.z : cu.z)/'
  expect_rejected 'the first sample of a run of the block path' 's/  r.first = x \& (eight_bit() ? 3u : 1u);/  r.first = 0u;/'
  expect_rejected 'the column weights of 3:2' 's/left = add_light(left, p, k == 0u ? 1.0f : 0.5f);/left = add_light(left, p, 1.0f);/'
  expect_rejected 'the row weights of 3:2' 's/(i == 0u ? 1.0f : i == 1u ? 0.5f : 0.0f);/(i == 0u ? 1.0f : i == 1u ? 1.0f : 0.0f);/'
  expect_rejected 'the rows of the quad path' 's/const bool top = k < 2u;/const bool top = k < 1u;/'
  expect_rejected 'the dither threshold' 's/if (!dithered()) return 0.5f;/return 0.5f;/'
  expect_rejected 'the transposed dither of Cr' 's/const float tr = threshold(uint2(p.y, p.x));/const float tr = threshold(p);/'
fi

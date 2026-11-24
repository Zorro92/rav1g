#!/bin/sh
# display_pass.hlsl as C++ on test frames (display_check.cpp), each display image held against the
# float32 model of the pass (model.py, numpy). Runs on a Linux host: a C++17 compiler, python3 with numpy
# (or uv, which fetches it). Pass --mutate to check that deliberate errors are rejected.
set -eu

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

build() {
  "${CXX:-c++}" -std=c++17 -O2 -fwrapv -DDISPLAY_MODE_FROM_FLAGS -ffp-contract=off -Wall -Wno-unused-function -Wno-unused-variable \
    -I "$here" -I "$sync" -I "$work" "$here/display_check.cpp" -o "$work/check"
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

build
check

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
  expect_rejected 'chroma siting of the quad path' 's/between ? 0.75f : 0.0f, m, eight/between ? 0.25f : 0.0f, m, eight/'
  expect_rejected 'chroma siting of the area path' 's/float2(0.0f, between_rows() ? 0.5f : 0.0f)) \* 0.5f;/float2(0.0f, between_rows() ? 0.0f : 0.5f)) * 0.5f;/'
  expect_rejected 'the chroma rows of the second step' 's/const int r = (between ? o.y - 1 : o.y) + (int)p;/const int r = (between ? o.y - 1 : o.y);/'
  expect_rejected 'the third chroma column' 's/(u1 + u2) \* 0.5f/(u1 + u1) * 0.5f/'
  expect_rejected 'the left chroma neighbour' 's/ex\[((uint)q \& 1u) \* THREADS + lane - 1u\]/ex[((uint)q \& 1u) * THREADS + lane]/'
  expect_rejected 'the edge column' 's/edge\[2u \* lane\] = x0 > 0 ? v.v10 : v.v00;/edge[2u * lane] = v.v00;/'
  expect_rejected 'the right chroma edge' 's/return float3(run_sample(r, 0u, eight), s1, edge ? s1 : run_sample(r, 2u, eight));/return float3(run_sample(r, 0u, eight), s1, run_sample(r, 2u, eight));/'
  expect_rejected 'the interpolation of the tables' 's/return mad(t, mad(t, mad(t, c.w, c.z), c.y), c.x);/return c.x;/'
  expect_rejected 'the exact peak below 1e-6 nits' 's/peak = pq_to_nits(emax);/peak = peak;/'
  expect_rejected 'the 8-bit chroma pair' 's/const uint pos = (r.first + k) \* 8u;/const uint pos = k * 8u;/'
  expect_rejected 'the output depth' 's/if (nv12_out()) return/if (eight_bit()) return/'
  expect_rejected 'the NV12 chroma codes' 's/floor(128.0f + 224.0f \* cr + tr)/floor(128.0f + 224.0f * cb + tr)/'
  expect_rejected 'the chroma rows of the block path' 's/const bool down = n == 2u ? between \&\& i == 1u : ((between ? (y - 1) >> 1 : y >> 1) - r0) != 0;/const bool down = false;/'
  expect_rejected 'the chroma rows above the frame' 's/const int r0 = between ? (sy - 1) >> 1 : sy >> 1;/const int r0 = sy >> 1;/'
  expect_rejected 'the chroma columns of 3:2' 's/const bool next = n == 2u ? false : (x >> 1) != c0;/const bool next = false;/'
  expect_rejected 'the chroma columns of 1:1' 's/const float fx = (n == 2u ? j == 1u : (x \& 1u) != 0) ? 0.5f : 0.0f;/const float fx = (x \& 1u) != 0 ? 0.0f : 0.5f;/'
  expect_rejected 'the first sample of a run of the block path' 's/  r.first = x \& (eight_bit() ? 3u : 1u);/  r.first = 0u;/'
  expect_rejected 'the right chroma edge of the block path' 's/const uint k1 = min(1u, cmax), k2 = n == 2u ? k1 : min(2u, cmax);/const uint k1 = 1u, k2 = 2u;/'
  expect_rejected 'the column weights of 3:2' 's/left = add_light(left, p, j == 0u ? 1.0f : 0.5f);/left = add_light(left, p, 1.0f);/'
  expect_rejected 'the row weights of 3:2' 's/const float w = i == 0u ? 1.0f : 0.5f;/const float w = 1.0f;/'
  expect_rejected 'the third luma row of 3:2' 's/if (n == 3u) l2 = load_aligned(cb_y_offset, cb_y_stride, sy + 2, (uint)sx);/if (n == 3u) l2 = l1;/'
  expect_rejected 'the odd chroma edge' 's/const int2 last = int2((int)((cb_src_w + 1u) \/ 2u) - 1,/const int2 last = int2((int)(cb_src_w \/ 2u) - 1,/'
  expect_rejected 'the dither threshold' 's/if (!dithered()) return 0.5f;/return 0.5f;/'
  expect_rejected 'the transposed dither of Cr' 's/const float tr = threshold(uint2(p.y, p.x));/const float tr = threshold(p);/'
fi

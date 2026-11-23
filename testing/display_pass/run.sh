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

# Runs the shader and the model on every case; exit 1 when a case fails either.
check() {
  mkdir -p "$work/out"
  timeout 600 "$work/check" "$work/out" > "$work/cases.txt" || return 1
  status=0
  while read -r name frames display args; do
    # shellcheck disable=SC2086
    if ! py "$here/model.py" "$frames" "$display" $args > "$work/model.log" 2>&1; then
      echo "MISMATCH $name"
      tail -n 3 "$work/model.log"
      status=1
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
  if check > "$work/mutant.log" 2>&1; then
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
  expect_rejected 'the NV12 chroma codes' 's/floor(128.0f + 224.0f \* cr + 0.5f)/floor(128.0f + 224.0f * cb + 0.5f)/'
fi

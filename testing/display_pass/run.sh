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
  "${CXX:-c++}" -std=c++17 -O2 -fwrapv -ffp-contract=off -Wall -Wno-unused-function -Wno-unused-variable \
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
  expect_rejected 'chroma siting of the quad path' 's/(between_rows() ? 0.5f : 0.0f)) \* 0.5f;/(between_rows() ? 0.0f : 0.5f)) * 0.5f;/'
  expect_rejected 'the third chroma column' 's/(u1 + u2) \* 0.5f/(u1 + u1) * 0.5f/'
  expect_rejected 'the left chroma neighbour' 's/ex\[(q \& 1u) \* THREADS + lane - 1u\]/ex[(q \& 1u) * THREADS + lane]/'
  expect_rejected 'the edge column' 's/area_linear(int2(x0 - 1, oy))/area_linear(int2(x0, oy))/'
  expect_rejected 'the EOTF table' 's/return mad(f, b - a, a);/return a;/'
  expect_rejected 'the exact peak below 1e-6 nits' 's/peak = pq_to_nits(emax);/peak = peak;/'
fi

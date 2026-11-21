#!/bin/sh
# Builds and runs the CDEF bit-exactness checks on a Linux host (g++, no GPU).
#
#   sh testing/cdef_check/run.sh            the check; exit code 0 = identical
#   sh testing/cdef_check/run.sh --mutate   also checks deliberate changes to
#                                           the filters and list generation
set -eu

here=$(cd "$(dirname "$0")" && pwd)
shaders="$here/../../libav1/dx/shaders"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
# No run of the check may hang: a thread group that does not end is ended by the scheduler
# (testing/common/group_sched.h), anything else by these timeouts, in seconds.
check_timeout=600
mutant_timeout=600

# HLSL to C++: drop attributes, semantics and register bindings, unwrap the
# constant buffer, rename the entry point. The shader code itself is untouched.
to_cpp() {
  sed -E \
    -e 's/\[numthreads\([^]]*\)\]//' \
    -e 's/\[(unroll|loop|branch)\]//g' \
    -e 's/\binout int4 /int4 \&/g' \
    -e 's/: *SV_[A-Za-z]+//g' \
    -e 's/: *register\([a-z0-9]+\)//g' \
    -e 's/^cbuffer +[A-Za-z_]+ *\{(.*)\}/\1/' \
    -e 's/^cbuffer +[A-Za-z_]+ *\{$/namespace {/' \
    -e 's/void main\(/void shader_main(/' \
    "$1" | sed -E -f "$here/../common/gs_groupshared.sed"
}

build() {
  g++ -std=c++20 -O2 -fwrapv -Wno-unknown-pragmas -I "$1" -I "$here" -I "$shaders" -o "$1/$2" "$here/$2.cpp"
}

to_cpp "$shaders/cdef_filter.hlsl" > "$work/cdef_filter.gen.h"
to_cpp "$shaders/cdef_filter_v2.hlsl" > "$work/cdef_filter_v2.gen.h"
to_cpp "$shaders/cdef_lists.hlsl" > "$work/cdef_lists.gen.h"
build "$work" cdef_check
timeout -k 10 "$check_timeout" "$work/cdef_check"
build "$work" cdef_lists_check
timeout -k 10 "$check_timeout" "$work/cdef_lists_check"

expect_rejected() {
  file="$1.gen.h"
  expr="$2"
  why="$3"
  expected="$4"
  check="${5:-cdef_check}"
  cp "$work"/*.gen.h "$work/mutant/"
  sed -e "$expr" "$work/$file" > "$work/mutant/$file"
  if cmp -s "$work/$file" "$work/mutant/$file"; then
    echo "cdef_check: $why mutation did not apply" >&2
    exit 1
  fi
  build "$work/mutant" "$check"
  status=0
  timeout -k 10 "$mutant_timeout" "$work/mutant/$check" --fail-fast > "$work/mutant/log" 2>&1 || status=$?
  if [ "$status" != 1 ] || ! grep -q "$expected" "$work/mutant/log"; then
    cat "$work/mutant/log" >&2
    echo "cdef_check: $why did not yield the expected error (exit $status)" >&2
    exit 1
  fi
  echo "cdef_check: rejected $why"
}

if [ "${1:-}" = "--mutate" ]; then
  mkdir "$work/mutant"
  expect_rejected cdef_filter_v2 's/f.pri_tap0 = 4 - p_t;/f.pri_tap0 = 3 - p_t;/' "wrong primary tap weight" '^MISMATCH optimized'
  expect_rejected cdef_filter_v2 's/if (cost\[i\] > best_cost)/if (cost[i] >= best_cost)/' "last-direction tie break" '^MISMATCH direction'
  expect_rejected cdef_filter_v2 's/(best_cost - opposite_cost) >> 10/(best_cost - opposite_cost) >> 9/' "wrong variance shift" '^MISMATCH direction'
  expect_rejected cdef_filter_v2 's/uint2(CDEF_VERY_LARGE_X2, CDEF_VERY_LARGE_X2)/uint2(0u, 0u)/' "missing border sentinel" '^MISMATCH optimized'
  for f in cdef_filter cdef_filter_v2; do
    expect_rejected "$f" \
      '/void shader_main(/,/GroupMemoryBarrierWithGroupSync();/s/GroupMemoryBarrierWithGroupSync();/GroupMemoryBarrier();/' \
      "$f halo sync weakened to a barrier" 'races in group memory: [1-9]'
  done
  expect_rejected cdef_filter_v2 's/^    GroupMemoryBarrierWithGroupSync();/    GroupMemoryBarrier();/' \
    "direction-cost sync weakened to a barrier" 'races in group memory: [1-9]'
  expect_rejected cdef_lists 's/ || top.x != bottom.y)/)/' "missing bottom-right record in the reuse test" \
    '^MISMATCH optimized lists' cdef_lists_check
  expect_rejected cdef_lists 's/((int)(word << 8)) >> 24/(int)((word >> 16) \& 255)/' "unsigned strength index" \
    '^MISMATCH optimized lists' cdef_lists_check
  expect_rejected cdef_lists 's/min(thread.y, cb_skip_rows - 1)/thread.y/' "unclamped MI row" \
    'out-of-bounds buffer accesses [1-9]' cdef_lists_check
fi

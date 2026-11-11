#!/bin/sh
# Builds and runs the CDEF bit-exactness check on a Linux host (g++, no GPU).
#
#   sh testing/cdef_check/run.sh            the check; exit code 0 = identical
#   sh testing/cdef_check/run.sh --mutate   also proves the check can fail: a
#                                           copy of the new shader with one tap
#                                           weight changed must be rejected
set -eu

here=$(cd "$(dirname "$0")" && pwd)
shaders="$here/../../libav1/dx/shaders"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# HLSL to C++: drop attributes, semantics and register bindings, unwrap the
# constant buffer, rename the entry point. The shader code itself is untouched.
to_cpp() {
  sed -E \
    -e 's/\[numthreads\([^]]*\)\]//' \
    -e 's/\[(unroll|loop)\]//g' \
    -e 's/: *SV_[A-Za-z]+//g' \
    -e 's/: *register\([a-z0-9]+\)//g' \
    -e 's/^cbuffer +[A-Za-z_]+ *\{(.*)\}/\1/' \
    -e 's/void main\(/void shader_main(/' \
    "$1"
}

build() {
  g++ -std=c++20 -O2 -fwrapv -Wno-unknown-pragmas -I "$1" -I "$here" -o "$1/cdef_check" "$here/cdef_check.cpp"
}

to_cpp "$shaders/cdef_filter.hlsl" > "$work/cdef_filter.gen.h"
to_cpp "$shaders/cdef_filter_v2.hlsl" > "$work/cdef_filter_v2.gen.h"
build "$work"
"$work/cdef_check"

if [ "${1:-}" = "--mutate" ]; then
  mkdir "$work/mutant"
  cp "$work/cdef_filter.gen.h" "$work/mutant/"
  sed -e 's/f.pri_tap0 = 4 - p_t;/f.pri_tap0 = 3 - p_t;/' "$work/cdef_filter_v2.gen.h" > "$work/mutant/cdef_filter_v2.gen.h"
  if cmp -s "$work/cdef_filter_v2.gen.h" "$work/mutant/cdef_filter_v2.gen.h"; then
    echo "cdef_check: mutation did not apply" >&2
    exit 1
  fi
  build "$work/mutant"
  if "$work/mutant/cdef_check" > "$work/mutant/log" 2>&1; then
    echo "cdef_check: the mutated shader passed, the check is not sensitive" >&2
    exit 1
  fi
  echo "cdef_check: mutated shader rejected as expected ($(grep -c MISMATCH "$work/mutant/log") mismatches shown)"
fi

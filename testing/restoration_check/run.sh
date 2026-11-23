#!/bin/sh
# Compare loop restoration with a scalar AV1 reference (g++, python3, no GPU).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo="$here/../.."
shaders="$repo/libav1/dx/shaders"
sync="$here/../sync_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

python3 "$sync/to_cpp.py" "$shaders/restoration.h" "$work/restoration.gen.h"
python3 "$sync/to_cpp.py" "$shaders/output_sp.h" "$work/output_sp.gen.h"
python3 "$sync/to_cpp.py" "$shaders/loop_restoration.hlsl" "$work/optimized.gen.h" "$work/optimized.entry.h"

# The parameter sets the Rust host puts in the constant buffer.
sed -n '/^const sgr_params_type sgr_params\[/,/^};/p' "$repo/libav1/av1/common/restoration.c" > "$work/sgr.gen.h"
sed -n '/^const int32_t x_by_xplus1\[/,/^};/p' "$repo/libav1/av1/common/restoration.c" >> "$work/sgr.gen.h"

build() {
  g++ -std=c++17 -O2 -fwrapv -Wall -Wextra -Wno-unknown-pragmas -Wno-sign-compare -Wno-unused-function \
    -Wno-unused-parameter -I "$work" -I "$sync" -o "$work/restoration_check" "$here/restoration_check.cpp"
}
build
timeout -k 10 120 "$work/restoration_check"

if [ "${1:-}" = "--mutate" ]; then
  shader="$work/optimized.gen.h"
  cp "$shader" "$shader.original"
  count=$(grep -c 'GroupMemoryBarrierWithGroupSync();' "$shader.original")
  for index in $(seq "$count"); do
    awk -v wanted="$index" '
      /GroupMemoryBarrierWithGroupSync\(\);/ && ++found == wanted {
        sub(/GroupMemoryBarrierWithGroupSync\(\);/, "GroupMemoryBarrier();")
      }
      { print }
    ' "$shader.original" > "$shader"
    build
    status=0
    timeout -k 10 120 "$work/restoration_check" > "$work/mutant.log" 2>&1 || status=$?
    if [ "$status" != 1 ] || ! grep -q 'races [1-9]' "$work/mutant.log"; then
      cat "$work/mutant.log" >&2
      echo "restoration_check: expected a shared-memory race after removing group sync $index" >&2
      exit 1
    fi
    echo "restoration_check: rejected weakening group sync $index"
  done
fi

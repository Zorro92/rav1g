#!/bin/sh
# Compare the loop restoration shaders on a Linux host (g++, python3, no GPU).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo="$here/../.."
shaders="$repo/libav1/dx/shaders"
sync="$here/../sync_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

python3 "$sync/to_cpp.py" "$shaders/restoration.h" "$work/restoration.gen.h"
python3 "$sync/to_cpp.py" "$shaders/loop_restoration.hlsl" "$work/optimized.gen.h" "$work/optimized.entry.h"
python3 "$sync/to_cpp.py" "$shaders/loop_restoration_legacy.hlsl" "$work/legacy.gen.h" "$work/legacy.entry.h"

# The parameter sets the Rust host puts in the constant buffer.
sed -n '/^const sgr_params_type sgr_params\[/,/^};/p' "$repo/libav1/av1/common/restoration.c" > "$work/sgr.gen.h"
sed -n '/^const int32_t x_by_xplus1\[/,/^};/p' "$repo/libav1/av1/common/restoration.c" >> "$work/sgr.gen.h"

g++ -std=c++17 -O2 -fwrapv -Wall -Wextra -Wno-unknown-pragmas -Wno-sign-compare -Wno-unused-function \
  -I "$work" -I "$sync" -o "$work/restoration_check" "$here/restoration_check.cpp"
timeout -k 10 120 "$work/restoration_check"

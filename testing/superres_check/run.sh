#!/bin/sh
# Builds and runs the check of the super-resolution shader (upscaling.hlsl) against libaom's normative
# upscale on a Linux host (gcc, g++, python3, no GPU).
#
#   sh testing/superres_check/run.sh            the check; exit code 0 = identical
#   sh testing/superres_check/run.sh --mutate   also proves the check can fail: copies of the shader with
#                                               one detail changed must all be rejected
# Further arguments go to the check (--cases N, --seed S, -v).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
src="$here/../.."
shaders="$src/libav1/dx/shaders"
sync="$here/../sync_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
check_timeout=600
mutant_timeout=300

mutate=0
if [ "${1:-}" = "--mutate" ]; then
  mutate=1
  shift
fi

# The shader as C++: the converter and the HLSL types of sync_check.
python3 "$sync/to_cpp.py" "$shaders/upscaling.hlsl" "$work/upscaling.gen.h" "$work/upscaling.entry.h"

# libaom's C code of this tree, built once: the reference. Only the normative upscale is used; what the
# rest of resize.c refers to is not linked (one section per function, unused sections dropped).
mkdir "$work/ref"
ref_flags="-O2 -w -DNDEBUG -ffunction-sections -fdata-sections -I $src/libav1 -I $src/libav1/build"
for f in av1/common/resize.c av1/common/tile_common.c av1/common/convolve.c aom_mem/aom_mem.c; do
  # shellcheck disable=SC2086
  gcc $ref_flags -c -o "$work/ref/$(basename "$f" .c).o" "$src/libav1/$f" &
done
# shellcheck disable=SC2086
gcc $ref_flags -Wall -I "$here" -c -o "$work/ref/ref_glue.o" "$here/ref_glue.c" &
wait
for f in resize tile_common convolve aom_mem ref_glue; do
  if [ ! -f "$work/ref/$f.o" ]; then
    echo "superres_check: $f did not compile" >&2
    exit 1
  fi
done

# $1: directory with the generated files, also takes the binary.
build() {
  g++ -std=c++17 -O2 -fwrapv -Wall -Wno-unknown-pragmas -Wno-unused -Wno-parentheses -Wno-sign-compare \
    -Wno-maybe-uninitialized -Wno-uninitialized -Wno-narrowing -I "$1" -I "$here" -I "$sync" \
    -o "$1/superres_check" "$here/superres_check.cpp" "$work"/ref/*.o -Wl,--gc-sections -lm
}

build "$work"
timeout -k 10 "$check_timeout" "$work/superres_check" "$@"

if [ "$mutate" = 1 ]; then
  # sed expression @ what it breaks
  mutants='s/(1 << (SUPERRES_EXTRA_BITS - 1)) - err \/ 2;/(1 << (SUPERRES_EXTRA_BITS - 1)) - err;/@the share of the rounding error in the initial subpixel position
s/int maxX = src_plane.w - 1;/int maxX = src_plane.z - 1;/@right edge clamp: the visible width instead of the last mode info column
s/const int stepX = ((downscaledPlaneW << SUPERRES_SCALE_BITS) + (upscaledPlaneW >> 1)) \/ upscaledPlaneW;/const int stepX = (downscaledPlaneW << SUPERRES_SCALE_BITS) \/ upscaledPlaneW;/@rounding of the step
s/{-1, 3, -10, 123, 18, -6, 2, -1}/{-1, 3, -10, 123, 18, -6, 1, 0}/@one filter tap
s/if (first_x + i < minX) {/if (first_x + i < minX - 1) {/@left edge clamp
s/int first_x = (srcXPx.x - SUPERRES_FILTER_OFFSET) >> 2;/int first_x = (srcXPx.x - SUPERRES_FILTER_OFFSET + 1) >> 2;/@first source word of the four output samples'
  mkdir "$work/mutant"
  echo "$mutants" | while IFS='@' read -r expr what; do
    cp "$work"/upscaling.entry.h "$work/mutant/"
    sed -e "$expr" "$work/upscaling.gen.h" > "$work/mutant/upscaling.gen.h"
    if cmp -s "$work/upscaling.gen.h" "$work/mutant/upscaling.gen.h"; then
      echo "superres_check: mutation '$what' did not apply" >&2
      exit 1
    fi
    build "$work/mutant"
    if timeout -k 10 "$mutant_timeout" "$work/mutant/superres_check" "$@" > "$work/mutant/log" 2>&1; then
      echo "superres_check: the mutant '$what' passed, the check is not sensitive to it" >&2
      exit 1
    fi
    echo "superres_check: mutant rejected as expected: $what"
  done
fi

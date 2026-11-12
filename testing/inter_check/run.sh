#!/bin/sh
# Builds and runs the check of the fast inter prediction shaders (test flag -interfast) against the
# original shaders and against libaom's C code on a Linux host (gcc, g++, no GPU).
#
#   sh testing/inter_check/run.sh            the check; exit code 0 = identical
#   sh testing/inter_check/run.sh --mutate   also proves the check can fail:
#                                            copies of the shaders with one
#                                            detail changed must all be rejected
# Further arguments go to the check (--cases N, --seed S, -v).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
src="$here/../.."
shaders="$src/libav1/dx/shaders"
warp="$here/../warp_check"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
# No run of the check may hang, in seconds.
check_timeout=1200
mutant_timeout=600

mutate=0
if [ "${1:-}" = "--mutate" ]; then
  mutate=1
  shift
fi

# HLSL to C++, as testing/warp_check/run.sh does it: drop attributes, semantics and register bindings,
# turn the constant buffers into globals, rename the entry point, spell out the swizzles C++ has no syntax
# for, and include the generated form of what a shader includes. The shader code itself is untouched.
to_cpp() {
  sed -E \
    -e 's/\[numthreads\([^]]*\)\]//' \
    -e 's/\[(unroll|loop|branch)\]//g' \
    -e 's/: *SV_[A-Za-z]+//g' \
    -e 's/: *register\([a-z0-9]+\)//g' \
    -e 's/^cbuffer +[A-Za-z_0-9]+ *\{/namespace {/' \
    -e 's/void main\(/void shader_main(/' \
    -e 's/\.xy;/.xy();/' \
    -e '/^int2 filter_line2_hbd\(/,/^}/ s/\bl1\.x\b/l1/g' \
    -e 's/\bdouble\b/gpu_double/g' \
    -e 's/#include "([a-z0-9_]+)\.(h|hlsl)"/#include "\1.gen.h"/' \
    "$1"
}

for name in inter_common.h inter_fast_common.h inter_main.hlsl inter_main_hbd.hlsl inter_compound.hlsl \
  inter_compound_hbd.hlsl inter_fast.hlsl inter_fast_hbd.hlsl inter_compound_fast.hlsl inter_compound_fast_hbd.hlsl \
  inter_ext_borders.hlsl inter_ext_borders_hbd.hlsl; do
  to_cpp "$shaders/$name" > "$work/${name%.*}.gen.h"
done
# The class of a motion vector as gen_pred_blocks_fast.hlsl takes it from a block record: the one function.
sed -n 's/^int inter_fast_class(/static inline int inter_fast_class(/p' "$shaders/gen_pred_blocks.hlsl" \
  > "$work/inter_fast_class.gen.h"
if [ "$(wc -l < "$work/inter_fast_class.gen.h")" != 1 ]; then
  echo "inter_check: inter_fast_class() of gen_pred_blocks.hlsl is not the one line it was" >&2
  exit 1
fi
if grep -q '^groupshared' "$work"/*.gen.h; then
  echo "inter_check: a shader with group memory; this check runs the threads of a group one after the other" >&2
  exit 1
fi

# libaom's C code of this tree, built once: the reference (the glue of warp_check).
mkdir "$work/ref"
for f in av1/common/reconinter.c av1/common/warped_motion.c av1/common/convolve.c av1/common/scale.c \
  aom_dsp/blend_a64_mask.c aom_dsp/blend_a64_hmask.c aom_dsp/blend_a64_vmask.c aom_dsp/aom_convolve.c; do
  gcc -O2 -w -DNDEBUG -I "$src/libav1" -I "$src/libav1/build" -c -o "$work/ref/$(basename "$f" .c).o" "$src/libav1/$f" &
done
gcc -O2 -Wall -DNDEBUG -I "$src/libav1" -I "$src/libav1/build" -I "$warp" -c -o "$work/ref/ref_glue.o" "$warp/ref_glue.c" &
wait
for f in reconinter warped_motion convolve scale blend_a64_mask blend_a64_hmask blend_a64_vmask aom_convolve ref_glue; do
  if [ ! -f "$work/ref/$f.o" ]; then
    echo "inter_check: $f did not compile" >&2
    exit 1
  fi
done

# $1: directory with the generated files, also takes the objects and the binary. The shaders are built as
# the translation units of warp_check (kind 3: the inter constants, no group memory; kind 2: the borders).
build() {
  flags="-std=c++17 -O2 -fwrapv -Wall -Wno-unknown-pragmas -Wno-unused -Wno-parentheses -Wno-sign-compare"
  flags="$flags -I $1 -I $here -include $here/inter_ops.h"
  rm -f "$1"/*.o
  for shader in inter_main:3 inter_main_hbd:3 inter_compound:3 inter_compound_hbd:3 inter_fast:3 inter_fast_hbd:3 \
    inter_compound_fast:3 inter_compound_fast_hbd:3 inter_ext_borders:2 inter_ext_borders_hbd:2; do
    name=${shader%:*}
    # shellcheck disable=SC2086
    g++ $flags -DSHADER_FILE="\"$name.gen.h\"" -DSHADER_NS="ns_$name" -DSHADER_API="${name}_api" \
      -DSHADER_KIND="${shader#*:}" -c -o "$1/$name.o" "$warp/shader_tu.cpp" &
  done
  # shellcheck disable=SC2086
  g++ $flags -c -o "$1/inter_check.o" "$here/inter_check.cpp" &
  wait
  g++ -o "$1/inter_check" "$1"/*.o "$work"/ref/*.o
}

build "$work"
timeout -k 10 "$check_timeout" "$work/inter_check" "$@"

if [ "$mutate" = 1 ]; then
  # file @ sed expression @ what it breaks
  mutants='inter_fast.gen.h@0,/(3 << InterFastSampleShift)/s//(2 << InterFastSampleShift)/@column of the copy (one reference)
inter_fast.gen.h@s/inter_fast_round(sum, 64, 7)/inter_fast_round(sum, 63, 7)/@rounding of the vertical pass (one reference)
inter_fast.gen.h@s/inter_fast_round(l, 8 - InterFastLineBias, 4)/inter_fast_round(l, 7 - InterFastLineBias, 4)/@rounding of the horizontal pass (one reference)
inter_fast.gen.h@s/rows\[i\] = inter_fast_row(dst_frame, column + ref_stride \* clamp(mvy + i, 0, dims.y))/rows[i] = inter_fast_row(dst_frame, column + ref_stride * clamp(mvy + i, 0, dims.y + 1))/@bottom edge clamp of the vertical pass (one reference)
inter_fast.gen.h@s/kernel_h0, kernel_h1);/kernel_h1, kernel_h0);/@order of the taps of the horizontal pass (one reference)
inter_fast.gen.h@s/mvx = clamp(mvx, -11, dims.x) << InterFastSampleShift;/mvx = clamp(mvx, -11, dims.x + 16) << InterFastSampleShift;/@right edge clamp (one reference)
inter_fast.gen.h@s/clamp(mvy + 3 + i, 0, dims.y)), 0, 0);/clamp(mvy + 3 + i, -1, dims.y)), 0, 0);/@top edge clamp of the copy (one reference)
inter_fast_common.gen.h@s/row.w = (int)(l.x >> 24);/row.w = (int)((l.x >> 16) \& 0xff);/@fourth sample of a row of four (8-bit)
inter_fast_common.gen.h@s/p.y = (lo.y >> shift) | ((hi.y << (24 - shift)) << 8);/p.y = (lo.y >> shift);/@fourth sample of a row of four at an odd sample position (16-bit)
inter_fast_common.gen.h@s/r4.z \* k1.x/r4.z * k1.y/@a tap of the vertical pass
inter_compound_fast.gen.h@0,/OutputSub - InterFastLineBias);/s//OutputSub - InterFastLineBias + 1);/@offset of the horizontal pass (two references)
inter_compound_fast.gen.h@s/clamp(mvy + 4, 0, dims.y)), 16, 0, 0,/clamp(mvy + 5, 0, dims.y)), 16, 0, 0,/@second row of the copy (two references)
inter_compound_fast.gen.h@0,/^ *4, 3, OutputSub);/s//3, 3, OutputSub);/@rounding of the vertical pass (two references)
inter_compound_fast.gen.h@s/dims.y)), 16, 0, 0,/dims.y)), 15, 0, 0,/@scale of the copy (two references)
inter_compound_fast.gen.h@s/const int frac_class1 = (cb_width_log2 >> 10) \& 3;/const int frac_class1 = (cb_width_log2 >> 8) \& 3;/@class of the second reference
inter_compound_fast.gen.h@s/((block.y >> 14) \& 7) \* 3 + plane, dims);/((block.y >> 2) \& 7) * 3 + plane, dims);/@frame of the second reference
inter_fast_class.gen.h@s/(mv \& 15) != 0 ? 2 : 0/(mv \& 7) != 0 ? 2 : 0/@class of a vector with half a sample in y'
  mkdir "$work/mutant"
  echo "$mutants" | while IFS='@' read -r file expr what; do
    cp "$work"/*.gen.h "$work/mutant/"
    sed -e "$expr" "$work/$file" > "$work/mutant/$file"
    if cmp -s "$work/$file" "$work/mutant/$file"; then
      echo "inter_check: mutation '$what' did not apply" >&2
      exit 1
    fi
    build "$work/mutant"
    if timeout -k 10 "$mutant_timeout" "$work/mutant/inter_check" "$@" > "$work/mutant/log" 2>&1; then
      echo "inter_check: the mutant '$what' passed, the check is not sensitive to it" >&2
      exit 1
    fi
    echo "inter_check: mutant rejected as expected: $what"
  done
fi

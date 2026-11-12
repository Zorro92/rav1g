#!/bin/sh
# Builds and runs the check of the warp inter prediction shaders against
# libaom's C code on a Linux host (gcc, g++, no GPU).
#
#   sh testing/warp_check/run.sh            the check; exit code 0 = identical
#   sh testing/warp_check/run.sh --mutate   also proves the check can fail:
#                                           copies of the shaders with one
#                                           detail changed must all be rejected
# Further arguments go to the check (--cases N, --seed S, -v).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
src="$here/../.."
shaders="$src/libav1/dx/shaders"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
# No run of the check may hang: a thread group that does not end is ended by the scheduler
# (testing/common/group_sched.h), anything else by these timeouts, in seconds.
check_timeout=1200
mutant_timeout=600

mutate=0
if [ "${1:-}" = "--mutate" ]; then
  mutate=1
  shift
fi

# HLSL to C++: drop attributes, semantics and register bindings, turn the
# constant buffers into globals, rename the entry point, spell out the
# swizzles C++ has no syntax for, and give double precision the type of
# hlsl_shim.h that counts its operations. The shader code itself is untouched.
to_cpp() {
  sed -E \
    -e 's/\[numthreads\([^]]*\)\]//' \
    -e 's/\[(unroll|loop|branch)\]//g' \
    -e 's/: *SV_[A-Za-z]+//g' \
    -e 's/: *register\([a-z0-9]+\)//g' \
    -e 's/^cbuffer +[A-Za-z_0-9]+ *\{/namespace {/' \
    -e 's/void main\(/void shader_main(/' \
    -e 's/\.xy;/.xy();/' \
    -e 's/([a-z0-9_]+)\.yzw = ([a-z0-9_]+)\.xxx;/\1.y = \2.x; \1.z = \2.x; \1.w = \2.x;/' \
    -e '/^int2 filter_line2_hbd\(/,/^}/ s/\bl1\.x\b/l1/g' \
    -e 's/\bdouble\b/gpu_double/g' \
    -e 's/#include "inter_common\.h"/#include "inter_common.gen.h"/' \
    "$1" | sed -E -f "$here/../common/gs_groupshared.sed"
}

to_cpp "$shaders/inter_common.h" > "$work/inter_common.gen.h"
to_cpp "$shaders/inter_warp.hlsl" > "$work/inter_warp.gen.h"
to_cpp "$shaders/inter_warp_hbd.hlsl" > "$work/inter_warp_hbd.gen.h"
to_cpp "$shaders/inter_scale_warp_compound.hlsl" > "$work/inter_warp_comp.gen.h"
to_cpp "$shaders/inter_scale_warp_compound_hbd.hlsl" > "$work/inter_warp_comp_hbd.gen.h"
to_cpp "$shaders/inter_ext_borders.hlsl" > "$work/inter_ext_borders.gen.h"
to_cpp "$shaders/inter_ext_borders_hbd.hlsl" > "$work/inter_ext_borders_hbd.gen.h"
# All group memory has to be the kind that is watched.
if grep -q '^groupshared' "$work"/*.gen.h; then
  echo "warp_check: a group memory declaration was not converted" >&2
  exit 1
fi

# libaom's C code of this tree, built once: the reference.
mkdir "$work/ref"
for f in av1/common/reconinter.c av1/common/warped_motion.c av1/common/convolve.c av1/common/scale.c \
  aom_dsp/blend_a64_mask.c aom_dsp/blend_a64_hmask.c aom_dsp/blend_a64_vmask.c aom_dsp/aom_convolve.c; do
  gcc -O2 -w -DNDEBUG -I "$src/libav1" -I "$src/libav1/build" -c -o "$work/ref/$(basename "$f" .c).o" "$src/libav1/$f" &
done
gcc -O2 -Wall -DNDEBUG -I "$src/libav1" -I "$src/libav1/build" -I "$here" -c -o "$work/ref/ref_glue.o" "$here/ref_glue.c" &
wait
for f in reconinter warped_motion convolve scale blend_a64_mask blend_a64_hmask blend_a64_vmask aom_convolve ref_glue; do
  if [ ! -f "$work/ref/$f.o" ]; then
    echo "warp_check: $f did not compile" >&2
    exit 1
  fi
done

# $1: directory with the generated files, also takes the objects and the binary.
build() {
  flags="-std=c++17 -O2 -fwrapv -Wall -Wno-unknown-pragmas -Wno-unused -Wno-parentheses -Wno-sign-compare"
  flags="$flags -I $1 -I $here"
  rm -f "$1"/*.o
  for shader in inter_warp:0 inter_warp_hbd:0 inter_warp_comp:1 inter_warp_comp_hbd:1 inter_ext_borders:2 \
    inter_ext_borders_hbd:2; do
    name=${shader%:*}
    # shellcheck disable=SC2086
    g++ $flags -DSHADER_FILE="\"$name.gen.h\"" -DSHADER_NS="ns_$name" -DSHADER_API="${name}_api" \
      -DSHADER_KIND="${shader#*:}" -c -o "$1/$name.o" "$here/shader_tu.cpp" &
  done
  # shellcheck disable=SC2086
  g++ $flags -c -o "$1/warp_check.o" "$here/warp_check.cpp" &
  wait
  g++ -o "$1/warp_check" "$1"/*.o "$work"/ref/*.o
}

build "$work"
timeout -k 10 "$check_timeout" "$work/warp_check" "$@"

if [ "$mutate" = 1 ]; then
  # file @ sed expression @ what it breaks
  mutants='inter_warp_hbd.gen.h@s/int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x \& 7), -11, dims.x) << 1;/int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x \& 7), -11, dims.x + 8) << 1;/@right edge clamp of the warp (10-bit)
inter_warp.gen.h@s/int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x \& 7), -11, dims.x);/int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x \& 7), -19, dims.x);/@left edge clamp of the warp (8-bit)
inter_warp_hbd.gen.h@s/clamp(iy4 + 10, 0, dims.y)/clamp(iy4 + 10, 0, dims.y + 1)/@bottom edge clamp of the warp
inter_warp_comp_hbd.gen.h@s/int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x \& 7), -11, ref_w);/int ix4 = clamp((x4 >> WarpPrecBits) - 7 + (x \& 7), -11, ref_w + 8);/@right edge clamp of the compound warp
inter_warp_comp.gen.h@s/int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w);/int x0 = clamp((mvx >> SCALE_SUBPEL_BITS) - 3, -11, ref_w + 8);/@right edge clamp of the reference that is not warped
inter_warp_comp_hbd.gen.h@s/sy4 += info2.w \* (-4) + info2.z \* (-4);/sy4 += info2.w * (-4) + info2.z * (-3);/@shear of the compound warp
inter_warp.gen.h@s/const int src_x = ((x \& (~7)) + 4) << subsampling;/const int src_x = ((x \& (~7)) + 3) << subsampling;/@centre of the 8x8 warp unit
inter_common.gen.h@s/const int lo = val \* (sf \& 255) + off;/const int lo = val * (sf \& 255);/@scale_value offset
inter_common.gen.h@s/return neg ? hi - ((128 - lo) >> 8) : hi + ((lo + 128) >> 8);/return neg ? hi - ((127 - lo) >> 8) : hi + ((lo + 128) >> 8);/@scale_value rounding of negative positions
inter_warp.gen.h@s/const int2 dims = int2(cb_refplanes\[refplane\].z, cb_refplanes\[refplane\].w);/const int2 dims = cb_dims[subsampling].xy();/@size of reference 0, not of the reference of the block, as the clamp of the warp (8-bit; the code before the fix)
inter_warp_hbd.gen.h@s/const int2 dims = int2(cb_refplanes\[refplane\].z, cb_refplanes\[refplane\].w);/const int2 dims = cb_dims[subsampling].xy();/@size of reference 0, not of the reference of the block, as the clamp of the warp (10-bit; the code before the fix)
inter_warp_comp.gen.h@s/^  int ref_h = cb_refplanes\[refplane\].w;/  int ref_h = cb_dims[plane > 0].y;/@height of reference 0, not of the first reference, as the clamp of the compound warp
inter_warp_comp.gen.h@0,/^  GroupMemoryBarrierWithGroupSync();/s//  GroupMemoryBarrier();/@group sync between the two passes of the first reference as a plain barrier@races in group memory: [1-9]
inter_warp_comp_hbd.gen.h@/^  if (active \&\& compound_type == 2) {/,/GroupMemoryBarrierWithGroupSync();/s/GroupMemoryBarrierWithGroupSync();/GroupMemoryBarrier();/@group sync before the mask sums are read as a plain barrier@races in group memory: [1-9]'
  mkdir "$work/mutant"
  echo "$mutants" | while IFS='@' read -r file expr what expect; do
    cp "$work"/*.gen.h "$work/mutant/"
    sed -e "$expr" "$work/$file" > "$work/mutant/$file"
    if cmp -s "$work/$file" "$work/mutant/$file"; then
      echo "warp_check: mutation '$what' did not apply" >&2
      exit 1
    fi
    build "$work/mutant"
    if timeout -k 10 "$mutant_timeout" "$work/mutant/warp_check" "$@" > "$work/mutant/log" 2>&1; then
      echo "warp_check: the mutant '$what' passed, the check is not sensitive to it" >&2
      exit 1
    fi
    # A mutant that takes a group sync away has to be caught as a race, not by chance.
    if [ -n "$expect" ] && ! grep -q "$expect" "$work/mutant/log"; then
      echo "warp_check: the mutant '$what' was rejected, but not for races in its group memory" >&2
      tail -n 20 "$work/mutant/log" >&2
      exit 1
    fi
    echo "warp_check: mutant rejected as expected: $what"
  done
fi

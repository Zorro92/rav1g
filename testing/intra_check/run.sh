#!/bin/sh
# Builds and runs the intra prediction bit-exactness check on a Linux host
# (g++, no GPU): the optimized and legacy intra kernels against intra_v2 (-intrav2).
#
#   sh testing/intra_check/run.sh            the check; exit code 0 = identical
#   sh testing/intra_check/run.sh --mutate   also proves the check can fail:
#                                            copies of the shaders and of the
#                                            scheduler with one detail changed
#                                            must all be rejected, those with a
#                                            group sync turned into a plain
#                                            barrier for a race
# Further arguments go to the check (--layouts N, --seed S, -v).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
src="$here/../.."
shaders="$src/libav1/dx/shaders"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
# No run of the check may hang: a thread group that does not end is ended by the scheduler
# (testing/common/group_sched.h), anything else by these timeouts, in seconds.
check_timeout=2400
mutant_timeout=900

mutate=0
if [ "${1:-}" = "--mutate" ]; then
  mutate=1
  shift
fi

# HLSL to C++: drop attributes, semantics and register bindings, turn the
# constant buffers into globals, rename the entry point, spell out the two
# swizzles C++ has no syntax for. The shader code itself is untouched.
to_cpp() {
  sed -E \
    -e 's/\[numthreads\([^]]*\)\]//' \
    -e 's/\[(unroll|loop|branch)\]//g' \
    -e 's/: *SV_[A-Za-z]+//g' \
    -e 's/: *register\([a-z0-9]+\)//g' \
    -e 's/^cbuffer +[A-Za-z_0-9]+ *\{/namespace {/' \
    -e 's/void main\(/void shader_main(/' \
    -e 's/\.xyz;/.xyz();/' \
    -e 's/([a-z0-9_]+)\.zw = ([a-z0-9_]+)\.yy;/\1.z = \2.y; \1.w = \2.y;/' \
    -e 's/#include "(intra_main(_hbd)?|intra_filter(_hbd)?)\.(hlsl|h)"/#include "\1.gen.h"/' \
    -e 's/#include "intra_v2\.hlsl"/#include "intra_v2.gen.h"/' \
    "$1" | sed -E -f "$here/../common/gs_groupshared.sed"
}

to_cpp "$shaders/intra_main.hlsl" > "$work/intra_main.gen.h"
to_cpp "$shaders/intra_main_hbd.hlsl" > "$work/intra_main_hbd.gen.h"
to_cpp "$shaders/intra_main_prefetch_hbd.hlsl" > "$work/intra_main_prefetch_hbd.gen.h"
to_cpp "$shaders/intra_main_prefetch.hlsl" > "$work/intra_main_prefetch.gen.h"
to_cpp "$shaders/intra_main_single_edge.hlsl" > "$work/intra_main_single_edge.gen.h"
to_cpp "$shaders/intra_main_single_edge_hbd.hlsl" > "$work/intra_main_single_edge_hbd.gen.h"

# The count of edge samples the intra edge filter takes from the row above (AV1 7.11.2.4, numPx =
# Min(w, maxX - x + 1) + ...): the samples of the row above that are in the frame. The 10-bit shaders took
# twice that until 2026-11 (above_available counts pairs). Copies of the prefetching shaders with that
# count must give other samples, inside the frame too, where a block's row above crosses maxX (chroma
# from luma reads the luma right of maxX, 7.11.5): the check counts the frames where they do.
spec_count='min(above_available << 1, bw) + need_right'
double_count='min(above_available << 2, bw) + need_right'
sed -e "s/$spec_count/$double_count/" "$work/intra_main_hbd.gen.h" > "$work/intra_main_hbd_double.gen.h"
if [ "$(grep -c "$double_count" "$work/intra_main_hbd_double.gen.h")" != 1 ] ||
  cmp -s "$work/intra_main_hbd.gen.h" "$work/intra_main_hbd_double.gen.h"; then
  echo "intra_check: the edge sample count of intra_main_hbd.hlsl is not where run.sh expects it" >&2
  exit 1
fi
sed -e 's/intra_main_hbd\.gen\.h/intra_main_hbd_double.gen.h/' "$work/intra_main_prefetch_hbd.gen.h" \
  > "$work/intra_main_prefetch_double_hbd.gen.h"
sed -e 's/intra_main_hbd\.gen\.h/intra_main_hbd_double.gen.h/' "$work/intra_main_prefetch.gen.h" \
  > "$work/intra_main_prefetch_double.gen.h"
to_cpp "$shaders/intra_filter_prefetch.hlsl" > "$work/intra_filter_prefetch.gen.h"
to_cpp "$shaders/intra_filter_prefetch_hbd.hlsl" > "$work/intra_filter_prefetch_hbd.gen.h"
# The reference kernels: the INTRA_MAIN_LEGACY parts of the main shaders and the previous 8-bit filter
# intra kernel (the wrappers and intra_filter_legacy.hlsl are here, not among the decoder's shaders).
to_cpp "$here/intra_main_legacy.hlsl" > "$work/intra_main_legacy.gen.h"
to_cpp "$here/intra_main_hbd_legacy.hlsl" > "$work/intra_main_hbd_legacy.gen.h"
to_cpp "$shaders/intra_filter.h" > "$work/intra_filter.gen.h"
to_cpp "$here/intra_filter_legacy.hlsl" > "$work/intra_filter_legacy.gen.h"
to_cpp "$shaders/intra_filter_hbd.h" > "$work/intra_filter_hbd.gen.h"
to_cpp "$shaders/intra_v2.hlsl" > "$work/intra_v2.gen.h"
to_cpp "$shaders/intra_v2_hbd.hlsl" > "$work/intra_v2_hbd.gen.h"
# All group memory has to be the kind that is watched (testing/common/group_sched.h).
if grep -q '^groupshared' "$work"/*.gen.h; then
  echo "intra_check: a group memory declaration was not converted" >&2
  exit 1
fi

# The lookup tables of the decoder, from its sources: the table of the intra modes and the masks of the
# neighbours a mode reads are constants of the Rust host (crates/rav1g/src/host), written here as C.
host="$src/crates/rav1g/src/host"
{
  echo '// Generated by run.sh from the decoder sources.'
  echo 'const int intra_mode_shader_params[16 * 7][4] = {'
  sed -n '/^pub(crate) const INTRA_MODE_SHADER_PARAMS: \[i32; 448\] = \[$/,/^\];/p' "$host/tables.rs" | sed -e 1d -e '$d'
  echo '};'
  echo 'static const signed char av1_filter_intra_taps[5][8][8] = {'
  sed -n '/av1_filter_intra_taps\[FILTER_INTRA_MODES\]\[8\]\[8\]) = {/,/^};/p' "$src/libav1/av1/common/reconintra.c" |
    sed 1d
  grep '^#define MAX_BLOCK_DIM' "$src/libav1/aom_dsp/intrapred_common.h"
  sed -n '/^static const uint8_t sm_weight_arrays\[/,/^};/p' "$src/libav1/aom_dsp/intrapred_common.h"
  sed -n '/^int intra_edge_filter_strength(/,/^}/p' "$shaders/gen_pred_blocks.hlsl"
  echo 'enum {'
  sed -n -E \
    -e 's/^const NEED_ABOVE_LUT: u32 = (0x[0-9a-fA-F]+);.*/  NeedAboveLut = \1,/p' \
    -e 's/^const NEED_LEFT_LUT: u32 = (0x[0-9a-fA-F]+);.*/  NeedLeftLut = \1,/p' \
    -e 's/^const NEED_RIGHT_LUT: u32 = (0x[0-9a-fA-F]+);.*/  NeedRightLut = \1,/p' \
    -e 's/^const NEED_BOT_LUT: u32 = (0x[0-9a-fA-F]+);.*/  NeedBotLut = \1,/p' \
    -e 's/^const NEED_ABOVE_LEFT_LUT: u32 = (0x[0-9a-fA-F]+);.*/  NeedAboveLeftLut = \1,/p' \
    "$host/blocks.rs"
  echo '};'
} > "$work/tables.gen.h"
for name in '^    11, 0, 305, 5, 7, 0, 305, 5,' '80, 5, 0, 0, 80, 5,$' '{ -6, 10, 0, 0, 0, 12, 0, 0 }' 'sm_weight_arrays\[' \
  'int intra_edge_filter_strength(' 'NeedAboveLut = ' 'NeedLeftLut = ' 'NeedRightLut = ' 'NeedBotLut = ' \
  'NeedAboveLeftLut = '; do
  if ! grep -q "$name" "$work/tables.gen.h"; then
    echo "intra_check: table '$name' not found in the decoder sources" >&2
    exit 1
  fi
done

# $1: directory with the generated files, also takes the objects and the binary.
# $2: more compiler flags.
build() {
  flags="-std=c++17 -O2 -fwrapv -Wall -Wno-unknown-pragmas -Wno-unused -Wno-parentheses -Wno-sign-compare"
  flags="$flags -I $1 -I $here -I $src/libav1/dx ${2:-}"
  rm -f "$1"/*.o
  for shader in intra_main:0 intra_main_hbd:0 intra_main_legacy:0 intra_main_hbd_legacy:0 \
    intra_filter_legacy:1 intra_main_prefetch_hbd:0 intra_main_prefetch:0 \
    intra_main_prefetch_double_hbd:0 intra_main_prefetch_double:0 intra_main_single_edge:0 intra_main_single_edge_hbd:0 \
    intra_filter_prefetch:1 intra_filter_prefetch_hbd:1 intra_v2:2 intra_v2_hbd:2; do
    name=${shader%:*}
    hbd=
    case $name in
      # (SHADER_HBD names the group memory to scramble: the 8-bit prefetching shader has the 10-bit one's.)
      *_hbd | *_hbd_legacy | intra_main_prefetch | intra_main_prefetch_double | intra_main_single_edge) hbd=-DSHADER_HBD ;;
    esac
    filter_layout=
    case $name in
      intra_filter_legacy) filter_layout=-DSHADER_FILTER_LEGACY ;;
    esac
    # shellcheck disable=SC2086
    g++ $flags $hbd $filter_layout -DSHADER_FILE="\"$name.gen.h\"" -DSHADER_NS="ns_$name" -DSHADER_API="${name}_api" \
      -DSHADER_KIND="${shader#*:}" -c -o "$1/$name.o" "$here/shader_tu.cpp" &
  done
  # shellcheck disable=SC2086
  g++ $flags -c -o "$1/intra_check.o" "$here/intra_check.cpp" &
  wait
  g++ -o "$1/intra_check" "$1"/*.o
}

build "$work"
timeout -k 10 "$check_timeout" "$work/intra_check" "$@"

# Again with a limit of 6 passes per thread group instead of 250, which no
# real region comes close to: regions then overflow into further steps all
# the time, and that path gets checked too.
build "$work" -DINTRA_V2_MAX_PASSES=6
echo "With at most 6 passes per thread group:"
if ! timeout -k 10 "$check_timeout" "$work/intra_check" "$@" > "$work/short-passes.log"; then
  cat "$work/short-passes.log"
  exit 1
fi
grep -E 'intra_v2:|identical|sync' "$work/short-passes.log"
build "$work"

if [ "$mutate" = 1 ]; then
  # file @ sed expression @ what it breaks
  mutants='intra_v2.gen.h@s/tl = ((l + t) \* 5 + tl \* 6 + 8) >> 4;/tl = ((l + t) * 5 + tl * 6 + 7) >> 4;/@corner filter rounding
intra_v2.gen.h@s/sum += (j == 1 || j == 2) ? 9 \* edge_px : -edge_px;/sum += (j == 1 || j == 2) ? 8 * edge_px : -edge_px;/@edge upsampling tap
intra_v2.gen.h@s/const int ef = strength << 2;/const int ef = (strength \& 1) << 2;/@edge filter strength
intra_v2.gen.h@s/gs_fi_right\[t \* 4 + 1\] = row1.w;/gs_fi_right[t * 4 + 1] = row0.w;/@filter intra left neighbours
intra_v2.gen.h@s/avrg = (total + (2 << bsize_log)) >> (bsize_log + 2);/avrg = (total + (1 << bsize_log)) >> (bsize_log + 2);/@CFL average rounding
intra_v2.gen.h@s/w_l0 = (diff_l <= diff_t \&\& diff_l <= diff_tl) ? 32 : 0;/w_l0 = (diff_l < diff_t \&\& diff_l <= diff_tl) ? 32 : 0;/@Paeth tie
intra_v2.gen.h@s/return clamp((intra \* mask + inter \* (64 - mask) + 32) >> 6, 0, PIXEL_MAX);/return clamp((intra * mask + inter * (64 - mask) + 31) >> 6, 0, PIXEL_MAX);/@inter-intra blend rounding
intra_v2.gen.h@s/const int cell_active = mine \&\& (my_pass == (int)pass_i);/const int cell_active = mine \&\& (my_pass == 0 || my_pass == (int)pass_i);/@pass order inside a region
intra_v2_sched.h@s/need_step = intra_v2_max(need_step, step + 1);/need_step = intra_v2_max(need_step, step);/@step order between regions
intra_v2_sched.h@s/if (step == cur) need_pass = intra_v2_max(need_pass, ((d >> 16) \& 255) + 1);/if (step == cur) need_pass = intra_v2_max(need_pass, (d >> 16) \& 255);/@pass order inside a region (host)
intra_filter_legacy.gen.h@0,/GroupMemoryBarrierWithGroupSync();/s//GroupMemoryBarrier();/@first legacy filter intra group sync as a plain barrier@races in group memory .*[1-9]
intra_filter.gen.h@0,/GroupMemoryBarrierWithGroupSync();/s//GroupMemoryBarrier();/@8-bit filter intra preload sync as a plain barrier@races in group memory .*[1-9]
intra_filter.gen.h@s/^    GroupMemoryBarrierWithGroupSync();/    GroupMemoryBarrier();/@8-bit filter intra diagonal sync as a plain barrier@races in group memory .*[1-9]
intra_filter.gen.h@s/(sum + 0x8000u)/(sum)/@8-bit filter intra low sum borrow@MISMATCH
intra_filter.gen.h@s/(uint)lo.x + ((uint)hi.x << 16)/((uint)lo.x \& 65535) | ((uint)hi.x << 16)/@8-bit filter intra signed tap packing@MISMATCH
intra_filter.gen.h@s/0x00080008u/0x00080007u/@8-bit filter intra pair rounding@MISMATCH
intra_filter.gen.h@s/predicted | (output << 16)/output | (output << 16)/@8-bit filter intra uncorrected prediction edge@MISMATCH
intra_filter.gen.h@/dst_frame.Store(/p@8-bit filter intra duplicate frame word store@duplicate frame word stores [1-9]
intra_filter.gen.h@s/2 \* q \* res_stride/q * res_stride/@8-bit filter intra preloaded residual stride@MISMATCH
intra_filter_hbd.gen.h@s/(sum + 0x8000u)/(sum)/@high-bit-depth filter intra tap borrow
intra_filter_hbd.gen.h@s/if (diagonal != 0) GroupMemoryBarrierWithGroupSync();/if (diagonal != 0) GroupMemoryBarrier();/@filter intra diagonal group sync as a plain barrier@races in group memory .*[1-9]
intra_main.gen.h@/above\[loc_base + wi\] = luma.x + luma.y + luma.z + luma.w;/,/GroupMemoryBarrierWithGroupSync();/s/GroupMemoryBarrierWithGroupSync();/GroupMemoryBarrier();/@group sync before the CFL sum of intra_main as a plain barrier@races in group memory .*[1-9]
intra_main.gen.h@/if (wi == 0) left\[loc_base - 1\] = count/,/GroupMemoryBarrierWithGroupSync();/s/GroupMemoryBarrierWithGroupSync();/GroupMemoryBarrier();/@8-bit group sync after DC reduction as a plain barrier@races in group memory .*[1-9]
intra_main.gen.h@/if (wi == 0) above\[loc_base - 1\] = luma_sum;/,/GroupMemoryBarrierWithGroupSync();/s/GroupMemoryBarrierWithGroupSync();/GroupMemoryBarrier();/@8-bit group sync after CFL reduction as a plain barrier@races in group memory .*[1-9]
intra_main.gen.h@s/if (need_aboveleft) topleft = above/topleft = above/@reading an unprepared corner in intra_main@reads unwritten group memory
intra_main.gen.h@s/^  filter_count = 0;/  \/\/ Keep the above filter count./@filtering an unprepared left edge in intra_main@reads unwritten group memory
intra_main.gen.h@s/(int)WaveGetLaneCount()/64/@hard-coded wave width in the 8-bit reduction@wave reduction did not exercise waves
intra_main.gen.h@s/WaveActiveAllEqual(base)/true/@mixing blocks in an 8-bit intra wave reduction@MISMATCH in wave reduction
intra_main.gen.h@s/WaveActiveCountBits(true) == (uint)min(lanes, wi_count)/true/@reducing an incomplete 8-bit logical block@MISMATCH in wave reduction
intra_main.gen.h@s/dc_sum = WaveActiveSum(dc_sum);/dc_sum += 0;/@omitting the 8-bit DC wave sum@MISMATCH in wave reduction
intra_main.gen.h@s/luma_sum = WaveActiveSum(luma_sum);/luma_sum += 0;/@omitting the 8-bit CFL wave sum@MISMATCH in wave reduction
intra_main_hbd.gen.h@s/(int)WaveGetLaneCount()/64/@hard-coded wave width in the intra reduction@wave reduction did not exercise waves
intra_main_hbd.gen.h@s/WaveActiveAllEqual(base)/true/@mixing blocks in an intra wave reduction@MISMATCH in wave reduction
intra_main_hbd.gen.h@s/WaveActiveCountBits(true) == (uint)min(lanes, wi_count)/true/@reducing an incomplete logical block@MISMATCH in wave reduction
intra_main_hbd.gen.h@s/sum = WaveActiveSum(sum);/sum += 0;/@omitting the intra wave sum@MISMATCH in wave reduction
intra_main_hbd.gen.h@/int4 next_above = 0;/,/GroupMemoryBarrierWithGroupSync();/s/GroupMemoryBarrierWithGroupSync();/GroupMemoryBarrier();/@group sync before writing processed intra edges as a plain barrier@races in group memory .*[1-9]
intra_v2.gen.h@0,/GroupMemoryBarrierWithGroupSync();/s//GroupMemoryBarrier();/@group sync of a pass of intra_v2 as a plain barrier@races in group memory .*[1-9]
intra_main_hbd.gen.h@s/angle < 90 ? 305/angle < 87 ? 305/@zone of an angle by arithmetic (prefetch)@MISMATCH in the prefetching
intra_main_hbd.gen.h@s/0x43CB9D71u/0x43B39D71u/@nominal angle of D203 (prefetch)@MISMATCH in the prefetching
intra_main_hbd.gen.h@s/mode_info != Dir3 || left_available == 0/true/@opposite edge read in the same step@MISMATCH in the prefetching single-edge
intra_main_hbd.gen.h@s/smooth_h_weights\[i\] \* (left - right)/smooth_h_weights[0] * (left - right)/@prefetched smooth weights@MISMATCH in the prefetching
intra_main_hbd.gen.h@s/min(above_available << 1, bw) + need_right/min(above_available << 2, bw) + need_right/@edge sample count of the row above twice the specification (AV1 7.11.2.4 numPx)@MISMATCH in the prefetching 8-bit shaders
intra_v2.gen.h@s/min(aa_cells << 2, b.bw)/min(aa_cells << 3, b.bw)/@intra_v2 edge sample count of the row above twice the specification@MISMATCH between the legacy path and intra_v2
intra_main_hbd_double.gen.h@s/min(above_available << 2, bw) + need_right/min(above_available << 1, bw) + need_right/@coverage of a row above across maxX that changes the frame@no layout reached
intra_filter_hbd.gen.h@s/queue\[q\] = queue\[q + 1\];/queue[q] = queue[q];/@10-bit filter intra residual queue@MISMATCH between the legacy path and intra_v2
intra_filter.gen.h@s/queue\[q\] = queue\[q + 1\];/queue[q] = queue[q];/@8-bit filter intra residual queue@MISMATCH'
  mkdir "$work/mutant"
  echo "$mutants" | while IFS='@' read -r file expr what expect; do
    cp "$work"/*.gen.h "$work/mutant/"
    rm -f "$work/mutant/intra_v2_sched.h"
    if [ "$file" = intra_v2_sched.h ]; then
      sed -e "$expr" "$src/libav1/dx/intra_v2_sched.h" > "$work/mutant/intra_v2_sched.h"
      original="$src/libav1/dx/intra_v2_sched.h"
    else
      sed -e "$expr" "$work/$file" > "$work/mutant/$file"
      original="$work/$file"
    fi
    if cmp -s "$original" "$work/mutant/$file"; then
      echo "intra_check: mutation '$what' did not apply" >&2
      exit 1
    fi
    build "$work/mutant"
    if timeout -k 10 "$mutant_timeout" "$work/mutant/intra_check" "$@" > "$work/mutant/log" 2>&1; then
      echo "intra_check: the mutant '$what' passed, the check is not sensitive to it" >&2
      exit 1
    fi
    # Mutations with a specific diagnostic must fail for that reason.
    if [ -n "$expect" ] && ! grep -q "$expect" "$work/mutant/log"; then
      echo "intra_check: the mutant '$what' failed without '$expect'" >&2
      tail -n 20 "$work/mutant/log" >&2
      exit 1
    fi
    echo "intra_check: mutant rejected as expected: $what"
  done
fi

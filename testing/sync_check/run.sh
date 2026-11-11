#!/bin/sh
# Builds and runs sync_check on a Linux host (g++, python3, no GPU): every compute shader of the
# decoder that has group memory, compiled as C++, with its thread groups run under an adversarial
# schedule and its group memory watched for races (testing/common/group_sched.h).
#
#   sh testing/sync_check/run.sh             the check; exit code 0 = no shader fails
#   sh testing/sync_check/run.sh --mutate    also proves the check can fail: for every group sync
#                                            of every shader, a copy with that sync turned into a
#                                            plain GroupMemoryBarrier() must be rejected for a race
#   sh testing/sync_check/run.sh --shaders DIR   check the shaders of another directory (for
#                                            example a checkout of upstream, which fails)
# Further arguments go to the check (--cases N, --seed S, --only shader).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
src="$here/../.."
shaders="$src/libav1/dx/shaders"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mutate=0
while [ $# -gt 0 ]; do
  case "$1" in
    --mutate) mutate=1; shift ;;
    --shaders) shaders=$(cd "$2" && pwd); shift 2 ;;
    *) break ;;
  esac
done

# shader:family. Families (shader_tu.cpp): 1 inverse transforms, 2 lossless transform, 3 inter
# prediction, 4 deblocking, 5 loop restoration, 6 film grain templates (6y: one group per chroma
# plane), 7 film grain. The intra and CDEF shaders have checks of their own with real input
# (intra_check, cdef_check), which watch their group memory the same way.
list=""
for f in "$shaders"/*.hlsl; do
  name=$(basename "$f" .hlsl)
  grep -q '^groupshared' "$f" || continue
  case "$name" in
    idct_lossless) family=2 ;;
    idct*) family=1 ;;
    inter_*) family=3 ;;
    loopfilter_*) family=4 ;;
    loop_restoration) family=5 ;;
    film_grain_gen_luma) family=6 ;;
    film_grain_gen_chroma) family=6y ;;
    film_grain_filter) family=7 ;;
    intra_* | cdef_*) continue ;;
    *) echo "sync_check: $name.hlsl has group memory and no family here" >&2; exit 1 ;;
  esac
  list="$list $name:$family"
done

flags="-std=c++17 -O1 -fwrapv -Wall -Wno-unknown-pragmas -Wno-unused -Wno-parentheses -Wno-sign-compare"
flags="$flags -Wno-maybe-uninitialized -Wno-uninitialized -Wno-narrowing -I $here"

# $1: directory for the generated files and objects of one shader set.
convert() {
  for h in "$shaders"/*.h; do
    python3 "$here/to_cpp.py" "$h" "$1/$(basename "$h" .h).gen.h"
  done
  for item in $list; do
    name=${item%:*}
    python3 "$here/to_cpp.py" "$shaders/$name.hlsl" "$1/$name.gen.h" "$1/$name.entry.h"
  done
}

# $1: directory, $2: shader name, $3: family.
compile_shader() {
  groups_y=1
  family=$3
  if [ "$family" = 6y ]; then family=6; groups_y=2; fi
  # shellcheck disable=SC2086
  g++ $flags -I "$1" -DSHADER_FILE="\"$2.gen.h\"" -DSHADER_ENTRY="\"$2.entry.h\"" -DSHADER_NS="ns_$2" \
    -DSHADER_NAME="\"$2.hlsl\"" -DSHADER_FAMILY="$family" -DSHADER_GROUPS_Y="$groups_y" -c -o "$1/$2.o" \
    "$here/shader_tu.cpp"
}

jobs_limit=$(nproc 2>/dev/null || echo 4)
convert "$work"
running=0
for item in $list; do
  compile_shader "$work" "${item%:*}" "${item#*:}" &
  running=$((running + 1))
  if [ "$running" -ge "$jobs_limit" ]; then wait; running=0; fi
done
# shellcheck disable=SC2086
g++ $flags -c -o "$work/sync_check.o" "$here/sync_check.cpp" &
wait
for item in $list; do
  if [ ! -f "$work/${item%:*}.o" ]; then
    echo "sync_check: ${item%:*}.hlsl did not compile as C++" >&2
    exit 1
  fi
done
g++ -o "$work/sync_check" "$work"/*.o
status=0
"$work/sync_check" "$@" || status=$?
if [ "$status" != 0 ]; then exit "$status"; fi

if [ "$mutate" = 1 ]; then
  # Every group sync of every shader, one at a time, as a plain barrier. A sync of the transforms'
  # common header is tried with one transform of each kind of size.
  mkdir "$work/mutant"
  total=0
  missed=""
  for item in $list; do
    name=${item%:*}
    family=${item#*:}
    file="$name.gen.h"
    case "$name" in
      idct4x4 | idct16x32 | idct64x64) file="idct_shader_common.gen.h" ;;
      idct_lossless) ;;
      idct*) continue ;;
    esac
    count=$(grep -c 'GroupMemoryBarrierWithGroupSync();' "$work/$file" || true)
    k=1
    while [ "$k" -le "$count" ]; do
      rm -f "$work/mutant"/*
      cp "$work"/*.gen.h "$work"/*.entry.h "$work/mutant/"
      awk -v k="$k" '{ if (index($0, "GroupMemoryBarrierWithGroupSync();")) { n++; if (n == k) sub(/GroupMemoryBarrierWithGroupSync\(\);/, "GroupMemoryBarrier();") } print }' \
        "$work/$file" > "$work/mutant/$file"
      compile_shader "$work/mutant" "$name" "$family"
      g++ -o "$work/mutant/sync_check" "$work/mutant/$name.o" "$work/sync_check.o"
      total=$((total + 1))
      if "$work/mutant/sync_check" --allow-plain-barrier "$@" > "$work/mutant/log" 2>&1; then
        missed="$missed $name:$k"
      elif ! grep -q 'races in group memory' "$work/mutant/log"; then
        echo "sync_check: the mutant of $name.hlsl (group sync $k) failed for another reason than a race" >&2
        cat "$work/mutant/log" >&2
        exit 1
      fi
      k=$((k + 1))
    done
  done
  if [ -n "$missed" ]; then
    echo "sync_check: mutants that passed (shader:group sync turned into a plain barrier):$missed" >&2
    exit 1
  fi
  echo "sync_check: $total mutants, each with one group sync turned into a plain barrier: all rejected for a race"
fi

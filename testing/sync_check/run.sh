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
# On the way out, also when interrupted: no process of this run is left behind, and no directory.
cleanup() {
  for pid in $(jobs -p); do
    pkill -P "$pid" 2>/dev/null || true
    kill "$pid" 2>/dev/null || true
  done
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

# No run of the check may hang. The scheduler ends a thread group that does not end
# (group_sched.h); what it cannot see, a thread that spins without touching group memory, ends here.
check_timeout=1800
mutant_timeout=180

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
timeout -k 10 "$check_timeout" "$work/sync_check" "$@" || status=$?
if [ "$status" = 124 ] || [ "$status" = 137 ]; then
  echo "sync_check: no result after $check_timeout s, ended" >&2
  exit 1
fi
if [ "$status" != 0 ]; then exit "$status"; fi

if [ "$mutate" = 1 ]; then
  # Every group sync of every shader, one at a time, as a plain barrier. A sync of the transforms'
  # common header is tried with one transform of each kind of size. The mutants are built and run
  # several at a time, each in a directory of its own.
  # $1: shader name, $2: family, $3: the file with the sync, $4: which of its syncs.
  mutant() {
    dir="$work/mutants/$1.$4"
    mkdir -p "$dir"
    cp "$work"/*.gen.h "$work"/*.entry.h "$dir/"
    awk -v k="$4" '{ if (index($0, "GroupMemoryBarrierWithGroupSync();")) { n++; if (n == k) sub(/GroupMemoryBarrierWithGroupSync\(\);/, "GroupMemoryBarrier();") } print }' \
      "$work/$3" > "$dir/$3"
    if cmp -s "$work/$3" "$dir/$3"; then
      echo "did not apply" > "$dir/verdict"
    elif ! compile_shader "$dir" "$1" "$2" > "$dir/log" 2>&1 ||
      ! g++ -o "$dir/sync_check" "$dir/$1.o" "$work/sync_check.o" >> "$dir/log" 2>&1; then
      echo "did not build" > "$dir/verdict"
    else
      code=0
      timeout -k 10 "$mutant_timeout" "$dir/sync_check" --allow-plain-barrier --cases 20 > "$dir/log" 2>&1 || code=$?
      if [ "$code" = 0 ]; then
        echo "passed" > "$dir/verdict"
      elif [ "$code" = 124 ] || [ "$code" = 137 ]; then
        # The shader itself ends (the check above ran it), so a copy that does not end was broken by
        # taking the group sync away: found, though not as a race.
        echo "rejected: hung" > "$dir/verdict"
      elif ! grep -q 'races in group memory' "$dir/log"; then
        echo "failed for another reason than a race" > "$dir/verdict"
      else
        echo "rejected" > "$dir/verdict"
      fi
    fi
    rm -f "$dir"/*.gen.h "$dir"/*.entry.h "$dir"/*.o "$dir/sync_check"
  }
  running=0
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
      mutant "$name" "$family" "$file" "$k" &
      running=$((running + 1))
      if [ "$running" -ge "$jobs_limit" ]; then wait; running=0; fi
      k=$((k + 1))
    done
  done
  wait
  # Group syncs that order nothing, so that their mutants pass: the first two of film_grain_filter stand
  # between passes in which every thread reads and writes only the samples it fetched itself. Upstream
  # has them; they are left alone. A mutant listed here has to pass, or the list is out of date.
  redundant=" film_grain_filter.1 film_grain_filter.2 "
  total=0
  bad=0
  hung=0
  spare=0
  for verdict in "$work"/mutants/*/verdict; do
    total=$((total + 1))
    case "$redundant" in
      *" $(basename "$(dirname "$verdict")") "*)
        if [ "$(cat "$verdict")" = passed ]; then
          spare=$((spare + 1))
          continue
        fi
        bad=$((bad + 1))
        echo "sync_check: mutant $(basename "$(dirname "$verdict")") is listed as a group sync that orders nothing, but: $(cat "$verdict")" >&2
        continue
        ;;
    esac
    if [ "$(cat "$verdict")" = "rejected: hung" ]; then
      hung=$((hung + 1))
      echo "sync_check: mutant $(basename "$(dirname "$verdict")") did not end within $mutant_timeout s"
    elif [ "$(cat "$verdict")" != rejected ]; then
      bad=$((bad + 1))
      dir=$(dirname "$verdict")
      echo "sync_check: mutant $(basename "$dir") (shader.group sync turned into a plain barrier): $(cat "$verdict")" >&2
      tail -n 12 "$dir/log" >&2
    fi
  done
  if [ "$total" = 0 ] || [ "$bad" != 0 ]; then
    echo "sync_check: $bad of $total mutants were not rejected for a race" >&2
    exit 1
  fi
  echo "sync_check: $total mutants, each with one group sync turned into a plain barrier: $((total - hung - spare)) rejected for a race, $hung because they did not end, $spare pass as listed (group syncs of upstream that order nothing)"
fi

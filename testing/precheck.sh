#!/bin/sh
# The local check before a push: the checks of CI that run natively on Linux (gcc, g++, python3, cargo,
# no GPU), for the files a change touches, with the flags CI gives them (.github/workflows/build.yml),
# plus cargo fmt and, when the local images are there, the Windows cross-build and clippy.
#
#   sh testing/precheck.sh                 the checks for the files changed against origin/main: commits,
#                                          staged and unstaged changes, and untracked files
#   sh testing/precheck.sh --base REF      the same against REF
#   sh testing/precheck.sh --all           every check
#   sh testing/precheck.sh --list          the mapping, and which checks a run would start, without running
#   sh testing/precheck.sh -j N            at most N checks at a time (1 to 4, default 4)
#   sh testing/precheck.sh --keep          keep the logs also when everything passes
#   sh testing/precheck.sh --check-mapping CI: every testing/*/run.sh is in the mapping below
#
# A summary table (check, result, seconds) at the end; exit code 0 when no check failed. The logs of a
# run are in a new directory under $TMPDIR (else /tmp), and so is everything the checks build: /tmp
# may be a small tmpfs, so point TMPDIR at a disk with a few GB free. Usage: testing/README.md.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
cd "$repo"

# The mapping, the one place that decides what runs: name|command|paths. A check runs when a changed
# file matches one of its paths (shell patterns, in which * also matches /). The commands are the ones
# of build.yml; the three checks that CI does not run (copy, idct, residual_clear) run with
# --mutate. Heaviest first, so that they start first. "@" in the paths of a check means always.
# When unsure, a path is listed: a missed check costs more than a spare one.
SHADERS='libav1/dx/shaders/'
# What every check that turns HLSL into C++ reads: the shared headers of the shaders, the group
# scheduler and the HLSL types.
SHADER_COMMON="${SHADERS}*.h testing/common/group_sched.h testing/common/gs_groupshared.sed testing/sync_check/hlsl.h testing/sync_check/api.h testing/sync_check/to_cpp.py testing/sync_check/shader_tu.cpp"
# libaom's C and headers, which the checks against libaom compile (the library has no C).
LIBAOM='libav1/aom/* libav1/aom_dsp/* libav1/aom_mem/* libav1/aom_ports/* libav1/aom_scale/* libav1/aom_util/* libav1/av1/* libav1/build/*'
RUST='*.rs Cargo.toml Cargo.lock .cargo/* rustfmt.toml crates/* tools/* third_party/*'
MAPPING="
intra|sh testing/intra_check/run.sh --mutate --layouts 400|testing/intra_check/* ${SHADERS}intra_* ${SHADERS}gen_pred_blocks* libav1/dx/intra_v2_sched.h crates/rav1g/src/host/tables.rs libav1/av1/common/reconintra.c libav1/aom_dsp/intrapred_common.h $SHADER_COMMON
sync|sh testing/sync_check/run.sh --mutate|testing/sync_check/* testing/common/* ${SHADERS}*
inter|sh testing/inter_check/run.sh --mutate|testing/inter_check/* testing/warp_check/* ${SHADERS}inter_* ${SHADERS}gen_pred_blocks* $SHADER_COMMON $LIBAOM
warp|sh testing/warp_check/run.sh --mutate|testing/warp_check/* ${SHADERS}inter_* $SHADER_COMMON $LIBAOM
cdef|sh testing/cdef_check/run.sh --mutate|testing/cdef_check/* ${SHADERS}cdef_* $SHADER_COMMON
xwin|precheck_xwin|$RUST libav1/dx/* testing/common/* testing/player_common/* testing/sm6test/* testing/stage_dump/* testing/shader_cache/*
tables|sh testing/tables_check/run.sh --mutate|testing/tables_check/* crates/rav1g/src/host/* crates/rav1g-sys/* tools/rav1g-tables/* third_party/rav1d/* Cargo.toml Cargo.lock .cargo/* $LIBAOM
superres|sh testing/superres_check/run.sh --mutate|testing/superres_check/* ${SHADERS}upscaling* $SHADER_COMMON $LIBAOM
loopfilter|sh testing/loopfilter_check/run.sh --mutate|testing/loopfilter_check/* ${SHADERS}loopfilter_* ${SHADERS}gen_lf_blocks* $SHADER_COMMON
idct|sh testing/idct_check/run.sh --mutate|testing/idct_check/* ${SHADERS}idct* libav1/av1/common/scan.c $SHADER_COMMON
film_grain|sh testing/film_grain_check/run.sh --mutate|testing/film_grain_check/* ${SHADERS}film_grain* ${SHADERS}copy_plane_sp* $SHADER_COMMON
display_pass|sh testing/display_pass/run.sh --mutate|testing/display_pass/* ${SHADERS}display_pass* ${SHADERS}output_sp.h $SHADER_COMMON
shader_regs|precheck_shader_regs|testing/shader_regs/* testing/shader_cache/shaders.py crates/rav1g-sys/shaders.txt ${SHADERS}*
restoration|sh testing/restoration_check/run.sh --mutate|testing/restoration_check/* ${SHADERS}loop_restoration* ${SHADERS}restoration* libav1/av1/common/restoration.c $SHADER_COMMON
copy|sh testing/copy_check/run.sh --mutate|testing/copy_check/* ${SHADERS}copy_plane_sp* $SHADER_COMMON
seq_header|sh testing/seq_header_check/run.sh --mutate|testing/seq_header_check/* libav1/av1/common/seq_header_peek.h $LIBAOM
residual_clear|sh testing/residual_clear_check/run.sh --mutate|testing/residual_clear_check/* crates/rav1g/src/host/residual*
sm6test|sh testing/sm6test/run.sh|testing/sm6test/*
perf_stats|precheck_perf_stats|testing/player_common/* $LIBAOM
steptime|sh testing/steptime/run.sh|testing/steptime/*
fmt|precheck_fmt|@
mapping|sh testing/precheck.sh --check-mapping|@
"
# Files no check reads (CI's other jobs, docs, git's files). A changed file that matches no check and
# is not here starts every check.
NO_CHECK='*.md docs/* .github/* .gitignore .gitattributes LICENSE.txt av1xbox.sln testing/README.md testing/precheck.sh testing/argon/* testing/conformance/* testing/console/* testing/language_bar/* testing/md5ref/* testing/rav1d_md5/* testing/uwp_* libav1/LICENSE libav1/PATENTS'

usage() {
  sed -n '2,18p' "$here/precheck.sh" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

base=origin/main
all=0
list=0
keep=0
jobs=4
while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || usage; base=$2; shift 2 ;;
    --all) all=1; shift ;;
    --list) list=1; shift ;;
    --keep) keep=1; shift ;;
    -j) [ $# -ge 2 ] || usage; jobs=$2; shift 2 ;;
    --check-mapping) mode=check-mapping; shift ;;
    -h|--help) usage ;;
    *) echo "precheck: unknown argument $1" >&2; usage ;;
  esac
done
case "$jobs" in 1|2|3|4) ;; *) echo "precheck: -j takes 1 to 4 (no gain beyond 4 threads)" >&2; exit 2 ;; esac

# One line of the mapping a field: name, command or paths.
field() {
  printf '%s\n' "$1" | cut -d '|' -f "$2"
}
mapping_lines() {
  printf '%s\n' "$MAPPING" | sed '/^$/d'
}

if [ "${mode:-}" = check-mapping ]; then
  status=0
  for script in testing/*/run.sh; do
    if ! mapping_lines | cut -d '|' -f 2 | grep -q "^sh $script\( \|\$\)"; then
      echo "precheck: $script is not in the mapping of testing/precheck.sh: add a line for it" >&2
      status=1
    fi
  done
  for script in $(mapping_lines | cut -d '|' -f 2 | sed -n 's/^sh \(testing\/[^ ]*\/run\.sh\).*/\1/p'); do
    if [ ! -f "$script" ]; then
      echo "precheck: the mapping names $script, which does not exist" >&2
      status=1
    fi
  done
  names=$(mapping_lines | cut -d '|' -f 1)
  if [ "$(printf '%s\n' "$names" | sort | uniq -d)" ]; then
    echo "precheck: a check name is in the mapping twice: $(printf '%s\n' "$names" | sort | uniq -d | tr '\n' ' ')" >&2
    status=1
  fi
  [ "$status" = 0 ] && echo "precheck: all $(ls testing/*/run.sh | wc -l) testing/*/run.sh are in the mapping"
  exit "$status"
fi

# --- The checks that are functions -------------------------------------------------------------------

tools_dir() {
  printf '%s\n' "${RAV1G_SHADER_REGS_TOOLS:-${XDG_CACHE_HOME:-$HOME/.cache}/rav1g/shader_regs}"
}

precheck_fmt() {
  # The first-party packages: third_party/rav1d keeps the formatting of its upstream.
  packages=$(cargo metadata --no-deps --format-version 1 --locked | python3 -c '
import json, sys
meta = json.load(sys.stdin)
for p in meta["packages"]:
    if "/third_party/" not in p["manifest_path"].replace("\\", "/"):
        print(p["name"])')
  [ -n "$packages" ] || { echo "no packages"; return 1; }
  set --
  for p in $packages; do set -- "$@" -p "$p"; done
  echo "cargo fmt --check $*"
  cargo fmt --check "$@"
}

precheck_perf_stats() {
  # The step "Timing summary self-test" of the job intra-check.
  t=$(mktemp -d)
  status=0
  g++ -std=c++14 -Wall -Wextra -I libav1 -o "$t/perf_stats_test" testing/player_common/perf_stats_test.cpp &&
    "$t/perf_stats_test" || status=$?
  rm -rf "$t"
  return "$status"
}

precheck_shader_regs() {
  if ! python3 testing/shader_regs/shader_regs.py have; then
    echo "SKIP: the pinned RGA and DXC are not in $(tools_dir) (python3 testing/shader_regs/shader_regs.py fetch, once)"
    return 0
  fi
  t=$(mktemp -d)
  status=0
  python3 testing/shader_regs/shader_regs.py measure --jobs 2 --out "$t/shader_regs.tsv" --failures "$t/failures.tsv" &&
    python3 testing/shader_regs/shader_regs.py compare --failures "$t/failures.tsv" "$t/shader_regs.tsv" || status=$?
  [ "$status" = 0 ] || { cp "$t/shader_regs.tsv" "$logs/shader_regs.tsv" && echo "measured: $logs/shader_regs.tsv"; }
  rm -rf "$t"
  return "$status"
}

# The jobs rust-cross (cargo xwin build) and clippy, in the local image localhost/rav1g-xwin (podman,
# cargo-xwin, clang-cl, lld-link) with the host's cargo, rustup and xwin cache. The library embeds the
# shader bytecode: RAV1G_SHADER_DIR if set, else the shaders built here by testing/shader_cache/shaders.py
# with the Linux DXC of testing/shader_regs (its cache in $XDG_CACHE_HOME/rav1g/precheck).
precheck_xwin() {
  if ! command -v podman > /dev/null 2>&1 || ! podman image exists localhost/rav1g-xwin; then
    echo "SKIP: no local image localhost/rav1g-xwin (podman): no Windows cross-build, no clippy"
    return 0
  fi
  shader_dir=${RAV1G_SHADER_DIR:-}
  if [ -z "$shader_dir" ]; then
    dxc=$(tools_dir)/$(cd "$(tools_dir)" 2>/dev/null && ls -d dxc-* 2>/dev/null | head -n 1)/bin/dxc
    if [ ! -x "$dxc" ]; then
      echo "SKIP: no shader bytecode: set RAV1G_SHADER_DIR, or fetch the Linux DXC once (python3 testing/shader_regs/shader_regs.py fetch)"
      return 0
    fi
    cache=${XDG_CACHE_HOME:-$HOME/.cache}/rav1g/precheck
    shader_dir=$cache/shaders
    python3 testing/shader_cache/shaders.py build --dxc "$dxc" --cache "$cache/shader-cache" --root "$shader_dir" \
      --jobs 4 | tail -n 1 || return 1
  fi
  shader_dir=$(cd "$shader_dir" && pwd)
  mkdir -p target/precheck-xwin "$HOME/.cache/cargo-xwin"
  # Clippy: a warning in a first-party file this change touches fails (-D warnings, for the change); the
  # warnings of the files it does not touch are listed, and those of third_party/rav1d (its upstream's
  # lint set) are left out.
  podman run --rm --security-opt label=disable -v "$repo:/src:ro" -v "$repo/target/precheck-xwin:/target" \
    -v "$HOME/.cargo:/root/.cargo" -v "$HOME/.rustup:/root/.rustup:ro" -v "$HOME/.cache/cargo-xwin:/root/.cache/cargo-xwin" \
    -v "$shader_dir:/shaders:ro" -e RAV1G_SHADER_DIR=/shaders -e CARGO_TARGET_DIR=/target \
    -e PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin -e CARGO_TERM_COLOR=never -w /src localhost/rav1g-xwin \
    sh -c 'set -e
      echo "== cargo xwin build --release --locked --target x86_64-pc-windows-msvc"
      cargo xwin build --release --locked --target x86_64-pc-windows-msvc
      echo "== cargo xwin clippy --release --locked --target x86_64-pc-windows-msvc"
      cargo xwin clippy --release --locked --target x86_64-pc-windows-msvc --message-format=short 2>&1 | tee /target/clippy.txt' || return 1
  grep -E '^[^ ]+:[0-9]+:[0-9]+: (warning|error)' target/precheck-xwin/clippy.txt | grep -v '^third_party/' > "$logs/clippy-first-party.txt" || true
  status=0
  while IFS= read -r line; do
    file=${line%%:*}
    if grep -qxF "$file" "$logs/changed.txt"; then
      echo "CLIPPY (changed file): $line"
      status=1
    else
      echo "clippy (file not changed, not counted): $line"
    fi
  done < "$logs/clippy-first-party.txt"
  return "$status"
}

# --- Which checks ------------------------------------------------------------------------------------

logs=$(mktemp -d "${TMPDIR:-/tmp}/rav1g-precheck.XXXXXX")
cleanup() {
  for pid in $(jobs -p); do
    pkill -P "$pid" 2> /dev/null || true
    kill "$pid" 2> /dev/null || true
  done
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

: > "$logs/changed.txt"
# Also for --all: clippy counts the warnings of the changed files.
if merge_base=$(git merge-base "$base" HEAD 2> /dev/null); then
  { git diff --name-only "$merge_base"; git ls-files --others --exclude-standard; } | sort -u > "$logs/changed.txt"
elif [ "$all" = 0 ]; then
  echo "precheck: no merge base with $base (git fetch, --base REF, or --all)" >&2
  exit 2
fi

matches() {
  # $1 file, $2 paths
  for pattern in $2; do
    # shellcheck disable=SC2254
    case "$1" in $pattern) return 0 ;; esac
  done
  return 1
}

set -f # the patterns stay patterns
selected=''
if [ "$all" = 1 ]; then
  selected=$(mapping_lines | cut -d '|' -f 1)
else
  everything=''
  while IFS= read -r file; do
    hit=0
    while IFS= read -r line; do
      paths=$(field "$line" 3)
      [ "$paths" = @ ] && continue
      if matches "$file" "$paths"; then
        hit=1
        selected="$selected $(field "$line" 1)"
      fi
    done << EOF
$(mapping_lines)
EOF
    if [ "$hit" = 0 ] && ! matches "$file" "$NO_CHECK"; then
      everything="$everything $file"
    fi
  done < "$logs/changed.txt"
  if [ -n "$everything" ]; then
    echo "precheck: no check is mapped to$everything: every check runs"
    selected=$(mapping_lines | cut -d '|' -f 1)
  else
    selected="$selected $(mapping_lines | awk -F '|' '$3 == "@" { print $1 }')"
  fi
fi
set +f
# In the order of the mapping, once each.
order=''
for name in $(mapping_lines | cut -d '|' -f 1); do
  for s in $selected; do
    if [ "$s" = "$name" ]; then order="$order $name"; break; fi
  done
done

changed_count=$(wc -l < "$logs/changed.txt" | tr -d ' ')
if [ "$all" = 1 ]; then
  echo "precheck: --all: $(echo $order | wc -w | tr -d ' ') checks"
else
  echo "precheck: $changed_count files changed against $base ($(git rev-parse --short "$merge_base")): checks$order"
fi

if [ "$list" = 1 ]; then
  echo
  mapping_lines | while IFS= read -r line; do
    printf '%-15s %s\n                paths: %s\n' "$(field "$line" 1)" "$(field "$line" 2)" "$(field "$line" 3)"
  done
  rm -rf "$logs"
  trap - EXIT
  exit 0
fi

work_dir=${TMPDIR:-/tmp}
free_kb=$(df -Pk "$work_dir" | awk 'NR == 2 { print $4 }')
echo "precheck: logs and build files under $work_dir ($((free_kb / 1024)) MiB free), up to $jobs checks at a time"
if [ "$free_kb" -lt 3145728 ]; then
  echo "precheck: less than 3 GiB free under $work_dir; set TMPDIR to a directory on a larger disk" >&2
fi

# --- Run -------------------------------------------------------------------------------------------------

start_check() {
  name=$1
  command=$(mapping_lines | awk -F '|' -v n="$name" '$1 == n { print $2 }')
  (
    begin=$(date +%s)
    status=0
    case "$command" in
      precheck_*) $command > "$logs/$name.log" 2>&1 || status=$? ;;
      *) sh -c "$command" > "$logs/$name.log" 2>&1 < /dev/null || status=$? ;;
    esac
    echo "$status $(($(date +%s) - begin))" > "$logs/$name.done"
  ) &
  echo "  started $name"
}

running=0
started=''
total_begin=$(date +%s)
for name in $order; do
  while [ "$running" -ge "$jobs" ]; do
    sleep 1
    running=0
    for s in $started; do [ -f "$logs/$s.done" ] || running=$((running + 1)); done
  done
  start_check "$name"
  started="$started $name"
  running=$((running + 1))
done
wait

# --- Summary -------------------------------------------------------------------------------------------

failed=0
echo
printf '%-15s %-6s %8s\n' check result seconds
printf '%-15s %-6s %8s\n' --------------- ------ --------
for name in $order; do
  read -r status seconds < "$logs/$name.done"
  if [ "$status" != 0 ]; then
    result=FAIL
    failed=$((failed + 1))
  elif grep -q '^SKIP: ' "$logs/$name.log"; then
    result=skip
  else
    result=ok
  fi
  printf '%-15s %-6s %8s\n' "$name" "$result" "$seconds"
done
echo "total: $(($(date +%s) - total_begin)) s"

for name in $order; do
  if grep -q '^SKIP: ' "$logs/$name.log"; then
    echo "$name: $(grep -m 1 '^SKIP: ' "$logs/$name.log")"
  fi
done
for name in $order; do
  read -r status seconds < "$logs/$name.done"
  if [ "$status" != 0 ]; then
    echo
    echo "=== $name failed (exit $status), the end of $logs/$name.log:"
    tail -n 25 "$logs/$name.log"
  fi
done
if [ -s "$logs/clippy-first-party.txt" ] && [ "$failed" = 0 ]; then
  echo "precheck: clippy has $(wc -l < "$logs/clippy-first-party.txt" | tr -d ' ') warnings in first-party files this change does not touch (see $logs/xwin.log)"
  keep=1
fi

trap - EXIT
if [ "$failed" != 0 ]; then
  echo
  echo "precheck: $failed checks failed; logs in $logs"
  exit 1
fi
if [ "$keep" = 1 ]; then
  echo "precheck: logs in $logs"
else
  rm -rf "$logs"
fi
echo "precheck: all checks passed"

#!/bin/sh
# Holds the Rust host's copies of tables and inline functions of libaom's headers
# (crates/rav1g/src/host/tables.rs, blocks.rs) against the headers of libav1, on a Linux host (gcc,
# cargo, no GPU). dump.c, compiled against the headers, prints every table and the functions over a
# range of inputs; tools/rav1g-tables prints the same lines from the copies, which are the host's own
# files compiled into it by path. The two texts must be equal. No C becomes part of the decoder's
# build by this: the C program is built here and nowhere else.
#
#   sh testing/tables_check/run.sh            the check; exit code 0 = identical
#   sh testing/tables_check/run.sh --mutate   also proves the check can fail: copies of the host's files
#                                             with one number or one result changed must all be rejected
set -eu

here=$(cd "$(dirname "$0")" && pwd)
src="$here/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
build_timeout=600
run_timeout=60

mutate=0
if [ "${1:-}" = "--mutate" ]; then
  mutate=1
  shift
fi
if [ $# != 0 ]; then
  echo "usage: sh testing/tables_check/run.sh [--mutate]" >&2
  exit 2
fi

# The reference: libaom's headers of this tree.
gcc -std=c11 -O1 -Wall -Wextra -DNDEBUG -I "$src/libav1" -I "$src/libav1/build" \
  -o "$work/dump" "$here/dump.c"
timeout -k 10 "$run_timeout" "$work/dump" > "$work/reference.txt"
tables=$(grep -c '^table ' "$work/reference.txt")
results=$(grep -vc '^table ' "$work/reference.txt")
if [ "$tables" != 22 ] || [ "$results" -lt 45000 ]; then
  echo "tables_check: the reference has $tables tables and $results function results" >&2
  exit 1
fi

# $1: the root of the workspace, $2: its target directory, further arguments to cargo. Builds the
# program that prints the lines of the copies: $2/debug/rav1g-tables.
build() {
  root=$1
  target=$2
  shift 2
  CARGO_TARGET_DIR="$target" timeout -k 10 "$build_timeout" \
    cargo build --quiet --locked "$@" --manifest-path "$root/Cargo.toml" -p rav1g-tables
}

target="${CARGO_TARGET_DIR:-$src/target}"
build "$src" "$target"
timeout -k 10 "$run_timeout" "$target/debug/rav1g-tables" > "$work/copies.txt"
if ! cmp -s "$work/reference.txt" "$work/copies.txt"; then
  echo "tables_check: the copies of the Rust host are not what libaom's headers have" >&2
  echo "(< dump.c with the headers of libav1, > tools/rav1g-tables with crates/rav1g/src/host)" >&2
  diff "$work/reference.txt" "$work/copies.txt" | cut -c 1-300 | head -n 20 >&2 || true
  exit 1
fi
echo "tables_check: $tables tables and $results results of 4 inline functions: the copies of the Rust host" \
  "are those of libaom's headers"

if [ "$mutate" = 1 ]; then
  # The workspace as a copy, so that a file of the host can be changed: what cargo reads for this
  # program (the manifests of all members, the sources of the host and of the two tools, and of libav1
  # what the host's build script reads).
  mkdir "$work/tree" "$work/tree/third_party"
  cp "$src/Cargo.toml" "$src/Cargo.lock" "$work/tree/"
  cp -r "$src/crates" "$src/tools" "$src/libav1" "$work/tree/"
  cp -r "$src/third_party/rav1d" "$work/tree/third_party/"
  host="$work/tree/crates/rav1g/src/host"
  mkdir "$work/pristine"
  cp "$host/tables.rs" "$host/blocks.rs" "$work/pristine/"

  # file @ sed expression @ what it breaks
  mutants='tables.rs@s/^    \[4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 32, 64, 64, 64, 128, 128, 4, 16, 8, 32, 16, 64\];/    [4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 32, 64, 64, 64, 128, 128, 4, 16, 8, 32, 16, 32];/@block_size_wide: the width of the last block size
tables.rs@s/0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 1, 2, 16, 17, 18, 19\];/0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 1, 2, 16, 17, 18, 18];/@ss_size_lookup[][1][1]: the chroma block of the last block size
tables.rs@s/^    0, 0, 0, 0, 1, 0, 255, 0, 2, 255, 0, 0, 3, 2, 1, 0,/    0, 0, 0, 0, 1, 0, 255, 0, 2, 255, 0, 0, 3, 1, 2, 0,/@ss_size_lookup: two neighbours swapped
tables.rs@s/^    \[0, 5, 6, 1, 7, 8, 2, 9, 10, 3, 11, 12, 4, 4, 4, 4, 13, 14, 15, 16, 17, 18\];/    [0, 5, 6, 1, 7, 8, 2, 9, 10, 3, 11, 12, 4, 4, 4, 3, 13, 14, 15, 16, 17, 18];/@max_txsize_rect_lookup: the transform of 128x128
tables.rs@s/^    0, 0, 255, 128, 255, 149, 85, 64,/    0, 0, 255, 128, 255, 148, 85, 64,/@sm_weight_arrays: one weight
tables.rs@s/TX_SIZE_HIGH_LOG2: \[i32; 19\] = \[2, 3, 4, 5, 6, 3, 2, 4,/TX_SIZE_HIGH_LOG2: [i32; 19] = [2, 3, 4, 5, 6, 3, 2, 3,/@tx_size_high_log2: one height
tables.rs@s/QUANT_DIST_LOOKUP_TABLE: \[i32; 16\] = \[9, 7, 11, 5,/QUANT_DIST_LOOKUP_TABLE: [i32; 16] = [7, 9, 11, 5,/@quant_dist_lookup_table: the two weights of the first pair swapped
tables.rs@s/^    0, 0, 0, 128, 0, 0, 0, 0, 0, 2, -6, 126, 8, -2, 0, 0, 0, 2, -10, 122, 18, -4, 0, 0, 0, 2, -12, 116, 28, -8, 2, 0,/    0, 0, 0, 128, 0, 0, 0, 0, 0, 2, -6, 126, 8, -2, 0, 0, 0, 2, -10, 122, 18, -4, 0, 0, 0, 2, -12, 116, 28, -6, 0, 0,/@av1_sub_pel_filters_8: two taps of one kernel (their sum kept)
tables.rs@s/ 126, -6, 2, -2,$/ 126, -6, 2, 2,/@av1_sub_pel_filters_8sharp: the sign of the last tap
tables.rs@s/^    0, 24, 104, 0, 0, 0, 0, 0, 0, 16, 112, 0, 0, 0, 0, 0, 0, 8, 120, 0, 0, 0,$/    0, 24, 104, 0, 0, 0, 0, 0, 0, 16, 112, 0, 0, 0, 0, 0, 0, 0, 8, 120, 0, 0,/@av1_bilinear_filters: the last kernel one tap to the right
blocks.rs@s/B4X16 => BLOCK_8X16 as u8,/B4X16 => BLOCK_8X8 as u8,/@scale_chroma_bsize: the chroma block of 4x16
blocks.rs@s/T64X16 => TX_32X16 as u8,/T64X16 => TX_32X32 as u8,/@av1_get_max_uv_txsize: the chroma transform of a 64x16 plane block
blocks.rs@s/    let plane_bsize = SS_SIZE_LOOKUP\[(bsize as usize \* 4) + (subsampling_x as usize \* 2) + subsampling_y as usize\];/    let plane_bsize = SS_SIZE_LOOKUP[(bsize as usize * 4) + subsampling_x as usize + (subsampling_y as usize * 2)];/@av1_get_max_uv_txsize: the two subsamplings swapped
blocks.rs@s/(BLOCK_8X8 as u8..=BLOCK_32X32 as u8).contains(&sb_type)/(BLOCK_8X8 as u8..BLOCK_32X32 as u8).contains(\&sb_type)/@is_interintra_pred: 32x32 left out
blocks.rs@s/    ref_frame\[0\] > INTRA$/    ref_frame[0] >= INTRA/@is_interintra_pred: an intra block taken for one
blocks.rs@s/(SINGLE_INTER_MODE_START as u8..SINGLE_INTER_MODE_END as u8).contains(&mode)/(SINGLE_INTER_MODE_START as u8..=SINGLE_INTER_MODE_END as u8).contains(\&mode)/@is_interintra_pred: the first compound mode taken for a single one
blocks.rs@s/filters\[usize::from(x_filter)\] & 0xf/filters[usize::from(x_filter)] \& 0x7/@av1_extract_interp_filter: three bits of the filter
blocks.rs@s/filters\[usize::from(x_filter)\] & 0xf/filters[usize::from(!x_filter)] \& 0xf/@av1_extract_interp_filter: the two directions swapped'
  echo "$mutants" | while IFS='@' read -r file expr what; do
    cp "$work/pristine/tables.rs" "$work/pristine/blocks.rs" "$host/"
    sed -e "$expr" "$work/pristine/$file" > "$host/$file"
    if cmp -s "$work/pristine/$file" "$host/$file"; then
      echo "tables_check: mutation '$what' did not apply" >&2
      exit 1
    fi
    # A mutant that does not build is not a mutant that was rejected. One that builds is rejected by
    # lines that differ, or by not printing them (an index outside of a table).
    if ! build "$work/tree" "$work/target" --offline 2> "$work/mutant.log"; then
      cat "$work/mutant.log" >&2
      echo "tables_check: the mutant '$what' did not build" >&2
      exit 1
    fi
    if ! timeout -k 10 "$run_timeout" "$work/target/debug/rav1g-tables" > "$work/mutant.txt" 2>> "$work/mutant.log"; then
      echo "tables_check: mutant rejected as expected: $what (the program ended with an error)"
      continue
    fi
    if cmp -s "$work/reference.txt" "$work/mutant.txt"; then
      echo "tables_check: the mutant '$what' passed, the check is not sensitive to it" >&2
      exit 1
    fi
    echo "tables_check: mutant rejected as expected: $what ($(diff "$work/reference.txt" "$work/mutant.txt" | grep -c '^>') lines differ)"
  done
fi

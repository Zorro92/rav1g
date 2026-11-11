#!/bin/sh
# Builds and runs the sequence header peek check on a Linux host (gcc and g++, no GPU).
#
#   sh testing/seq_header_check/run.sh            the check; exit code 0 = every value read correctly
#   sh testing/seq_header_check/run.sh --mutate   also proves the check can fail: copies of the peek with one
#                                                 field of the syntax left out must be rejected
set -eu

here=$(cd "$(dirname "$0")" && pwd)
libav1="$here/../../libav1"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The decoder's own OBU header reader and what it needs, as C.
for source in av1/common/obu_util.c aom_dsp/bitreader_buffer.c aom/src/aom_integer.c; do
  gcc -std=c99 -O2 -I "$libav1" -I "$libav1/build" -c -o "$work/$(basename "$source" .c).o" "$libav1/$source"
done

# $1: directory that is searched for headers first, $2: the program
build() {
  g++ -std=c++14 -O2 -Wall -Wextra -I "$1" -I "$libav1" -I "$libav1/build" -o "$2" \
    "$here/seq_header_check.cpp" "$work/obu_util.o" "$work/bitreader_buffer.o" "$work/aom_integer.o"
}

build "$here" "$work/seq_header_check"
"$work/seq_header_check"

if [ "${1:-}" = "--mutate" ]; then
  peek="$libav1/av1/common/seq_header_peek.h"
  number=0
  # Each mutation drops one read of the syntax: the two flags the reader before the fix did not know, the
  # tier bit, the per-operating-point display delay, and the 12-bit flag.
  for mutation in \
    's/out->timing_info_present = (int)av1_seq_peek_bits(rb, 1);/out->timing_info_present = 0;/' \
    's/out->initial_display_delay_present = (int)av1_seq_peek_bits(rb, 1);/out->initial_display_delay_present = 0;/' \
    's/const int tier = level > 7 ? (int)av1_seq_peek_bits(rb, 1) : 0;/const int tier = 0;/' \
    's/if (av1_seq_peek_bits(rb, 1)) av1_seq_peek_bits(rb, 4); \/\* initial_display_delay_minus_1 \*\//av1_seq_peek_bits(rb, 1);/' \
    's/if (out->profile == 2 \&\& av1_seq_peek_bits(rb, 1)) out->bit_depth = 12;/if (out->profile == 2) out->bit_depth = 12;/'; do
    number=$((number + 1))
    mutant="$work/mutant$number"
    mkdir -p "$mutant/av1/common"
    sed -e "$mutation" "$peek" > "$mutant/av1/common/seq_header_peek.h"
    if cmp -s "$peek" "$mutant/av1/common/seq_header_peek.h"; then
      echo "seq_header_check: mutation $number did not apply" >&2
      exit 1
    fi
    build "$mutant" "$mutant/seq_header_check"
    if "$mutant/seq_header_check" > "$mutant/log" 2>&1; then
      echo "seq_header_check: mutated peek $number passed, the check is not sensitive" >&2
      exit 1
    fi
    echo "seq_header_check: mutated peek $number rejected as expected ($(tail -n 1 "$mutant/log"))"
  done
fi

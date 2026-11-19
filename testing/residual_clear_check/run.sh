#!/bin/sh
# Run the residual-layout and clear-range tests without the Windows decoder.
# Pass --mutate to verify that the reuse test catches deliberately wrong ranges.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
source="$here/../../crates/rav1g/src/host/residual.rs"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

build() {
  "${RUSTC:-rustc}" --edition 2024 --test "$1" -o "$work/check"
}
build "$source"
"$work/check"

expect_rejected() {
  sed "$2" "$source" > "$work/mutant.rs"
  build "$work/mutant.rs"
  status=0
  "$work/check" --exact tests::clears_active_planes_on_slot_reuse > "$work/mutant.log" 2>&1 || status=$?
  if [ "$status" != 101 ] || ! grep -q '^test tests::clears_active_planes_on_slot_reuse .* FAILED' "$work/mutant.log"; then
    cat "$work/mutant.log" >&2
    echo "residual_clear_check: expected a mismatch for $1" >&2
    exit 1
  fi
  echo "residual_clear_check: rejected $1"
}

if [ "${1:-}" = "--mutate" ]; then
  expect_rejected 'missing V plane' 's/v_offset as usize + (v_offset - u_offset) as usize/v_offset as usize + 0 * u_offset as usize/'
  expect_rejected 'off-by-one work item' 's/(bytes \/ size_of::<\[u32; 4\]>()) as c_int/(bytes \/ size_of::<[u32; 4]>()) as c_int - 1/'
fi

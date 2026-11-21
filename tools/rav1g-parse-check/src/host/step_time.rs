// SPDX-License-Identifier: BSD-2-Clause

//! The intra step timing of the host (`crates/rav1g/src/host/step_time.rs`): counts of the transform blocks
//! by step for GPU timestamps. This program has no GPU: its tiles never count (`av1_tile_data::perf_steps`
//! stays null), so `blocks.rs` never calls these.

use std::ffi::{c_int, c_void};

/// Never called here: the tiles of this program have no counts.
pub(crate) fn kind(_mode: u32, _delta: i8, _filter: bool, _intrabc: bool, _inter_intra: bool) -> usize {
    unreachable!("this program has no intra step timing");
}

/// Never called here.
///
/// # Safety
/// None: the function does not return.
pub(crate) unsafe fn count(_counts: *mut c_void, _iter: c_int, _plane: usize, _kind: usize, _filter_diagonals: u32) {
    unreachable!("this program has no intra step timing");
}

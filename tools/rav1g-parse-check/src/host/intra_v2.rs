// SPDX-License-Identifier: BSD-2-Clause

//! `-intrav2` of the host (`crates/rav1g/src/host/intra_v2.rs`): a schedule in D3D12 buffers. This
//! program's frames never take that path (`av1_frame_thread_data::intra_v2` stays 0).

use std::ffi::c_int;

use super::types::IntraV2Sched;

/// Never called here.
///
/// # Safety
/// None: the function does not return.
#[expect(clippy::too_many_arguments, reason = "the signature of the host's function")]
pub(crate) unsafe fn place(
    _s: &IntraV2Sched,
    _tile_base: u32,
    _is_uv: bool,
    _cx: c_int,
    _cy: c_int,
    _w_log: c_int,
    _h_log: c_int,
    _above_left: c_int,
    _above_count: c_int,
    _left_count: c_int,
    _cfl: bool,
    _bucket: u32,
    _index: u32,
) {
    unreachable!("this program has no -intrav2 schedule");
}

// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What `blocks.rs` of the decoder's host takes from its `libaom` module, for this program: the two
//! functions of `av1/common/reconintra.c` themselves. The decoder has them in Rust
//! (`crates/rav1g/src/host/libaom.rs`), because it no longer links the C of libav1; this program links
//! all of it, so the block lists of both parsers are counted with libaom's own functions.

use std::ffi::c_int;

use crate::sys::parser::AV1_COMMON;

unsafe extern "C" {
    /// The samples above and to the right of a transform block are decoded.
    pub(crate) fn has_top_right(
        cm: *const AV1_COMMON,
        bsize: u8,
        mi_row: c_int,
        mi_col: c_int,
        top_available: c_int,
        right_available: c_int,
        partition: u8,
        txsz: u8,
        row_off: c_int,
        col_off: c_int,
        ss_x: c_int,
        ss_y: c_int,
    ) -> c_int;
    /// The same for the samples to the left and below.
    pub(crate) fn has_bottom_left(
        cm: *const AV1_COMMON,
        bsize: u8,
        mi_row: c_int,
        mi_col: c_int,
        bottom_available: c_int,
        left_available: c_int,
        partition: u8,
        txsz: u8,
        row_off: c_int,
        col_off: c_int,
        ss_x: c_int,
        ss_y: c_int,
    ) -> c_int;
}

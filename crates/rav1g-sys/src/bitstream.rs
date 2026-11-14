// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The enums and limits of the AV1 bitstream that the decoder names: block sizes, transform sizes,
//! prediction modes, reference frames, restoration types. The values of `av1/common/enums.h` and its
//! neighbours in libaom, which are those of the AV1 specification.
//!
//! The constants are `c_int`, as the enumerators of C were; a record stores one in the integer type
//! named for its enum ([`BLOCK_SIZE`], [`TX_SIZE`], ...).

use std::ffi::c_int;

// The integer types the records and the state of the host store the enums in.
pub type tran_low_t = i32;
pub type BLOCK_SIZE = u8;
pub type PARTITION_TYPE = u8;
pub type TX_SIZE = u8;
pub type PREDICTION_MODE = u8;
pub type UV_PREDICTION_MODE = u8;
pub type MOTION_MODE = u8;
pub type INTERINTRA_MODE = u8;
pub type COMPOUND_TYPE = u32;
pub type FILTER_INTRA_MODE = u8;
pub type MV_REFERENCE_FRAME = i8;
pub type TransformationType = u8;
pub type InterpFilters = u32;
pub type DIFFWTD_MASK_TYPE = u32;

pub const MAX_MIB_SIZE_LOG2: u32 = 5;
pub const MI_SIZE_64X64: u32 = 16;
pub const MAX_FRAME_DISTANCE: u32 = 31;
pub const SCALE_SUBPEL_BITS: u32 = 10;
pub const REF_NO_SCALE: u32 = 16384;
pub const INVALID_IDX: i32 = -1;
pub const BLOCK_4X4: c_int = 0;
pub const BLOCK_4X8: c_int = 1;
pub const BLOCK_8X4: c_int = 2;
pub const BLOCK_8X8: c_int = 3;
pub const BLOCK_8X16: c_int = 4;
pub const BLOCK_16X8: c_int = 5;
pub const BLOCK_32X32: c_int = 9;
pub const BLOCK_64X64: c_int = 12;
pub const BLOCK_4X16: c_int = 16;
pub const BLOCK_16X4: c_int = 17;
pub const BLOCK_SIZES_ALL: c_int = 22;
pub const BLOCK_SIZES: c_int = 16;
pub const PARTITION_NONE: c_int = 0;
pub const PARTITION_SPLIT: c_int = 3;
pub const PARTITION_VERT_A: c_int = 6;
pub const PARTITION_VERT_B: c_int = 7;
pub const TX_4X4: c_int = 0;
pub const TX_8X8: c_int = 1;
pub const TX_16X16: c_int = 2;
pub const TX_32X32: c_int = 3;
pub const TX_64X64: c_int = 4;
pub const TX_4X8: c_int = 5;
pub const TX_8X4: c_int = 6;
pub const TX_8X16: c_int = 7;
pub const TX_16X8: c_int = 8;
pub const TX_16X32: c_int = 9;
pub const TX_32X16: c_int = 10;
pub const TX_32X64: c_int = 11;
pub const TX_64X32: c_int = 12;
pub const TX_4X16: c_int = 13;
pub const TX_16X4: c_int = 14;
pub const TX_8X32: c_int = 15;
pub const TX_32X8: c_int = 16;
pub const TX_16X64: c_int = 17;
pub const TX_64X16: c_int = 18;
pub const TX_SIZES_ALL: c_int = 19;
pub const TX_TYPES: c_int = 16;
pub const NEARESTMV: c_int = 13;
pub const GLOBALMV: c_int = 15;
pub const NEAREST_NEARESTMV: c_int = 17;
pub const GLOBAL_GLOBALMV: c_int = 23;
pub const SINGLE_INTER_MODE_START: c_int = 13;
pub const SINGLE_INTER_MODE_END: c_int = 17;
pub const UV_CFL_PRED: c_int = 13;
pub const OBMC_CAUSAL: c_int = 1;
pub const WARPED_CAUSAL: c_int = 2;
pub const COMPOUND_AVERAGE: c_int = 0;
pub const COMPOUND_DISTWTD: c_int = 1;
pub const COMPOUND_WEDGE: c_int = 2;
pub const COMPOUND_DIFFWTD: c_int = 3;
pub const NONE_FRAME: c_int = -1;
pub const INTRA_FRAME: c_int = 0;
pub const LAST_FRAME: c_int = 1;
pub const ALTREF_FRAME: c_int = 7;
pub const EXTREF_FRAME: c_int = 8;
pub const RESTORE_NONE: c_int = 0;
pub const RESTORE_WIENER: c_int = 1;
pub const RESTORE_SGRPROJ: c_int = 2;
pub const RESTORE_SWITCHABLE: c_int = 3;
pub type RestorationType = u8;
pub const BILINEAR: InterpFilter = 3;
pub type InterpFilter = c_int;
pub type FRAME_TYPE = u8;

// SPDX-License-Identifier: BSD-2-Clause

//! What `blocks.rs` uses of the host's `types.rs`: the names of the block lists (the host's constants,
//! read from its file by build.rs) and the two structs through which `av1_mi_push_block` reaches the
//! frame: the decoder and the frame's slot, here with the fields that function reads and nothing else.

#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::c_int;

pub use crate::sys::parser::{av1_perf_inter_blocks, av1_perf_inter_work};

include!(concat!(env!("OUT_DIR"), "/host_constants.rs"));

/// `IntraV2Sched` of the host: not used here.
#[derive(Default)]
pub struct IntraV2Sched;

/// `ScaleFactor` of the host: a reference's scale, for the counters of the timing run.
#[derive(Clone, Copy, Default)]
pub struct ScaleFactor {
    pub x_scale: c_int,
    pub x_step: c_int,
    pub y_scale: c_int,
    pub y_step: c_int,
}

/// The counters of a tile in the timing run of the host: never there in this program
/// (`av1_tile_data::perf_inter` stays null).
#[repr(C)]
pub struct av1_perf_inter_tile {
    pub work: [av1_perf_inter_work; 35],
    pub blocks: av1_perf_inter_blocks,
}

/// The slot of a frame as `av1_mi_push_block` reads it.
pub struct av1_frame_thread_data {
    /// A block with global motion is warped from this reference.
    pub gm_warp: [c_int; 8],
    /// The wavefront grids of intra prediction: luma, then chroma, and the distance of their rows.
    pub gen_intra_iter_y: *mut c_int,
    pub gen_intra_iter_uv: *mut c_int,
    pub iter_grid_stride: c_int,
    pub iter_grid_stride_uv: c_int,
    pub intra_v2: c_int,
    pub intra_v2_sched: IntraV2Sched,
    pub scale_factors: [ScaleFactor; 8],
    pub scale_enable: c_int,
}

/// The decoder of the entropy decoder of libaom in this program: the side (one of the two parsers) it
/// belongs to. `av1_mi_push_block` does not look at the decoder: it finds the slot of a block's frame
/// through the data of the block's tile.
pub struct Av1Core {
    pub side: *mut crate::plain::Side,
}

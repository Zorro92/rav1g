// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The host of the decoder: what the C++ of `libav1/dx` is in the C++ decoder (branch `cpp`), in Rust
//! (steps 3 and 4 of `docs/rust-port.md`). The modules say which C++ file their code comes from
//! (`libav1/dx/prediction.cpp`, ...): those files are on branch `cpp`, not in this tree.
//!
//! The parser is rav1d's parse pass (docs/phase2-rav1d.md): `rav1d_host` is the host of its sink, with
//! `rav1d_frames` for more than one frame in parse, and calls the functions of `frame`, `completion` and
//! `blocks` that set a frame up, hand a tile its memory, record the frame and submit it. Many of them
//! still have the names of the entry points the entropy decoder of libaom called while it was the
//! parser (`dx/av1_core.h`); that entropy decoder is not part of the decoder any more, and its
//! sources are not in the tree. The application's entry
//! points (`dx/av1_perf.h`, and the creation, decode and release that `crate::decoder` calls) are here
//! as well. The structs of the host are Rust's own (`types`); the structs of the frame header and of
//! the blocks, which the stages read and the sink writes, have the shape libav1's headers gave them and
//! are in `rav1g_sys`. What the host needs of libaom's common code is in `libaom`, and what it reads
//! of a temporal unit itself in `obu`.

// The stages index several tables of the C structs with one index, as the C++ they come from does.
#![allow(clippy::needless_range_loop)]

mod blocks;
mod completion;
pub(crate) use completion::frame_fence;
mod compute;
mod core;
mod create;
pub(crate) use completion::av1_drain_gpu_decoder;
pub(crate) use create::{
    av1_create_gpu_decoder, av1_destroy_gpu_decoder, av1_query_memory_requirements, decoder_own_bytes,
};
pub(crate) use rav1d_host::{av1_decoder_configuration, rav1g_rav1d_decode, rav1g_rav1d_drain};
pub(crate) use types::Av1Core;
// The test switches, the timing run, the stage dumps and the pipeline cache: `switches.rs`, `device.rs` and
// `capi.rs` call them.
pub(crate) use compute::{
    av1_create_pipeline_cache_handle, av1_destroy_pipeline_cache_handle, av1_wait_pipeline_cache_handle,
    set_shader_pick, shader_pick_lines,
};
pub(crate) use film_grain::{av1_set_fg_legacy, av1_set_skip_film_grain};
pub(crate) use frame::{av1_set_clears, av1_set_intra_v2, av1_shader_variant};
pub(crate) use loopfilter::{av1_set_lf_h_split, av1_set_lf_legacy};
pub(crate) use memory::{av1_log_video_memory_now, av1_set_poison_buffers};
pub(crate) use output::av1_set_copy_legacy;
pub(crate) use perf::{av1_set_inter_fast, av1_set_perf_callback, av1_set_perf_detail};
pub(crate) use postfilter::{av1_set_cdef_legacy, av1_set_cdef_lists_legacy, av1_set_cdef_v2, av1_set_lr_legacy};
pub(crate) use prediction::{
    av1_set_inter_legacy, av1_set_intra_edge_fix, av1_set_intra_filter_legacy, av1_set_intra_opt,
    av1_set_intra_schedule_legacy,
};
pub(crate) use stage_dump::av1_set_stage_dump;
pub(crate) use transform::{av1_set_clear_legacy, av1_set_idct_legacy};
mod film_grain;
mod frame;
mod gpu_pred_lists;
mod grid_check;
mod intra_v2;
mod libaom;
pub(crate) mod log;
mod loopfilter;
mod mem_stats;
pub(crate) use mem_stats::av1_set_memory_stats;
mod memory;
pub(crate) mod obu;
mod output;
mod perf;
mod pool;
mod pool_buffers;
mod postfilter;
mod prediction;
mod rav1d_frames;
mod rav1d_host;
mod residual;
mod shader_pick;
mod stage_buffers;
mod stage_dump;
mod tables;
mod thread;
mod transform;
mod types;

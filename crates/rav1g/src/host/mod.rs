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
//! parser (`dx/av1_core.h`); that entropy decoder is not part of the decoder any more, and
//! `tools/rav1g-parse-check` is the only program that implements that header. The application's entry
//! points (`dx/av1_perf.h`, and the creation, decode and release that `crate::decoder` calls) are here
//! as well. The structs of the host are Rust's own (`types`); the structs of the frame header and of
//! the blocks, which the stages read and the sink writes, are those of libav1's headers, bound in
//! `rav1g_sys::parser`. What the host needs of libaom's common code is in `libaom`, and what it reads
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
mod film_grain;
mod frame;
mod intra_v2;
mod libaom;
pub(crate) mod log;
mod loopfilter;
mod memory;
pub(crate) mod obu;
mod output;
mod perf;
mod pool;
mod postfilter;
mod prediction;
mod rav1d_frames;
mod rav1d_host;
mod stage_dump;
mod tables;
mod thread;
mod transform;
mod types;

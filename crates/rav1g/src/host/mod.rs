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
//! The entropy decoder (C) calls the host through the functions of `dx/av1_core.h`, which are `extern "C"`
//! functions here under those names (`create`, `frame`, `completion`, `blocks`), or C functions of
//! `crates/rav1g-sys/host_entry.c` where the decoder's error may have to be raised. The application's
//! entry points (`aom/aom_decoder.h`, `dx/av1_perf.h`) are here as well. The structs of the host are
//! Rust's own (`types`); what the entropy decoder shares with it is bound in `rav1g_sys::parser`.

// The stages index several tables of the C structs with one index, as the C++ they come from does.
#![allow(clippy::needless_range_loop)]

mod blocks;
mod completion;
pub(crate) use completion::frame_fence;
mod compute;
mod core;
mod create;
mod film_grain;
mod frame;
mod intra_v2;
pub(crate) mod log;
mod loopfilter;
mod memory;
mod output;
mod perf;
mod postfilter;
mod prediction;
mod stage_dump;
mod tables;
mod thread;
mod transform;
mod types;

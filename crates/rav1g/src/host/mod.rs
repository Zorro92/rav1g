// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The host of the decoder: what `libav1/dx` is in C++, module by module in Rust (step 3 of
//! `docs/rust-port.md`).
//!
//! Each module has the functions of one C++ file under the names the rest of the decoder calls them by:
//! the `extern "C"` functions of the file as they were, and for its C++ functions an `extern "C"`
//! function `rav1g_*` that the glue file of the same name in `crates/rav1g-sys/glue` calls. The structs
//! are those of the C++ host (`rav1g_sys::host`), read and written in place, until the frame pipeline
//! that owns them is Rust too (step 4).

pub(crate) mod log;
mod memory;
mod thread;

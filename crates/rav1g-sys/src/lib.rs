// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The decoder's C and C++ sources, built by `build.rs`, and their bindings.
//!
//! Everything here is the C interface as it is (`libav1/aom/aom_decoder.h`, `aom_codec.h`, `aomdx.h`,
//! `libav1/dx/av1_perf.h`) plus the `rav1g_codec_*` functions of `helpers.c`, which stand in for the
//! macros of that interface. The `rav1g` crate is the wrapper to use.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

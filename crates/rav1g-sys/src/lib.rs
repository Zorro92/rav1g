// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The decoder's C and C++ sources, built by `build.rs`, and their bindings.
//!
//! Everything at the top level is the C interface as it is (`libav1/aom/aom_decoder.h`, `aom_codec.h`, `aomdx.h`,
//! `libav1/dx/av1_perf.h`) plus the `rav1g_codec_*` functions of `helpers.c`, which stand in for the
//! macros of that interface. The `rav1g` crate is the wrapper to use.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// The layouts shared with the GPU and between the entropy decoder and the host
/// (`libav1/av1/common/blockd.h`, `mv.h`): the mode info record that the shaders read as `MB_MODE_INFO`
/// of `libav1/dx/shaders/mode_info.h`, the block records of the prediction and transform stages, and the
/// tile's cursors into the buffers of a pipeline slot. The tests of this crate hold them against the C
/// compiler of the build and against the shader's header.
pub mod shared {
    include!(concat!(env!("OUT_DIR"), "/shared.rs"));
}

/// The C++ host as the Rust host modules of the `rav1g` crate see it (`host.hpp`): the structs and
/// constants of `libav1/dx` (`Av1Core`, the data of a pipeline slot, the allocator, the shader library)
/// and the structs of the entropy decoder they hold, read as C++ with the settings of the build. A COM
/// pointer (`Microsoft::WRL::ComPtr`) is eight opaque bytes: the pointer. The tests of this crate hold the
/// structs the Rust reads or writes against the C++ compiler of the build. This module goes when the last
/// C++ file of the host does (step 4 of `docs/rust-port.md`).
pub mod host {
    include!(concat!(env!("OUT_DIR"), "/host.rs"));
}

#[cfg(test)]
mod layout_tests;

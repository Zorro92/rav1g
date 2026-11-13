// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The C of the decoder (the entropy decoder), built by `build.rs`, and its bindings.
//!
//! Everything at the top level is the C interface as it is (`libav1/aom/aom_decoder.h`, `aom_codec.h`, `aomdx.h`,
//! `libav1/dx/av1_perf.h`) plus the `rav1g_codec_*` functions of `helpers.c`, which stand in for the
//! macros of that interface. The `rav1g` crate is the wrapper to use.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// What built the C of this library: the compiler, its link time optimisation and the C runtime, as a
/// text ("MSVC, /GL, static runtime").
pub const C_BUILD: &str = env!("RAV1G_C_BUILD");

/// The layouts shared with the GPU and between the entropy decoder and the host
/// (`libav1/av1/common/blockd.h`, `mv.h`): the mode info record that the shaders read as `MB_MODE_INFO`
/// of `libav1/dx/shaders/mode_info.h`, the block records of the prediction and transform stages, and the
/// tile's cursors into the buffers of a pipeline slot. The tests of this crate hold them against the C
/// compiler of the build and against the shader's header.
pub mod shared {
    include!(concat!(env!("OUT_DIR"), "/shared.rs"));
}

/// The entropy decoder as the Rust host of the `rav1g` crate sees it (`parser.h`): the structs the host
/// reads and fills in place (`AV1Decoder`, `AV1_COMMON`, `MACROBLOCKD`, the state of a tile thread, the
/// frame buffer descriptions), the enums and limits of the bitstream, and the tables the host reads. The
/// tests of this crate hold the structs against the C compiler of the build.
pub mod parser {
    include!(concat!(env!("OUT_DIR"), "/parser.rs"));
}

unsafe extern "C" {
    fn rav1g_c_tables(index: u32, name: *mut *const std::ffi::c_char, size: *mut u32) -> *const std::ffi::c_void;
}

/// Inline functions of the C headers as the compiler of the build has them, for the tests of their Rust
/// ports in the `rav1g` crate (`C_FUNCTION_PROBES` in `build.rs`).
pub mod probe {
    use std::ffi::c_int;

    unsafe extern "C" {
        /// `scale_chroma_bsize` of `av1/common/onyxc_int.h`.
        pub fn rav1g_probe_scale_chroma_bsize(bsize: c_int, ss_x: c_int, ss_y: c_int) -> c_int;
        /// `av1_get_max_uv_txsize` of `av1/common/blockd.h`.
        pub fn rav1g_probe_max_uv_txsize(bsize: c_int, ss_x: c_int, ss_y: c_int) -> c_int;
        /// `is_interintra_pred` of `av1/common/blockd.h` for a mode info with these four fields.
        pub fn rav1g_probe_is_interintra_pred(sb_type: c_int, mode: c_int, ref0: c_int, ref1: c_int) -> c_int;
        /// `av1_extract_interp_filter` of `av1/common/filter.h`.
        pub fn rav1g_probe_extract_interp_filter(filters: u32, x_filter: c_int) -> c_int;
    }
}

/// The bytes of a table that is `static` in a header of the decoder, as the C compiler of the build has
/// it: for the tests that hold the Rust copy of such a table against the header. `None` for a name the
/// probe does not have (the list is `C_TABLES` in `build.rs`).
pub fn c_table(name: &str) -> Option<&'static [u8]> {
    probed_table(rav1g_c_tables, name)
}

type TableProbe = unsafe extern "C" fn(u32, *mut *const std::ffi::c_char, *mut u32) -> *const std::ffi::c_void;

fn probed_table(tables: TableProbe, name: &str) -> Option<&'static [u8]> {
    for index in 0.. {
        let mut entry_name = std::ptr::null();
        let mut size = 0u32;
        // SAFETY: the probe returns null behind its last entry, else a static table of `size` bytes and
        // its static name.
        unsafe {
            let data = tables(index, &mut entry_name, &mut size);
            if data.is_null() {
                break;
            }
            if std::ffi::CStr::from_ptr(entry_name).to_bytes() == name.as_bytes() {
                return Some(std::slice::from_raw_parts(data.cast::<u8>(), size as usize));
            }
        }
    }
    None
}

#[cfg(test)]
mod layout_tests;

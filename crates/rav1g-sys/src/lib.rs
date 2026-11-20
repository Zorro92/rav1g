// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The types of the decoder that have a fixed shape, and its shader table.
//!
//! These types were bindings to the headers of libaom and of the C++ decoder (`libav1`). They are Rust
//! now, and no header is read to build the decoder (docs/rust-port.md, "Own types, no libaom headers"):
//!
//! - [`records`]: the records the parser writes and the shaders read, and the cursors of a tile, with
//!   their layouts held by constant assertions.
//! - [`host`]: the state of the host that had the shape of libaom's decoder structs.
//! - [`bitstream`]: the enums and limits of the AV1 bitstream.
//! - [`api`]: error codes, the configuration of a decoder, the output image and the output callbacks.
//! - [`perf`]: the timing run, the stage dumps and the test switches.
//!
//! Everything is also at the top level, under the names the C had (`rav1g_sys::MB_MODE_INFO`). They live
//! in this crate because the sink of the parser (`rav1g-parse`) and the decoder (`rav1g`) both use them.
//!
//! [`shaders`] is the shader table: the bytecode of the shaders of `RAV1G_SHADER_DIR`, which `build.rs`
//! embeds for Windows as DXC wrote it. The crate has no C and compiles none.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

pub mod api;
pub mod bitstream;
pub mod host;
pub mod perf;
pub mod records;
pub mod shaders;

pub use api::*;
pub use bitstream::*;
pub use host::*;
pub use perf::*;
pub use records::*;

/// The C runtime the library is built for, as a text: "static runtime" or "DLL runtime". (Until
/// 2026-11-19 `C_BUILD`, which also named the compiler of the C the library had.)
pub const RUNTIME: &str = env!("RAV1G_RUNTIME");

/// What compiled the default shader set, for example "DXC 1.9.2602.17 cs_6_0".
pub const SHADER_SET: &str = env!("RAV1G_SHADER_SET");

/// The compiler and arguments of the two shader-pick sets (dxc, od). Empty strings without the feature.
pub const SHADER_SETS: [&str; 2] = [env!("RAV1G_SHADER_SET_DXC"), env!("RAV1G_SHADER_SET_OD")];

#[cfg(test)]
mod layout_tests;

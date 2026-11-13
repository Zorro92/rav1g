// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! rav1g: an AV1 decoder whose reconstruction runs in D3D12 compute shaders.
//!
//! The entropy decoder runs on the CPU; everything behind it (inverse transforms, prediction, the loop
//! filters, film grain) runs on the GPU, and a frame comes out in a D3D12 resource of the application: a
//! buffer to read it from, or one texture per plane to draw it with.
//!
//! ```no_run
//! use std::sync::Arc;
//! use rav1g::{Decoder, Device, Frame, ImageRequest, Output, OutputImage, Settings};
//!
//! struct Screen;
//! impl Output for Screen {
//!     fn image(&self, request: &ImageRequest) -> Option<OutputImage> {
//!         // A free image of the application for a frame of request.width x request.height.
//!         None
//!     }
//!     fn frame(&self, frame: Frame) {
//!         // The GPU has written the frame into frame.image: show it, then reuse the image.
//!     }
//! }
//!
//! # fn temporal_units() -> Vec<Vec<u8>> { Vec::new() }
//! # fn main() -> Result<(), rav1g::Error> {
//! let device = Device::new(4)?; // or Device::with_d3d12(&device, Some(&queue), 4)
//! let units = temporal_units();
//! let info = rav1g::peek_stream_info(&units[0])?;
//! let settings = Settings {
//!     width: info.width,
//!     height: info.height,
//!     bit_depth: if info.bit_depth == 8 { 8 } else { 10 },
//!     threads: 4,
//!     ..Settings::default()
//! };
//! let mut decoder = Decoder::new(&device, &settings, Arc::new(Screen))?;
//! for (index, unit) in units.iter().enumerate() {
//!     decoder.decode(unit, index as u64)?;
//! }
//! decoder.drain();
//! # Ok(())
//! # }
//! ```
//!
//! - [`Device`]: the D3D12 device and queue (the application's, or the decoder's own) with the compute
//!   pipeline states.
//! - [`Settings`]: what a decoder is created with: the frame size and bit depth of the sequence
//!   ([`peek_stream_info`]), tile threads, pipeline slots, the memory reductions, the budget callback,
//!   the operating point and the layers that are output.
//! - [`Output`]: the application's side of the output: it gives an [`OutputImage`] for every frame that
//!   will be shown and gets the [`Frame`] when the GPU has written it.
//! - [`Decoder`]: [`Decoder::decode`] per temporal unit, [`Decoder::drain`] at the end.
//! - [`switches`]: the process-wide choices of the test programs (which shader a stage uses, timing
//!   records, stage dumps, the log).
//!
//! The same interface for C is the `capi` feature: the functions of `rav1g.h` (`capi` module).
//!
//! The bitstream is read by the parse pass of rav1d ([`Parser`], docs/phase2-rav1d.md). The entropy
//! decoder that was there before it, the libaom-derived C of `libav1/av1`, built by `rav1g-sys`, is the
//! other choice of [`Settings::parser`] until the last step of phase 2; its interface (`aom_codec_*`) is
//! what this crate creates a decoder through and is not part of the interface of this crate.

mod decoder;
mod device;
mod error;
mod gpu;
mod host;
mod output;
pub mod switches;

#[cfg(feature = "capi")]
pub mod capi;

#[doc(hidden)]
pub use rav1g_sys as sys;

/// The parser of phase 2 on a stream, without a decoder: rav1d's parse pass, and what it reads folded
/// into one number (docs/phase2-rav1d.md). `rav1g-dec -parsedigest` prints it.
pub use rav1g_parse::digest as parse_digest;

pub use decoder::{Decoder, LowMemory, MemoryAvailable, MemoryRequirements, Parser, ParserOptions, Settings};
pub use device::Device;
pub use error::{Error, ErrorCode};
pub use output::{Frame, ImageFormat, ImageRequest, Output, OutputImage, Plane};

const _: () = assert!(sys::AOM_CODEC_NEEDS_RECONFIGURE == ErrorCode::NeedsReconfigure as sys::aom_codec_err_t);

/// What the sequence header of a temporal unit says about the stream.
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamInfo {
    /// Largest frame width of the sequence.
    pub width: u32,
    /// Largest frame height of the sequence.
    pub height: u32,
    /// The temporal unit starts with a key frame.
    pub is_key_frame: bool,
    /// Bits a sample: 8, 10 or 12; 0 when the temporal unit has no sequence header.
    pub bit_depth: u32,
}

/// Reads the sequence header of a temporal unit, without a decoder.
pub fn peek_stream_info(data: &[u8]) -> Result<StreamInfo, Error> {
    let mut si = sys::aom_codec_stream_info_t::default();
    // SAFETY: data is a valid buffer of data.len() bytes and si a valid aom_codec_stream_info_t.
    let code = unsafe { sys::rav1g_codec_peek_stream_info(data.as_ptr(), data.len(), &mut si) };
    if code != sys::AOM_CODEC_OK {
        return Err(Error::new(ErrorCode::from_raw(code), "the temporal unit has no sequence header that can be read"));
    }
    Ok(StreamInfo { width: si.w, height: si.h, is_key_frame: si.is_kf != 0, bit_depth: si.bit_depth })
}

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
//! decoder.drain()?;
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
//! - [`ColorInfo`], [`MasteringDisplay`], [`ContentLightLevel`]: what the stream says about the colour
//!   of its frames, in [`StreamInfo`] and with every [`Frame`]. The decoder passes it on and does not
//!   use it; the application needs it to show the frames right (docs/color.md).
//! - [`switches`]: the process-wide choices of the test programs (which shader a stage uses, timing
//!   records, stage dumps, the log).
//!
//! The same interface for C is the `capi` feature: the functions of `rav1g.h` (`capi` module).
//!
//! The bitstream is read by the parse pass of rav1d (docs/phase2-rav1d.md; `third_party/rav1d`, behind
//! the sink of `rav1g-parse`). The entropy decoder that was there before it, the libaom-derived C of
//! `libav1/av1`, is not in the decoder any more (step 6 of that document): `rav1g-sys` builds it only
//! for `tools/rav1g-parse-check`, which holds the parser against it.

mod color;
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

pub use color::{
    ChromaSamplePosition, ColorInfo, ColorPrimaries, ColorRange, ContentLightLevel, MasteringDisplay,
    MatrixCoefficients, TransferCharacteristics, color_text,
};
pub use decoder::{Configuration, Decoder, LowMemory, MemoryAvailable, MemoryRequirements, ParserOptions, Settings};
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
    /// The sequence has a luma plane only.
    pub monochrome: bool,
    /// The colour description of the sequence header: what the samples mean, for the application that
    /// shows them (docs/color.md). Every [`Frame`] has the description of its own sequence; HDR
    /// metadata is not part of a sequence header and comes with the frames.
    pub color: ColorInfo,
}

/// Reads the sequence header of a temporal unit, without a decoder.
pub fn peek_stream_info(data: &[u8]) -> Result<StreamInfo, Error> {
    let info = host::obu::peek_stream_info(data).map_err(|code| {
        Error::new(ErrorCode::from_raw(code), "the temporal unit has no sequence header that can be read")
    })?;
    Ok(StreamInfo {
        width: info.width,
        height: info.height,
        is_key_frame: info.is_key_frame,
        bit_depth: info.bit_depth,
        monochrome: info.monochrome,
        color: ColorInfo {
            description_present: info.color_description_present,
            primaries: ColorPrimaries(info.color_primaries as u8),
            transfer_characteristics: TransferCharacteristics(info.transfer_characteristics as u8),
            matrix_coefficients: MatrixCoefficients(info.matrix_coefficients as u8),
            range: if info.color_range != 0 { ColorRange::Full } else { ColorRange::Limited },
            chroma_sample_position: ChromaSamplePosition::from_raw(info.chroma_sample_position as u8),
        },
    })
}

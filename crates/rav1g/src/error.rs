// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The errors of the interface.

use std::ffi::{CStr, c_char};
use std::fmt;

use crate::sys;

/// What kind of error a call ended with. The numbers are those of the entropy decoder
/// (`aom_codec_err_t`), and those of `Rav1gStatus` in `rav1g.h`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum ErrorCode {
    /// An error without a kind of its own; also a device or a pipeline state that could not be created.
    Error = 1,
    /// Memory could not be had, or the budget guard refused the creation.
    Memory = 2,
    AbiMismatch = 3,
    Incapable = 4,
    /// The stream is not one the decoder reads.
    UnsupportedBitstream = 5,
    /// The stream uses something the decoder does not have.
    UnsupportedFeature = 6,
    /// The data of a frame is damaged or cut.
    CorruptFrame = 7,
    InvalidParam = 8,
    /// A new sequence whose frames are larger, or have more bits a sample, than the decoder was created
    /// for. Nothing of the temporal unit was decoded: drain and drop the decoder, create one for the new
    /// sequence ([`crate::peek_stream_info`] on the temporal unit), pass the temporal unit again.
    NeedsReconfigure = 10,
}

impl ErrorCode {
    pub(crate) fn from_raw(code: sys::aom_codec_err_t) -> ErrorCode {
        match code {
            sys::AOM_CODEC_MEM_ERROR => ErrorCode::Memory,
            sys::AOM_CODEC_ABI_MISMATCH => ErrorCode::AbiMismatch,
            sys::AOM_CODEC_INCAPABLE => ErrorCode::Incapable,
            sys::AOM_CODEC_UNSUP_BITSTREAM => ErrorCode::UnsupportedBitstream,
            sys::AOM_CODEC_UNSUP_FEATURE => ErrorCode::UnsupportedFeature,
            sys::AOM_CODEC_CORRUPT_FRAME => ErrorCode::CorruptFrame,
            sys::AOM_CODEC_INVALID_PARAM => ErrorCode::InvalidParam,
            sys::AOM_CODEC_NEEDS_RECONFIGURE => ErrorCode::NeedsReconfigure,
            _ => ErrorCode::Error,
        }
    }
}

/// A call that failed: the kind, the text of the kind, and what the decoder added.
#[derive(Clone, Debug)]
pub struct Error {
    pub code: ErrorCode,
    /// The text of the kind ("Corrupt frame detected", ...), or what failed.
    pub message: String,
    /// What the decoder said about this failure; empty when it said nothing. For a creation that failed
    /// this is the decoder's "MEMORY FAILED" or "MEMORY REFUSED" line.
    pub detail: String,
}

impl Error {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Error {
        Error { code, message: message.into(), detail: String::new() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.message.is_empty() { "?" } else { &self.message })?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

pub(crate) fn text(pointer: *const c_char) -> String {
    if pointer.is_null() {
        String::new()
    } else {
        // SAFETY: the decoder returns NUL-terminated strings that live as long as its context.
        unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned()
    }
}

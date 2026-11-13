// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Phase 2 of rav1g (docs/phase2-rav1d.md): the parse pass of rav1d (`third_party/rav1d`) feeding the
//! buffers the GPU stages read, in place of the entropy decoder of libaom.
//!
//! - [`sink::Sink`] is what rav1d's parser calls: it writes the mode info records, the grid, the
//!   transform block records and the coefficients, and counts the block lists, in the layouts the
//!   shaders read today. The memory comes from a [`sink::SinkHost`].
//! - [`record`] turns a block of rav1d into a mode info record.
//! - [`options`] has the switches of this parser.
//! - [`digest`] folds what the parser reads from a stream into one number, the same on every platform:
//!   what a build of the library has of this parser until the decoder runs it.
//!
//! Nothing here needs a GPU or Windows. `tools/rav1g-parse-check` runs this and the entropy decoder of
//! libaom on the same streams and holds every buffer of one against the other's.

pub mod digest;
pub mod options;
pub mod record;
pub mod sink;

pub use rav1d;

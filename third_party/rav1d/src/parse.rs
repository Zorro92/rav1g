//! rav1g: the parse pass behind a sink. This file is not part of upstream rav1d.
//!
//! Upstream decodes a frame in two passes when it has more than one frame context: a parse pass that
//! stores blocks, coefficients and palette data, and a reconstruction pass. This copy has the parse pass
//! only. What it reads from the bitstream goes to a [`ParseSink`] of the application instead of
//! `f.frame_thread`: one call per block, per coded transform block (whose coefficients are written,
//! dequantised and in scan order, where the sink says), per palette plane and per superblock.
//!
//! A [`Parser`] is the decoder: [`Parser::send`] takes the bytes of a temporal unit and returns when the
//! frames in it are parsed.

use std::sync::Arc;

use crate::c_box::CRef;
use crate::include::dav1d::data::Rav1dData;
use crate::include::dav1d::dav1d::{Rav1dDecodeFrameType, Rav1dInloopFilterType, Rav1dSettings};
use crate::include::dav1d::headers::{
    Rav1dContentLightLevel, Rav1dFrameHeader, Rav1dMasteringDisplay, Rav1dSequenceHeader,
    Rav1dWarpedMotionParams,
};
use crate::include::dav1d::picture::Rav1dPicture;
use crate::in_range::InRange;
use crate::internal::Rav1dContext;
use crate::levels::{Av1Block, BlockSize, TxfmSize, TxfmType};
use crate::{rav1d_close, rav1d_flush, rav1d_get_picture, rav1d_open, rav1d_send_data};
pub use crate::error::Rav1dError;

/// The colour description of a sequence header as it is coded: `color_primaries`,
/// `transfer_characteristics` and `matrix_coefficients` of `color_config()`, each 2 ("unspecified")
/// in a header without a description. [`Rav1dSequenceHeader`] has the three as enums, in which a value
/// the enum has no name for (one the specification reserves) is "unspecified"; an application that
/// reports the code points wants them as they are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParseColorCodes {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

impl Default for ParseColorCodes {
    /// The three "unspecified".
    fn default() -> Self {
        Self {
            primaries: 2,
            transfer: 2,
            matrix: 2,
        }
    }
}

/// The HDR metadata that goes with a frame: what upstream attaches to the frame's picture. A
/// metadata OBU of the type `METADATA_TYPE_HDR_CLL` or `METADATA_TYPE_HDR_MDCV` stays with the parser
/// from the OBU on, for every frame whose last tile group, or whose `show_existing_frame` header,
/// comes behind it, until another OBU of its type replaces it or a sequence header that differs from
/// the one before forgets both.
#[derive(Clone, Copy, Default)]
pub struct ParseHdrMetadata<'a> {
    pub content_light: Option<&'a Rav1dContentLightLevel>,
    pub mastering_display: Option<&'a Rav1dMasteringDisplay>,
}

/// The frame whose parse starts: its headers and its size in 4x4 units.
pub struct ParseFrame<'a> {
    pub seq_hdr: &'a Rav1dSequenceHeader,
    pub frame_hdr: &'a Rav1dFrameHeader,
    /// Width and height of the frame in 4x4 luma units (`mi_cols`, `mi_rows`).
    pub bw: i32,
    pub bh: i32,
    /// Bits a sample.
    pub bpc: u8,
    /// The frame is predicted with a warp from this reference under global motion
    /// (`gmv_warp_allowed`: the model is a warp and the reference is not scaled).
    pub gmv_warp_allowed: [u8; 7],
    /// The seven references: the scale of each in x and y (14 fractional bits; 0 for a reference of the
    /// frame's size) and the step that follows from it (`ScalableMotionParams`). Zero in an intra frame.
    pub ref_scale: [[i32; 2]; 7],
    pub ref_step: [[i32; 2]; 7],
    /// The bytes of the data that was sent ([`Parser::send`]) from the start of the OBU that completed
    /// the frame (its last tile group) to the end of the data.
    pub data_left: usize,
    /// The HDR metadata of the frame.
    pub hdr: ParseHdrMetadata<'a>,
}

/// A frame header with `show_existing_frame`: the frame in a reference slot is shown.
pub struct ParseShowExisting<'a> {
    /// The header that shows the frame: `existing_frame_idx` is the slot, `spatial_id` and
    /// `temporal_id` are those of its OBU.
    pub frame_hdr: &'a Rav1dFrameHeader,
    /// The frame in the slot is a key frame: every slot refers to it from here on.
    pub key: bool,
    /// As [`ParseFrame::data_left`], from the start of the OBU of this header.
    pub data_left: usize,
    /// The HDR metadata of the frame as it is shown: what the parser has at this header, not what
    /// the frame had when it was coded.
    pub hdr: ParseHdrMetadata<'a>,
}

/// A tile whose parse starts, on the thread that parses its first superblock row.
pub struct ParseTile {
    /// Index of the tile in the frame, row by row.
    pub index: usize,
    /// The tile in 4x4 luma units.
    pub col_start: i32,
    pub col_end: i32,
    pub row_start: i32,
    pub row_end: i32,
}

/// A block, after its mode info was read and before its coefficients are.
pub struct ParseBlock<'a> {
    /// Position in 4x4 luma units.
    pub x4: i32,
    pub y4: i32,
    pub bs: BlockSize,
    pub b: &'a Av1Block,
    /// An inter block of a frame with intra block copy.
    pub intrabc: bool,
    /// The local warp of the task, valid for a block with `MotionMode::Warp`.
    pub warpmv: &'a Rav1dWarpedMotionParams,
    /// The loop filter deltas at the block (`delta_lf`: vertical and horizontal luma, U, V).
    pub delta_lf: [i8; 4],
    /// The quantiser index at the block.
    pub qidx: u8,
    /// The palette colours of the block when it has palettes (`pal_sz` of the intra block).
    pub pal: &'a [[u16; 8]; 3],
    /// The index of the reference motion vector (`drl_idx`), the number of warp samples and the other
    /// values nothing downstream reads are not passed.
    pub seg_pred: bool,
}

/// A coded transform block (one with a non-zero coefficient).
pub struct ParseTxb {
    pub plane: u8,
    pub tx: TxfmSize,
    pub txtp: TxfmType,
    /// Index of the last coefficient in scan order.
    pub eob: u16,
    /// Position in 4x4 units of the plane.
    pub x4: i32,
    pub y4: i32,
}

/// The colour index map of a palette block's plane.
pub struct ParsePalette<'a> {
    /// The block: position in 4x4 luma units, and its size.
    pub x4: i32,
    pub y4: i32,
    pub bs: BlockSize,
    /// 0: luma, 1: both chroma planes.
    pub plane: u8,
    /// The colours: `[0]` for luma, `[1]` and `[2]` for chroma.
    pub pal: &'a [[u16; 8]; 3],
    pub pal_sz: u8,
    /// One index per sample, `stride` apart per row: `w` x `h` samples are the part of the block inside
    /// the frame, with the columns up to the block's width filled in.
    pub map: &'a [u8],
    pub stride: usize,
    pub w: usize,
    pub h: usize,
    /// The block's plane in samples.
    pub bw: usize,
    pub bh: usize,
}

/// A superblock, after its blocks.
pub struct ParseSuperblock<'a> {
    /// Position in 4x4 luma units.
    pub x4: i32,
    pub y4: i32,
    /// The CDEF index of each 64x64 of the superblock's 128x128 area in raster order (-1: none was coded;
    /// a 64x64 superblock uses the one of its position in that area).
    pub cdef_idx: [i8; 4],
    /// The loop restoration units read with this superblock: plane, unit column and row, and the unit.
    pub lr: &'a [ParseLrUnit],
}

/// A loop restoration unit as it was read.
#[derive(Clone, Copy, Default)]
pub struct ParseLrUnit {
    pub plane: u8,
    pub unit_x: u16,
    pub unit_y: u16,
    /// `Rav1dRestorationType` as a number: 0 none, 1 switchable, 2 Wiener, 3 SGR projection.
    pub r#type: u8,
    /// SGR: the set of parameters.
    pub sgr_idx: u8,
    /// Wiener: the three coded taps of the vertical and horizontal filter.
    pub filter_v: [i8; 3],
    pub filter_h: [i8; 3],
    /// SGR: the two weights.
    pub sgr_weights: [i8; 2],
}

/// What takes the parse of a stream. The calls of a frame come between its `frame_begin` and its
/// `frame_end`; those of a tile come from one thread at a time, in decode order.
pub trait ParseSink: Send + Sync {
    /// A sequence header was read (the thread that sends the data), before the parser takes it. `new`:
    /// it is the first one or differs from the one before, and the parser forgets its references.
    /// `Err` refuses it: the parser keeps the state it had and the data fails. `color`: the colour
    /// description as it is coded.
    fn sequence(
        &self,
        _seq_hdr: &Rav1dSequenceHeader,
        _color: ParseColorCodes,
        _new: bool,
    ) -> Result<(), ()> {
        Ok(())
    }

    /// A frame header shows the frame of a reference slot (the thread that sends the data, in decode
    /// order with the frames). `Err` fails the data; the slots stay as they are.
    fn show_existing(&self, _show: &ParseShowExisting<'_>) -> Result<(), ()> {
        Ok(())
    }

    /// The clock of [`Self::tile_time`], or 0 when nothing is timed.
    fn ticks(&self) -> i64 {
        0
    }

    /// A superblock row of a tile was parsed, with everything the sink was called for; `start` is what
    /// [`Self::ticks`] returned before it.
    fn tile_time(&self, _frame: usize, _start: i64) {}

    /// The parse of a frame starts (the thread that sends the data, frames in decode order). The
    /// number returned comes back with every call of the frame. `Err` fails the frame.
    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<usize, ()>;

    /// The parse of a tile starts.
    fn tile_begin(&self, frame: usize, tile: &ParseTile);

    /// A block. Its coded transform blocks follow.
    fn block(&self, frame: usize, tile: usize, block: &ParseBlock<'_>);

    /// A coded transform block of the last block of the tile. Returns where its coefficients go:
    /// `eob + 1` dequantised coefficients in scan order, 32 bits each (only the non-zero ones are
    /// written). Null fails the frame.
    fn txb(&self, frame: usize, tile: usize, txb: &ParseTxb) -> *mut i32;

    /// The colour index map of a palette block, before its `block` call.
    fn palette(&self, frame: usize, tile: usize, palette: &ParsePalette<'_>);

    /// A superblock is parsed.
    fn superblock(&self, frame: usize, tile: usize, sb: &ParseSuperblock<'_>);

    /// The last superblock row of a tile is parsed.
    fn tile_end(&self, frame: usize, tile: usize);

    /// The parse of the frame has ended, with or without an error. Returns whether the frame stands:
    /// `false` fails a frame whose parse was good (the sink could not take it), and the parser then
    /// treats it as it treats a frame with damaged data.
    fn frame_end(&self, frame: usize, ok: bool) -> bool;
}

/// The settings of a [`Parser`].
#[derive(Clone, Copy)]
pub struct ParseSettings {
    /// Worker threads; 1 parses on the thread that sends the data.
    pub threads: u16,
    /// Frames that may be in parse at once (at most `threads`).
    pub frames: u16,
    /// The operating point of a scalable stream.
    pub operating_point: u8,
    /// Largest frame size in samples, 0 for no limit.
    pub frame_size_limit: u32,
    /// The parse pass fills the loop filter masks and levels of upstream's filters, block by block, as
    /// upstream does. Nothing of the parse reads them and no sink is given them: `false` leaves them out.
    pub lf_masks: bool,
    /// A tile whose next superblock row waits for nothing goes on with it on the thread it is on.
    /// `false` is upstream: after every superblock row the tile goes back to the task queue, and the
    /// thread that takes it next may be another one. With one frame in parse nothing waits for the
    /// progress of a tile row by row, so the other threads are not woken for it either.
    pub tile_keeps_thread: bool,
}

impl Default for ParseSettings {
    fn default() -> Self {
        Self {
            threads: 1,
            frames: 1,
            operating_point: 0,
            frame_size_limit: 0,
            lf_masks: true,
            tile_keeps_thread: false,
        }
    }
}

/// The parse pass of rav1d with a sink.
pub struct Parser {
    c: Arc<Rav1dContext>,
}

impl Parser {
    pub fn new(settings: &ParseSettings, sink: Arc<dyn ParseSink>) -> Result<Self, Rav1dError> {
        let s = Rav1dSettings {
            n_threads: InRange::new(settings.threads.max(1)).ok_or(Rav1dError::InvalidArgument)?,
            max_frame_delay: InRange::new(settings.frames.max(1))
                .ok_or(Rav1dError::InvalidArgument)?,
            apply_grain: false,
            operating_point: InRange::new(settings.operating_point)
                .ok_or(Rav1dError::InvalidArgument)?,
            all_layers: true,
            frame_size_limit: settings.frame_size_limit,
            allocator: Default::default(),
            logger: None,
            strict_std_compliance: false,
            output_invisible_frames: false,
            inloop_filters: Rav1dInloopFilterType::all(),
            decode_frame_type: Rav1dDecodeFrameType::All,
        };
        Ok(Self {
            c: rav1d_open(&s, sink, settings.lf_masks, settings.tile_keeps_thread)?,
        })
    }

    /// Parses the frames in `data` (a temporal unit, or any run of whole OBUs). Returns when they are
    /// parsed when one frame is parsed at a time; with more, frames may still be in parse.
    ///
    /// After an error nothing of the data is left with the parser: the rest of it is dropped, with a
    /// frame whose tiles were being collected, as the decoder of upstream's tools drops it. The next
    /// call starts with its own data.
    pub fn send(&self, data: &[u8]) -> Result<(), Rav1dError> {
        let mut data = Rav1dData::wrap(CRef::Box(Box::from(data)))?;
        let result = self.send_all(&mut data);
        if result.is_err() {
            // `gen_picture` puts the data back when a picture is ready, also behind an OBU it could not
            // read; a later call would then read that OBU again before its own data.
            let state = &mut *self.c.state.try_lock().unwrap();
            state.in_0 = Default::default();
            state.out = Default::default();
            state.cache = Default::default();
            state.frame_hdr = None;
            state.tiles.clear();
            state.n_tiles = 0;
        }
        result
    }

    fn send_all(&self, data: &mut Rav1dData) -> Result<(), Rav1dError> {
        loop {
            match rav1d_send_data(&self.c, data) {
                Ok(()) | Err(Rav1dError::TryAgain) => {}
                Err(error) => return Err(error),
            }
            // The pictures are handles without samples here: taking them keeps the queue moving, and
            // the call goes on with the data the parser has kept.
            loop {
                let mut picture = Rav1dPicture::default();
                match rav1d_get_picture(&self.c, &mut picture) {
                    Ok(()) => {}
                    Err(Rav1dError::TryAgain) => break,
                    Err(error) => return Err(error),
                }
            }
            let sent = data.data.as_ref().is_none_or(|data| data.is_empty());
            if sent && self.c.state.try_lock().unwrap().in_0.data.is_none() {
                return Ok(());
            }
        }
    }

    /// Waits for the frames in parse and forgets the state of the stream (a seek, the end of a stream).
    pub fn flush(&self) {
        loop {
            let mut picture = Rav1dPicture::default();
            match rav1d_get_picture(&self.c, &mut picture) {
                Ok(()) => {}
                Err(_) => break,
            }
        }
        rav1d_flush(&self.c);
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        rav1d_close(&self.c);
    }
}

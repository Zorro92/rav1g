// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! A number for what rav1d's parse pass reads from a stream: the same on every platform, for any number
//! of threads.
//!
//! The sink here keeps nothing. It folds what the parser hands over into a 64-bit number per tile: the
//! mode info record of every block (as [`record::fill`](crate::record::fill) writes it), every coded
//! transform block and its coefficients, the palette blocks, the CDEF indexes and the loop restoration
//! units. The tiles of a frame are folded in their order in the frame, the frames in decode order.
//!
//! It is what a build of the library has of the parser of phase 2 until the decoder itself runs it
//! (docs/phase2-rav1d.md): `rav1g-dec -parsedigest` prints the number of a stream on Windows,
//! `rav1g-parse-check -digest` on Linux, and CI holds the two against each other. The check of the
//! values themselves is `tools/rav1g-parse-check`, against the entropy decoder of libaom.

use std::cell::UnsafeCell;
use std::sync::{Arc, Mutex};

use rav1d::include::dav1d::headers::{Rav1dFrameHeader, Rav1dPixelLayout};
use rav1d::parse::{
    ParseBlock, ParseFrame, ParsePalette, ParseSettings, ParseSink, ParseSuperblock, ParseTile, ParseTxb, Parser,
    Rav1dError,
};
use rav1g_sys::parser::MB_MODE_INFO;

use crate::record::{self, RecordFrame};

/// The most coefficients a transform block has (32x32 are coded of the largest transforms).
const MAX_COEFS: usize = 1024;

/// What the parse of a stream came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Digest {
    /// Frames parsed without an error, and with one.
    pub frames: u64,
    pub failed_frames: u64,
    pub blocks: u64,
    pub tx_blocks: u64,
    /// Coefficients up to the last one of each transform block.
    pub coefs: u64,
    /// The number.
    pub hash: u64,
}

/// FNV-1a over 64-bit words.
#[derive(Clone, Copy)]
struct Hash(u64);

impl Hash {
    const START: Self = Self(0xCBF2_9CE4_8422_2325);

    fn word(&mut self, word: u64) {
        self.0 = (self.0 ^ word).wrapping_mul(0x0000_0100_0000_01B3);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.word(u64::from_le_bytes(*chunk));
        }
        if !rest.is_empty() {
            let mut last = [0; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.word(u64::from_le_bytes(last));
        }
        self.word(bytes.len() as u64);
    }
}

/// A tile of the frame in parse.
struct Tile {
    hash: Hash,
    blocks: u64,
    tx_blocks: u64,
    coefs: u64,
    /// Where the parser writes the coefficients of a transform block; zero between two blocks.
    scratch: Box<[i32; MAX_COEFS]>,
    /// Coefficients of the transform block the parser is writing (0: none).
    pending: usize,
}

impl Tile {
    fn new() -> Self {
        Self { hash: Hash::START, blocks: 0, tx_blocks: 0, coefs: 0, scratch: Box::new([0; MAX_COEFS]), pending: 0 }
    }

    /// The coefficients of the last transform block are written: folds them in.
    fn flush(&mut self) {
        for coef in &mut self.scratch[..self.pending] {
            self.hash.word(*coef as u32 as u64);
            *coef = 0;
        }
        self.pending = 0;
    }
}

struct Frame {
    hdr: Rav1dFrameHeader,
    mi_cols: i32,
    mi_rows: i32,
    has_chroma: bool,
    tiles: usize,
}

/// The sink.
struct DigestSink {
    /// Written by `frame_begin`, while no tile is in parse; read by the tiles.
    frame: UnsafeCell<Option<Frame>>,
    /// The vector is written by `frame_begin`, an element by its tile's thread.
    tiles: UnsafeCell<Vec<UnsafeCell<Tile>>>,
    total: Mutex<Digest>,
}

// SAFETY: the calls of a `ParseSink` come as its contract says: `frame_begin` and `frame_end` while no
// tile of the frame is in parse, the calls of a tile from one thread at a time. The fields are written
// under that contract (see their comments).
unsafe impl Send for DigestSink {}
// SAFETY: as above.
unsafe impl Sync for DigestSink {}

impl DigestSink {
    /// # Safety
    /// Between the `frame_begin` and the `frame_end` of a frame.
    unsafe fn frame(&self) -> &Frame {
        // SAFETY: the caller's contract.
        unsafe { (*self.frame.get()).as_ref().expect("a frame is in parse") }
    }

    /// # Safety
    /// As [`Self::frame`], from the thread that parses the tile.
    #[expect(clippy::mut_from_ref, reason = "a tile's state is its thread's, by the contract of ParseSink")]
    unsafe fn tile(&self, tile: usize) -> &mut Tile {
        // SAFETY: the caller's contract.
        unsafe { &mut *(&*self.tiles.get())[tile].get() }
    }
}

impl ParseSink for DigestSink {
    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<usize, ()> {
        let count = usize::from(frame.frame_hdr.tiling.cols) * usize::from(frame.frame_hdr.tiling.rows);
        // SAFETY: no tile is in parse.
        unsafe {
            let tiles = &mut *self.tiles.get();
            while tiles.len() < count {
                tiles.push(UnsafeCell::new(Tile::new()));
            }
            for tile in tiles.iter_mut() {
                let tile = tile.get_mut();
                tile.flush();
                tile.hash = Hash::START;
                (tile.blocks, tile.tx_blocks, tile.coefs) = (0, 0, 0);
            }
            *self.frame.get() = Some(Frame {
                hdr: frame.frame_hdr.clone(),
                mi_cols: frame.bw,
                mi_rows: frame.bh,
                has_chroma: frame.seq_hdr.layout != Rav1dPixelLayout::I400,
                tiles: count,
            });
        }
        Ok(0)
    }

    fn tile_begin(&self, _frame: usize, tile: &ParseTile) {
        // SAFETY: a call of the tile's thread inside the frame.
        let state = unsafe { self.tile(tile.index) };
        for value in [tile.col_start, tile.col_end, tile.row_start, tile.row_end] {
            state.hash.word(value as u64);
        }
    }

    fn block(&self, _frame: usize, tile: usize, block: &ParseBlock<'_>) {
        // SAFETY: a call of the tile's thread inside the frame.
        let (frame, state) = unsafe { (self.frame(), self.tile(tile)) };
        state.flush();
        // SAFETY: a record is a C struct of numbers, for which zero bytes are a value.
        let mut mi: MB_MODE_INFO = unsafe { std::mem::zeroed() };
        let lossless = frame.hdr.segmentation.lossless[block.b.seg_id.get()];
        let record_frame = RecordFrame {
            hdr: &frame.hdr,
            mi_cols: frame.mi_cols,
            mi_rows: frame.mi_rows,
            has_chroma: frame.has_chroma,
        };
        record::fill(&mut mi, &record_frame, block, lossless);
        // SAFETY: the bytes of the record, whose padding is zero as the whole of it was.
        state
            .hash
            .bytes(unsafe { std::slice::from_raw_parts((&raw const mi).cast::<u8>(), size_of::<MB_MODE_INFO>()) });
        state.blocks += 1;
    }

    fn txb(&self, _frame: usize, tile: usize, txb: &ParseTxb) -> *mut i32 {
        // SAFETY: a call of the tile's thread inside the frame.
        let state = unsafe { self.tile(tile) };
        state.flush();
        for value in
            [u64::from(txb.plane), txb.tx as u64, u64::from(txb.txtp), u64::from(txb.eob), txb.x4 as u64, txb.y4 as u64]
        {
            state.hash.word(value);
        }
        state.pending = usize::from(txb.eob) + 1;
        state.tx_blocks += 1;
        state.coefs += state.pending as u64;
        state.scratch.as_mut_ptr()
    }

    fn palette(&self, _frame: usize, tile: usize, palette: &ParsePalette<'_>) {
        // SAFETY: a call of the tile's thread inside the frame.
        let state = unsafe { self.tile(tile) };
        state.flush();
        for value in [palette.x4 as u64, palette.y4 as u64, u64::from(palette.plane), u64::from(palette.pal_sz)] {
            state.hash.word(value);
        }
        let planes = if palette.plane == 0 { 0..1 } else { 1..3 };
        for colors in &palette.pal[planes] {
            for color in &colors[..usize::from(palette.pal_sz)] {
                state.hash.word(u64::from(*color));
            }
        }
        for row in 0..palette.h {
            state.hash.bytes(&palette.map[row * palette.stride..][..palette.bw]);
        }
    }

    fn superblock(&self, _frame: usize, tile: usize, superblock: &ParseSuperblock<'_>) {
        // SAFETY: a call of the tile's thread inside the frame.
        let state = unsafe { self.tile(tile) };
        state.flush();
        state.hash.word(superblock.x4 as u64);
        state.hash.word(superblock.y4 as u64);
        for index in superblock.cdef_idx {
            state.hash.word(index as u8 as u64);
        }
        for unit in superblock.lr {
            for value in
                [unit.plane.into(), unit.unit_x.into(), unit.unit_y.into(), unit.r#type.into(), u64::from(unit.sgr_idx)]
            {
                state.hash.word(value);
            }
            for value in unit.filter_v.iter().chain(&unit.filter_h).chain(&unit.sgr_weights) {
                state.hash.word(*value as u8 as u64);
            }
        }
    }

    fn tile_end(&self, _frame: usize, tile: usize) {
        // SAFETY: a call of the tile's thread inside the frame.
        unsafe { self.tile(tile) }.flush();
    }

    fn frame_end(&self, _frame: usize, ok: bool) -> bool {
        self.end_frame(ok);
        true
    }
}

impl DigestSink {
    /// The parse of the frame has ended: its tiles are folded into the digest.
    fn end_frame(&self, ok: bool) {
        let mut total = self.total.lock().expect("no thread panics with it");
        // SAFETY: no tile of the frame is in parse any more.
        unsafe {
            let Some(frame) = (*self.frame.get()).take() else { return };
            if !ok {
                total.failed_frames += 1;
                return;
            }
            let mut hash = Hash(total.hash);
            for tile in &mut (&mut *self.tiles.get())[..frame.tiles] {
                let tile = tile.get_mut();
                tile.flush();
                hash.word(tile.hash.0);
                total.blocks += tile.blocks;
                total.tx_blocks += tile.tx_blocks;
                total.coefs += tile.coefs;
            }
            total.frames += 1;
            total.hash = hash.0;
        }
    }
}

/// rav1d's parse pass with the sink that folds what it reads into a number.
pub struct DigestParser {
    parser: Parser,
    sink: Arc<DigestSink>,
}

impl DigestParser {
    /// `threads`: the threads of the parser (1: the thread that sends the data).
    pub fn new(threads: u16) -> Result<Self, Rav1dError> {
        let sink = Arc::new(DigestSink {
            frame: UnsafeCell::new(None),
            tiles: UnsafeCell::new(Vec::new()),
            total: Mutex::new(Digest { hash: Hash::START.0, ..Default::default() }),
        });
        // The parse as the decoder does it by default (`crate::options`); the switches change nothing
        // of what the sink is handed.
        let mut settings = ParseSettings { threads: threads.max(1), frames: 1, ..Default::default() };
        crate::options::ParseOptions::default().apply(&mut settings);
        Ok(Self { parser: Parser::new(&settings, sink.clone())?, sink })
    }

    /// Parses the frames of a temporal unit. An error is that of the last frame that failed; the frames
    /// behind it in the unit are dropped.
    pub fn send(&self, data: &[u8]) -> Result<(), Rav1dError> {
        self.parser.send(data)
    }

    /// What the stream came to so far.
    pub fn digest(&self) -> Digest {
        *self.sink.total.lock().expect("no thread panics with it")
    }
}

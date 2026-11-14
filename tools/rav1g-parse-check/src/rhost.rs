// SPDX-License-Identifier: BSD-2-Clause

//! The parser of phase 2 with plain memory: rav1d's parse pass, the sink of `rav1g-parse`, and a host
//! of that sink whose buffers are an [`Arena`](crate::plain::Arena) a frame.
//!
//! Frames are values here as in the decoder: every frame in parse has its arena, the tiles of several
//! frames write side by side, and a frame is a parsed frame when its parse has ended, in the order the
//! frames began (decode order), whatever the order they ended in.

use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::ffi::c_int;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rav1g_parse::options::ParseOptions;
use rav1g_parse::rav1d::include::dav1d::headers::Rav1dPixelLayout;
use rav1g_parse::rav1d::parse::{ParseFrame, ParsePalette, ParseSettings, ParseSuperblock, ParseTile, Parser};
use rav1g_parse::sink::{FrameMemory, ModeInfoPiece, Sink, SinkHost, SinkTimer, TileMemory};

use crate::plain::{Arena, FrameSize, LrUnit, Palette, TileBounds};
use crate::sys::parser::{RESTORE_NONE, RESTORE_SGRPROJ, RESTORE_WIENER, av1_tile_data};

/// A frame in parse: its place in decode order and its buffers.
struct InParse {
    order: u64,
    arena: Arena,
}

/// What the frames share, under a lock: taken at the begin and at the end of a frame.
#[derive(Default)]
struct Frames {
    /// The frames that have begun.
    begun: u64,
    /// The frames whose parse has ended, by their place in decode order; none for one that failed.
    parsed: BTreeMap<u64, Option<Arena>>,
    /// Frames whose parse ended with an error.
    failed: usize,
    /// The timing run: the frames are not kept; the arena of one is a later one's.
    recycle: bool,
    spare: Vec<Arena>,
    /// Frames parsed in the timing run.
    recycled: usize,
}

/// The host of the sink.
pub struct PlainHost {
    /// The frames keep the wavefront in the edges of their tiles, not in the grids.
    wave_edges: bool,
    /// The frames in parse, by the number the sink gave them. An entry is written by `frame_begin` and
    /// `frame_end` of its frame, while no tile of that frame is in parse; the tiles read the arena,
    /// whose buffers they write each in its own part.
    places: Box<[UnsafeCell<Option<InParse>>]>,
    frames: Mutex<Frames>,
    /// Nanoseconds spent making the arenas of the frames ready.
    arena_nanos: AtomicU64,
    /// The timing run: the clock of the sink's timers, and what the sink has measured with it, in
    /// nanoseconds: the block lists, and with `-parseopt time` the other calls of the sink.
    clock: Option<Instant>,
    list_nanos: AtomicU64,
    call_nanos: AtomicU64,
}

// SAFETY: the calls come as the contract of `SinkHost` says (see `places`).
unsafe impl Send for PlainHost {}
// SAFETY: as above.
unsafe impl Sync for PlainHost {}

impl PlainHost {
    /// # Safety
    /// Between the `frame_begin` and the `frame_end` of the frame with this number.
    unsafe fn arena(&self, frame: usize) -> &Arena {
        // SAFETY: the caller's contract: the entry is not written while tiles of the frame are in parse.
        unsafe { &(*self.places[frame].get()).as_ref().expect("the frame is in parse").arena }
    }

    fn frames(&self) -> std::sync::MutexGuard<'_, Frames> {
        self.frames.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SinkHost for PlainHost {
    fn ticks(&self) -> i64 {
        // Nanoseconds, and never 0, which says that there is no clock.
        self.clock.map_or(0, |clock| clock.elapsed().as_nanos() as i64 + 1)
    }

    fn add_time(&self, _frame: usize, timer: SinkTimer, ticks: i64) {
        let nanos = match timer {
            SinkTimer::Lists => &self.list_nanos,
            SinkTimer::Calls => &self.call_nanos,
        };
        nanos.fetch_add(ticks.max(0) as u64, Ordering::Relaxed);
    }

    fn frame_begin(&self, frame: usize, header: &ParseFrame<'_>) -> Result<FrameMemory, ()> {
        let size = FrameSize {
            mi_cols: header.bw,
            mi_rows: header.bh,
            tile_cols: header.frame_hdr.tiling.cols.into(),
            tile_rows: header.frame_hdr.tiling.rows.into(),
            has_chroma: header.seq_hdr.layout != Rav1dPixelLayout::I400,
        };
        let mut gm_warp = [0; 8];
        for (warp, allowed) in gm_warp[1..].iter_mut().zip(header.gmv_warp_allowed) {
            *warp = c_int::from(allowed);
        }
        let start = Instant::now();
        let (order, spare) = {
            let mut frames = self.frames();
            let order = frames.begun;
            frames.begun += 1;
            let spare = frames.spare.iter().position(|arena| arena.size == size).map(|at| frames.spare.swap_remove(at));
            (order, spare)
        };
        let arena = match spare {
            Some(mut arena) => {
                arena.reset(gm_warp);
                arena
            }
            None => {
                let mut arena = Arena::new(size, self.wave_edges);
                arena.set_gm_warp(gm_warp);
                arena
            }
        };
        let memory = FrameMemory { mi_grid: arena.mi_grid.ptr(), mi_stride: size.mi_stride() };
        // SAFETY: the place is this frame's and none of its tiles is in parse (the contract of
        // `SinkHost::frame_begin`).
        unsafe { *self.places[frame].get() = Some(InParse { order, arena }) };
        self.arena_nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok(memory)
    }

    fn tile_begin(&self, frame: usize, tile: &ParseTile) -> TileMemory {
        // SAFETY: a call of the tile's thread inside the frame.
        unsafe {
            let (tile_data, mi_pool, mi_count_max) = self.arena(frame).setup_tile(&TileBounds {
                index: tile.index,
                row_start: tile.row_start,
                row_end: tile.row_end,
                col_start: tile.col_start,
                col_end: tile.col_end,
            });
            // A tile has room for all its blocks, all its transform blocks and the worst-case layout of
            // its coefficients here: it never asks for more.
            TileMemory { tile_data, mi_pool, mi_count_max, txb_room: std::ffi::c_int::MAX }
        }
    }

    fn more_mode_info(&self, _frame: usize, _tile: usize, _tile_data: *mut av1_tile_data) -> Option<ModeInfoPiece> {
        None
    }

    fn more_coefs(&self, _frame: usize, _tile: usize, _tile_data: *mut av1_tile_data) -> bool {
        false
    }

    fn more_transform_blocks(&self, _frame: usize, _tile: usize, _tile_data: *mut av1_tile_data) -> std::ffi::c_int {
        0
    }

    fn palette(&self, frame: usize, tile: usize, palette: &ParsePalette<'_>) {
        let count = usize::from(palette.pal_sz);
        let colors = if palette.plane == 0 {
            palette.pal[0][..count].to_vec()
        } else {
            [&palette.pal[1][..count], &palette.pal[2][..count]].concat()
        };
        // The part of the block outside the frame: the columns are filled in by the parser, the rows
        // repeat the last one inside (`decode_color_map_tokens` of libaom).
        let mut map = Vec::with_capacity(palette.bw * palette.bh);
        for row in 0..palette.bh {
            let row = row.min(palette.h - 1);
            map.extend_from_slice(&palette.map[row * palette.stride..][..palette.bw]);
        }
        // SAFETY: a call of the tile's thread inside the frame.
        let arena = unsafe { self.arena(frame) };
        arena.palettes[tile].lock().expect("no thread panics with it").push(Palette {
            mi_row: palette.y4,
            mi_col: palette.x4,
            plane: palette.plane,
            colors,
            width: palette.bw,
            height: palette.bh,
            map,
        });
    }

    fn superblock(&self, frame: usize, _tile: usize, superblock: &ParseSuperblock<'_>) {
        if superblock.lr.is_empty() {
            return;
        }
        // SAFETY: a call of a tile's thread inside the frame.
        let arena = unsafe { self.arena(frame) };
        let mut units = arena.lr_units.lock().expect("no thread panics with it");
        for unit in superblock.lr {
            // `RestorationType` of libaom and the coefficients it keeps for the type.
            let (kind, values) = match unit.r#type {
                2 => (RESTORE_WIENER, [unit.filter_v, unit.filter_h].concat()),
                3 => (RESTORE_SGRPROJ, vec![unit.sgr_idx as i8, unit.sgr_weights[0], unit.sgr_weights[1], 0, 0, 0]),
                _ => (RESTORE_NONE, vec![0; 6]),
            };
            let mut numbers = [0; 6];
            for (number, value) in numbers.iter_mut().zip(values) {
                *number = i32::from(value);
            }
            units.push(LrUnit {
                plane: unit.plane,
                unit_x: unit.unit_x,
                unit_y: unit.unit_y,
                r#type: kind as u8,
                values: numbers,
            });
        }
    }

    fn tile_end(&self, _frame: usize, _tile: usize) {}

    fn frame_end(&self, frame: usize, ok: bool) -> bool {
        // SAFETY: no tile of the frame is in parse any more, and the place is the frame's until this
        // call returns.
        let Some(InParse { order, mut arena }) = (unsafe { (*self.places[frame].get()).take() }) else {
            return true;
        };
        let mut frames = self.frames();
        if frames.recycle {
            frames.recycled += usize::from(ok);
            frames.spare.push(arena);
        } else if ok {
            arena.ok = true;
            frames.parsed.insert(order, Some(arena));
        } else {
            // A frame that failed is not a parsed frame: its buffers are dropped.
            frames.failed += 1;
            frames.parsed.insert(order, None);
        }
        true
    }
}

/// rav1d's parse pass with the sink and the plain host.
pub struct RustParser {
    parser: Parser,
    sink: Arc<Sink<PlainHost>>,
}

impl RustParser {
    /// `frames`: the frames that may be in parse at once. `wave_edges`: the frames keep the wavefront
    /// in the edges of their tiles (the host's default), not in the grids.
    pub fn new(threads: u16, frames: u16, options: ParseOptions, wave_edges: bool) -> Result<Self, String> {
        Self::with_clock(threads, frames, options, false, wave_edges)
    }

    /// `timed`: the host has a clock, so the sink measures its block lists (and with `-parseopt time` its
    /// other calls).
    pub fn with_clock(
        threads: u16,
        frames: u16,
        options: ParseOptions,
        timed: bool,
        wave_edges: bool,
    ) -> Result<Self, String> {
        // What rav1d makes of the two numbers: at most as many frames as threads.
        let frames = frames.clamp(1, threads.max(1));
        let sink = Arc::new(Sink::with_frames(
            PlainHost {
                wave_edges,
                places: (0..frames).map(|_| UnsafeCell::new(None)).collect(),
                frames: Mutex::new(Frames::default()),
                arena_nanos: AtomicU64::new(0),
                clock: timed.then(Instant::now),
                list_nanos: AtomicU64::new(0),
                call_nanos: AtomicU64::new(0),
            },
            options.sink(),
            usize::from(frames),
        ));
        let mut settings = ParseSettings { threads, frames, ..Default::default() };
        options.apply(&mut settings);
        let parser =
            Parser::new(&settings, sink.clone()).map_err(|error| format!("rav1d was not created: {error:?}"))?;
        Ok(Self { parser, sink })
    }

    /// The frames that may be in parse at once.
    pub fn frames(&self) -> usize {
        self.parser.frames()
    }

    /// Parses the frames of a temporal unit. With more than one frame in parse they may still be in
    /// parse when this returns ([`Self::take_frames`] waits for them).
    pub fn send(&mut self, data: &[u8]) -> Result<(), String> {
        self.parser.send(data).map_err(|error| format!("{error:?}"))
    }

    /// Waits until no frame is in parse.
    pub fn wait(&mut self) {
        self.parser.wait();
    }

    /// A thread of the parser ended with a panic.
    pub fn broken(&self) -> bool {
        self.parser.broken()
    }

    /// The timing run: the frames are not kept, and the arena of a frame serves a later one.
    pub fn recycle(&mut self) {
        self.sink.host().frames().recycle = true;
    }

    /// Frames parsed in the timing run.
    pub fn recycled(&self) -> usize {
        self.sink.host().frames().recycled
    }

    /// The time spent making the arenas of the frames ready.
    pub fn arena_time(&self) -> Duration {
        Duration::from_nanos(self.sink.host().arena_nanos.load(Ordering::Relaxed))
    }

    /// Nanoseconds the tiles spent counting their block lists, in a parser with a clock.
    pub fn block_list_nanos(&self) -> u64 {
        self.sink.host().list_nanos.load(Ordering::Relaxed)
    }

    /// Nanoseconds the tiles spent in the other calls of the sink, with a clock and `-parseopt time`.
    pub fn sink_call_nanos(&self) -> u64 {
        self.sink.host().call_nanos.load(Ordering::Relaxed)
    }

    /// The frames parsed since the last call, in decode order, once no frame is in parse any more.
    pub fn take_frames(&mut self) -> Vec<Arena> {
        self.parser.wait();
        std::mem::take(&mut self.sink.host().frames().parsed).into_values().flatten().collect()
    }
}

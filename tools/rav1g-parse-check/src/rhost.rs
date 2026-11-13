// SPDX-License-Identifier: BSD-2-Clause

//! The parser of phase 2 with plain memory: rav1d's parse pass, the sink of `rav1g-parse`, and a host
//! of that sink whose buffers are an [`Arena`](crate::plain::Arena).

use std::cell::UnsafeCell;
use std::ffi::{c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rav1g_parse::options::ParseOptions;
use rav1g_parse::rav1d::include::dav1d::headers::Rav1dPixelLayout;
use rav1g_parse::rav1d::parse::{ParseFrame, ParsePalette, ParseSettings, ParseSuperblock, ParseTile, Parser};
use rav1g_parse::sink::{FrameMemory, SecondModeInfo, Sink, SinkHost, SinkTimer, TileMemory};

use crate::plain::{Arena, FrameSize, LrUnit, Palette, Side, TileBounds};
use crate::sys::parser::{RESTORE_NONE, RESTORE_SGRPROJ, RESTORE_WIENER, av1_tile_data};

/// The host of the sink.
pub struct PlainHost {
    /// Written by `frame_begin` and `frame_end`, while no tile is in parse; the tiles read the arena
    /// of the frame, whose buffers they write each in its own part.
    side: UnsafeCell<Box<Side>>,
    /// Nanoseconds spent making the arenas of the frames ready.
    arena_nanos: AtomicU64,
    /// The timing run: the clock of the sink's timers, and what the sink has measured with it, in
    /// nanoseconds: the block lists, and with `-parseopt time` the other calls of the sink.
    clock: Option<Instant>,
    list_nanos: AtomicU64,
    call_nanos: AtomicU64,
}

// SAFETY: the calls come as the contract of `SinkHost` says (see the field).
unsafe impl Send for PlainHost {}
// SAFETY: as above.
unsafe impl Sync for PlainHost {}

impl PlainHost {
    /// # Safety
    /// Between the `frame_begin` and the `frame_end` of a frame.
    unsafe fn arena(&self) -> &Arena {
        // SAFETY: the caller's contract: the side is not written while tiles are in parse.
        unsafe { (*self.side.get()).arena.as_ref().expect("a frame is in parse") }
    }
}

impl SinkHost for PlainHost {
    fn gpu_decoder(&self) -> *mut c_void {
        // SAFETY: the address of a field of the boxed side.
        unsafe { (&raw mut (*self.side.get()).core).cast() }
    }

    fn ticks(&self) -> i64 {
        // Nanoseconds, and never 0, which says that there is no clock.
        self.clock.map_or(0, |clock| clock.elapsed().as_nanos() as i64 + 1)
    }

    fn add_time(&self, timer: SinkTimer, ticks: i64) {
        let nanos = match timer {
            SinkTimer::Lists => &self.list_nanos,
            SinkTimer::Calls => &self.call_nanos,
        };
        nanos.fetch_add(ticks.max(0) as u64, Ordering::Relaxed);
    }

    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<FrameMemory, ()> {
        let size = FrameSize {
            mi_cols: frame.bw,
            mi_rows: frame.bh,
            tile_cols: frame.frame_hdr.tiling.cols.into(),
            tile_rows: frame.frame_hdr.tiling.rows.into(),
            has_chroma: frame.seq_hdr.layout != Rav1dPixelLayout::I400,
        };
        let mut gm_warp = [0; 8];
        for (warp, allowed) in gm_warp[1..].iter_mut().zip(frame.gmv_warp_allowed) {
            *warp = c_int::from(allowed);
        }
        let start = Instant::now();
        // SAFETY: no tile is in parse (the contract of `SinkHost::frame_begin`).
        let arena = unsafe { (*self.side.get()).begin_frame(size, gm_warp) };
        self.arena_nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok(FrameMemory { mi_grid: arena.mi_grid.ptr(), mi_stride: size.mi_stride() })
    }

    fn tile_begin(&self, tile: &ParseTile) -> TileMemory {
        // SAFETY: a call of the tile's thread inside the frame.
        unsafe {
            let (tile_data, mi_pool, mi_count_max) = self.arena().setup_tile(&TileBounds {
                index: tile.index,
                row_start: tile.row_start,
                row_end: tile.row_end,
                col_start: tile.col_start,
                col_end: tile.col_end,
            });
            // A tile has room for all its blocks and the worst-case layout of its coefficients here.
            TileMemory { tile_data, mi_pool, mi_count_max, worst_case_coefs: true }
        }
    }

    fn second_mode_info(&self, _tile: usize, _tile_data: *mut av1_tile_data) -> Option<SecondModeInfo> {
        None
    }

    fn second_coefs(&self, _tile: usize, _tile_data: *mut av1_tile_data) {}

    fn palette(&self, tile: usize, palette: &ParsePalette<'_>) {
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
        let arena = unsafe { self.arena() };
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

    fn superblock(&self, _tile: usize, superblock: &ParseSuperblock<'_>) {
        if superblock.lr.is_empty() {
            return;
        }
        // SAFETY: a call of a tile's thread inside the frame.
        let arena = unsafe { self.arena() };
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

    fn tile_end(&self, _tile: usize) {}

    fn frame_end(&self, ok: bool) -> bool {
        // SAFETY: no tile is in parse any more.
        unsafe { (*self.side.get()).end_frame(ok) };
        true
    }
}

/// rav1d's parse pass with the sink and the plain host.
pub struct RustParser {
    parser: Parser,
    sink: Arc<Sink<PlainHost>>,
}

impl RustParser {
    pub fn new(threads: u16, options: ParseOptions) -> Result<Self, String> {
        Self::with_clock(threads, options, false)
    }

    /// `timed`: the host has a clock, so the sink measures its block lists (and with `-parseopt time` its
    /// other calls).
    pub fn with_clock(threads: u16, options: ParseOptions, timed: bool) -> Result<Self, String> {
        let sink = Arc::new(Sink::with_options(
            PlainHost {
                side: UnsafeCell::new(Side::new()),
                arena_nanos: AtomicU64::new(0),
                clock: timed.then(Instant::now),
                list_nanos: AtomicU64::new(0),
                call_nanos: AtomicU64::new(0),
            },
            options.sink(),
        ));
        let mut settings = ParseSettings { threads, frames: 1, ..Default::default() };
        options.apply(&mut settings);
        let parser =
            Parser::new(&settings, sink.clone()).map_err(|error| format!("rav1d was not created: {error:?}"))?;
        Ok(Self { parser, sink })
    }

    /// Parses the frames of a temporal unit.
    pub fn send(&mut self, data: &[u8]) -> Result<(), String> {
        self.parser.send(data).map_err(|error| format!("{error:?}"))
    }

    /// The timing run: the frames are not kept, and one arena serves them all.
    pub fn recycle(&mut self) {
        // SAFETY: no frame is in parse.
        unsafe { (*self.sink.host().side.get()).recycle = true };
    }

    /// Frames parsed in the timing run.
    pub fn recycled(&self) -> usize {
        // SAFETY: no frame is in parse.
        unsafe { (*self.sink.host().side.get()).recycled }
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

    /// The frames parsed since the last call.
    pub fn take_frames(&mut self) -> Vec<Arena> {
        // SAFETY: `send` has returned, and one frame is parsed at a time: no frame is in parse.
        unsafe { std::mem::take(&mut (*self.sink.host().side.get()).frames) }
    }
}

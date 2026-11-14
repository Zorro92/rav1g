// SPDX-License-Identifier: BSD-2-Clause

//! `-time`: what the sink costs on the CPU.
//!
//! The stream is parsed twice by rav1d's parse pass: with a sink that does nothing (the coefficients go
//! to a scratch block), and with the sink of `rav1g-parse` writing the buffers of the GPU stages into
//! plain memory. The difference is the sink: the mode info records, the grid, the transform block
//! records, the coefficients written to their buffer, and the block lists (`av1_mi_push_block`, which
//! the tile threads of the entropy decoder of libaom run today as well and which is timed by itself).
//!
//! The second run uses one arena for all frames (cleared between frames, outside the time measured), so
//! the parser writes memory the process already has, as it would write mapped buffers.

use std::cell::UnsafeCell;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rav1g_parse::rav1d::parse::{
    ParseBlock, ParseFrame, ParsePalette, ParseSettings, ParseSink, ParseSuperblock, ParseTile, ParseTxb, Parser,
};

use rav1g_parse::options::ParseOptions;

use crate::container;
use crate::rhost::RustParser;

/// The largest number of coefficients a transform block has (32x32 coded).
const MAX_COEFS: usize = 1024;

/// A sink that keeps nothing.
struct NullSink {
    /// For every frame that may be in parse at once: whether a frame has the place, and a block of
    /// coefficients per tile, which the parser writes and nothing reads.
    places: Box<[(AtomicBool, UnsafeCell<Vec<Box<[i32; MAX_COEFS]>>>)]>,
    frames: AtomicU64,
    width: AtomicU64,
    height: AtomicU64,
}

// SAFETY: the vector of a place is written by the `frame_begin` of the frame that has the place, while
// none of its tiles is in parse; a tile's thread writes the tile's own block.
unsafe impl Send for NullSink {}
// SAFETY: as above.
unsafe impl Sync for NullSink {}

impl ParseSink for NullSink {
    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<usize, ()> {
        let tiles = usize::from(frame.frame_hdr.tiling.cols) * usize::from(frame.frame_hdr.tiling.rows);
        let number = self
            .places
            .iter()
            .position(|(taken, _)| taken.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
            .ok_or(())?;
        // SAFETY: the place is this frame's and none of its tiles is in parse.
        unsafe {
            let scratch = &mut *self.places[number].1.get();
            while scratch.len() < tiles {
                scratch.push(Box::new([0; MAX_COEFS]));
            }
        }
        self.width.store(frame.bw as u64 * 4, Ordering::Relaxed);
        self.height.store(frame.bh as u64 * 4, Ordering::Relaxed);
        Ok(number)
    }

    fn tile_begin(&self, _frame: usize, _tile: &ParseTile) {}

    fn block(&self, _frame: usize, _tile: usize, _block: &ParseBlock<'_>) {}

    fn txb(&self, frame: usize, tile: usize, _txb: &ParseTxb) -> *mut i32 {
        // SAFETY: the tile's own block, on the tile's thread.
        unsafe { (&*self.places[frame].1.get())[tile].as_ptr().cast_mut() }
    }

    fn palette(&self, _frame: usize, _tile: usize, _palette: &ParsePalette<'_>) {}

    fn superblock(&self, _frame: usize, _tile: usize, _superblock: &ParseSuperblock<'_>) {}

    fn tile_end(&self, _frame: usize, _tile: usize) {}

    fn frame_end(&self, frame: usize, ok: bool) -> bool {
        self.frames.fetch_add(u64::from(ok), Ordering::Relaxed);
        self.places[frame].0.store(false, Ordering::Release);
        true
    }
}

fn milliseconds(duration: Duration, frames: u64) -> f64 {
    duration.as_secs_f64() * 1000.0 / frames.max(1) as f64
}

/// `wave_edges`: the wavefront in the edges of the tiles (the default of the decoder), not in the grids.
pub fn run(path: &Path, threads: u16, frames_in_parse: u16, options: ParseOptions, wave_edges: bool) -> ExitCode {
    // What rav1d makes of the two numbers: at most as many frames as threads.
    let frames_in_parse = frames_in_parse.clamp(1, threads.max(1));
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("rav1g-parse-check: cannot read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let units = match container::read(&data) {
        Ok(units) => units,
        Err(error) => {
            eprintln!("rav1g-parse-check: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    const RUNS: usize = 3;

    // The parse alone: the best of the runs.
    let mut parse = Duration::MAX;
    let mut frames = 0;
    let (mut width, mut height) = (0, 0);
    for _ in 0..RUNS {
        let sink = Arc::new(NullSink {
            places: (0..frames_in_parse).map(|_| (AtomicBool::new(false), UnsafeCell::new(Vec::new()))).collect(),
            frames: AtomicU64::new(0),
            width: AtomicU64::new(0),
            height: AtomicU64::new(0),
        });
        let mut settings = ParseSettings { threads, frames: frames_in_parse, ..Default::default() };
        options.apply(&mut settings);
        let Ok(parser) = Parser::new(&settings, sink.clone()) else {
            eprintln!("rav1g-parse-check: rav1d was not created");
            return ExitCode::FAILURE;
        };
        let start = Instant::now();
        for unit in &units {
            let _ = parser.send(unit);
        }
        // The frames that are still in parse.
        parser.wait();
        parse = parse.min(start.elapsed());
        frames = sink.frames.load(Ordering::Relaxed);
        width = sink.width.load(Ordering::Relaxed);
        height = sink.height.load(Ordering::Relaxed);
    }

    // The parse with the sink.
    let mut with_sink = Duration::MAX;
    let mut block_lists = Duration::MAX;
    let mut sink_calls = Duration::ZERO;
    for _ in 0..RUNS {
        let mut parser = match RustParser::with_clock(threads, frames_in_parse, options, true, wave_edges) {
            Ok(parser) => parser,
            Err(error) => {
                eprintln!("rav1g-parse-check: {error}");
                return ExitCode::FAILURE;
            }
        };
        parser.recycle();
        let mut total = Duration::ZERO;
        for unit in &units {
            let start = Instant::now();
            let _ = parser.send(unit);
            total += start.elapsed();
        }
        let start = Instant::now();
        parser.wait();
        total += start.elapsed();
        let start = Instant::now();
        parser.wait();
        total += start.elapsed();
        // The arena of a frame is made ready inside `send`: that time is the host's, not the sink's.
        let total = total.saturating_sub(parser.arena_time());
        if total < with_sink {
            with_sink = total;
            block_lists = Duration::from_nanos(parser.block_list_nanos());
            sink_calls = Duration::from_nanos(parser.sink_call_nanos());
        }
        if parser.recycled() as u64 != frames {
            eprintln!("rav1g-parse-check: {} frames with the sink, {frames} without", parser.recycled());
            return ExitCode::FAILURE;
        }
    }

    let sink = with_sink.saturating_sub(parse);
    println!(
        "PARSE TIME {}: {frames} frames {width}x{height}, {threads} thread(s), {frames_in_parse} frame(s) in parse, switches {}, best of {RUNS}: parse {:.3} ms/frame, with \
         the sink {:.3} ms/frame, the sink {:.3} ms/frame, of which the block lists {:.3} ms/frame{} (CPU time of all \
         threads for the block lists)",
        path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        options.words(),
        milliseconds(parse, frames),
        milliseconds(with_sink, frames),
        milliseconds(sink, frames),
        milliseconds(block_lists, frames),
        if options.time_calls {
            format!(" and the other calls of the sink {:.3} ms/frame", milliseconds(sink_calls, frames))
        } else {
            String::new()
        },
    );
    ExitCode::SUCCESS
}

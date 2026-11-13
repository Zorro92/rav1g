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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rav1g_parse::rav1d::parse::{
    ParseBlock, ParseFrame, ParsePalette, ParseSettings, ParseSink, ParseSuperblock, ParseTile, ParseTxb, Parser,
};

use crate::container;
use crate::rhost::RustParser;

/// The largest number of coefficients a transform block has (32x32 coded).
const MAX_COEFS: usize = 1024;

/// A sink that keeps nothing.
struct NullSink {
    /// A block of coefficients per tile, which the parser writes and nothing reads.
    scratch: UnsafeCell<Vec<Box<[i32; MAX_COEFS]>>>,
    frames: AtomicU64,
    width: AtomicU64,
    height: AtomicU64,
}

// SAFETY: the vector is written by `frame_begin` while no tile is in parse; a tile's thread writes the
// tile's own block.
unsafe impl Send for NullSink {}
// SAFETY: as above.
unsafe impl Sync for NullSink {}

impl ParseSink for NullSink {
    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<usize, ()> {
        let tiles = usize::from(frame.frame_hdr.tiling.cols) * usize::from(frame.frame_hdr.tiling.rows);
        // SAFETY: no tile is in parse.
        unsafe {
            let scratch = &mut *self.scratch.get();
            while scratch.len() < tiles {
                scratch.push(Box::new([0; MAX_COEFS]));
            }
        }
        self.width.store(frame.bw as u64 * 4, Ordering::Relaxed);
        self.height.store(frame.bh as u64 * 4, Ordering::Relaxed);
        Ok(0)
    }

    fn tile_begin(&self, _frame: usize, _tile: &ParseTile) {}

    fn block(&self, _frame: usize, _tile: usize, _block: &ParseBlock<'_>) {}

    fn txb(&self, _frame: usize, tile: usize, _txb: &ParseTxb) -> *mut i32 {
        // SAFETY: the tile's own block, on the tile's thread.
        unsafe { (&*self.scratch.get())[tile].as_ptr().cast_mut() }
    }

    fn palette(&self, _frame: usize, _tile: usize, _palette: &ParsePalette<'_>) {}

    fn superblock(&self, _frame: usize, _tile: usize, _superblock: &ParseSuperblock<'_>) {}

    fn tile_end(&self, _frame: usize, _tile: usize) {}

    fn frame_end(&self, _frame: usize, ok: bool) {
        self.frames.fetch_add(u64::from(ok), Ordering::Relaxed);
    }
}

fn milliseconds(duration: Duration, frames: u64) -> f64 {
    duration.as_secs_f64() * 1000.0 / frames.max(1) as f64
}

pub fn run(path: &Path, threads: u16) -> ExitCode {
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
            scratch: UnsafeCell::new(Vec::new()),
            frames: AtomicU64::new(0),
            width: AtomicU64::new(0),
            height: AtomicU64::new(0),
        });
        let settings = ParseSettings { threads, frames: 1, ..Default::default() };
        let Ok(parser) = Parser::new(&settings, sink.clone()) else {
            eprintln!("rav1g-parse-check: rav1d was not created");
            return ExitCode::FAILURE;
        };
        let start = Instant::now();
        for unit in &units {
            let _ = parser.send(unit);
        }
        parse = parse.min(start.elapsed());
        frames = sink.frames.load(Ordering::Relaxed);
        width = sink.width.load(Ordering::Relaxed);
        height = sink.height.load(Ordering::Relaxed);
    }

    // The parse with the sink.
    let mut with_sink = Duration::MAX;
    let mut block_lists = Duration::MAX;
    for _ in 0..RUNS {
        let mut parser = match RustParser::new(threads) {
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
        // The arena of a frame is made ready inside `send`: that time is the host's, not the sink's.
        let total = total.saturating_sub(parser.arena_time());
        if total < with_sink {
            with_sink = total;
            block_lists = Duration::from_nanos(parser.block_list_nanos());
        }
        if parser.recycled() as u64 != frames {
            eprintln!("rav1g-parse-check: {} frames with the sink, {frames} without", parser.recycled());
            return ExitCode::FAILURE;
        }
    }

    let sink = with_sink.saturating_sub(parse);
    println!(
        "PARSE TIME {}: {frames} frames {width}x{height}, {threads} thread(s), best of {RUNS}: parse {:.3} ms/frame, with \
         the sink {:.3} ms/frame, the sink {:.3} ms/frame, of which the block lists {:.3} ms/frame (CPU time of all \
         threads for the block lists)",
        path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        milliseconds(parse, frames),
        milliseconds(with_sink, frames),
        milliseconds(sink, frames),
        milliseconds(block_lists, frames),
    );
    ExitCode::SUCCESS
}

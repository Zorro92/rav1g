// SPDX-License-Identifier: BSD-2-Clause

//! The intra step timing of the host (`crates/rav1g/src/host/step_time.rs`): counts of the transform blocks
//! by step for GPU timestamps. This program has no GPU: its tiles never count (`av1_tile_data::perf_steps`
//! stays null), so `blocks.rs` never calls these. What it does have is [`place`], which `-steps` records
//! the transform blocks of the wavefront with.

use std::ffi::{c_int, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Never called here: the tiles of this program have no counts.
pub(crate) fn kind(_mode: u32, _delta: i8, _filter: bool, _intrabc: bool, _inter_intra: bool) -> usize {
    unreachable!("this program has no intra step timing");
}

/// Never called here.
///
/// # Safety
/// None: the function does not return.
pub(crate) unsafe fn count(_counts: *mut c_void, _iter: c_int, _plane: usize, _kind: usize, _filter_diagonals: u32) {
    unreachable!("this program has no intra step timing");
}

/// A transform block of the wavefront as [`place`] is given it.
#[derive(Clone, Copy, Debug)]
pub struct Placed {
    pub chroma: bool,
    pub col: c_int,
    pub row: c_int,
    pub txw: c_int,
    pub txh: c_int,
    /// The corner (0 or 1), the units of the row above and of the column to the left it reads.
    pub reads: [c_int; 3],
    pub cfl: bool,
    pub filter: bool,
    pub intrabc: bool,
    pub iter: c_int,
}

/// `-steps`: [`place`] records the blocks (one frame in parse, so the blocks of one frame at a time).
pub static RECORD: AtomicBool = AtomicBool::new(false);
pub static PLACED: Mutex<Vec<Placed>> = Mutex::new(Vec::new());

/// The host's hook where a transform block of the wavefront gets its iteration (`intra_plane_edges`).
#[expect(clippy::too_many_arguments, reason = "the signature of the host's function")]
pub(crate) fn place(
    chroma: bool,
    col: c_int,
    row: c_int,
    txw: c_int,
    txh: c_int,
    reads: [c_int; 3],
    cfl: bool,
    filter: bool,
    intrabc: bool,
    iter: c_int,
) {
    if RECORD.load(Ordering::Relaxed) {
        PLACED.lock().expect("no thread panics with it").push(Placed {
            chroma,
            col,
            row,
            txw,
            txh,
            reads,
            cfl,
            filter,
            intrabc,
            iter,
        });
    }
}

/// The blocks [`place`] records next are those of another frame (`-steps`).
pub fn frame_start() {
    if RECORD.load(Ordering::Relaxed) {
        PLACED.lock().expect("no thread panics with it").push(Placed {
            chroma: false,
            col: 0,
            row: 0,
            txw: 0,
            txh: 0,
            reads: [0; 3],
            cfl: false,
            filter: false,
            intrabc: false,
            iter: c_int::MIN,
        });
    }
}

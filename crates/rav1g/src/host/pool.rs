// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The pools of what a frame's parse writes for the stages (docs/phase2-rav1d.md, step 5): the mode info
//! records, the coefficients and the transform block records of all frames in flight come from one
//! buffer each, in pieces, as the parse proceeds. A frame holds what it uses, not a share that is sized
//! for the worst frame. (The entropy decoder of libaom, while it was a parser of the decoder, had a
//! share of each buffer for every slot; it left in step 6.)
//!
//! **Chunks and pieces.** A pool is divided into chunks: what a 64x64 area needs at worst, so a tile of
//! `c` such areas ("cells") never needs more than `c` chunks:
//!
//! | Pool | A chunk | Why |
//! | --- | --- | --- |
//! | mode info | 256 records | a block is 4x4 or larger |
//! | coefficients | 7680 values | a 4x4 transform block with 16 coefficients takes 20 (its count is rounded up to the next multiple of four above it), four of luma and two of chroma an 8x8 area |
//! | transform block records | 384 records | 256 of luma, 64 of each chroma plane |
//!
//! A tile takes a piece when it needs room: up to [`PIECE_CHUNKS`] chunks that follow each other, fewer
//! when no such run is free, and never more than its cells that are not parsed yet. When the tile ends,
//! the chunks of its last piece that it did not write go back at once. The frame's pieces go back when
//! its GPU work is done, or when the frame fails.
//!
//! **Waiting.** A pool holds the worst case of one frame of the largest size, so a frame always fits
//! once the frames in front of it are through. Until then a tile that finds no chunk free waits for the
//! GPU to finish an older frame. With more than one frame in parse there is a second reason to wait:
//! the frames behind the oldest frame in parse cannot give anything back before that frame is
//! submitted, so they must leave it what it can still need. What a frame in parse can still need is
//! known as it goes: `held + cells that are not parsed`, at most what it was when the frame began (all
//! its cells) and never rising. The rule ([`Pools::take`]): a frame takes chunks only while, for every
//! frame in parse in front of it, that frame's bound and what the frames behind that frame hold fit
//! the pool. The oldest frame in parse takes whatever is free. So frames that are parsed side by side
//! share the room the worst case leaves, which is nearly all of it: a second frame in parse costs no
//! memory, and waits for the first one only where the first one really is a worst case.
//!
//! A frame that cannot fit (a pool made smaller with `AV1_LOW_MEMORY_*`) fails when it is the oldest,
//! nothing in front of it holds a chunk and none is free: a decode error of that frame, as a frame that
//! outgrew its two shares was.
//!
//! **The test switch** (`AV1_PARSER_OPTION_TEST_POOL_WAIT`, `-parseopt poolwait`). A pool that holds
//! the worst case is exhausted only by streams made for it, so the switch makes every stream a test
//! of the waits: a frame's first piece of a pool is given as always, and every piece after it counts
//! the pool as empty for as long as a frame in front holds anything of it. So a tile waits in the
//! middle of its parse, with pieces in hand, for the GPU to finish every older frame, and with more
//! than one frame in parse for the frames in parse in front as well. Nothing fails for it.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// The pools.
pub(crate) const KINDS: usize = 3;
/// What a pool holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    ModeInfo = 0,
    Coefs = 1,
    Txb = 2,
}

/// The mode info records of a chunk: a 64x64 area of 4x4 blocks.
pub(crate) const MODE_INFO_CHUNK: usize = 256;
/// The coefficients of a chunk: 30 for each 4x4 unit of a 64x64 area.
pub(crate) const COEF_CHUNK: usize = 256 * 30;
/// The transform block records of a chunk: one for each 4x4 block of luma and of both chroma planes.
pub(crate) const TXB_CHUNK: usize = 384;
/// The items of a chunk, by [`Kind`].
pub(crate) const CHUNK_ITEMS: [usize; KINDS] = [MODE_INFO_CHUNK, COEF_CHUNK, TXB_CHUNK];
/// The chunks of a piece at most, by [`Kind`]: 0.9 MiB of mode info, 0.5 MiB of coefficients, 0.4 MiB of
/// transform block records. A stage that goes through a tile's records has a dispatch for each piece
/// that does not follow the one before it. A small pool has smaller pieces ([`Pools::piece`]).
pub(crate) const PIECE_CHUNKS: [usize; KINDS] = [16, 16, 64];
/// The pipeline slots a decoder has at most (`FrameThreadDataCount`): the frames that hold pieces.
const SLOTS: usize = 4;

/// What [`Pools::take`] gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Take {
    /// Chunks `first..first + chunks`.
    Piece { first: usize, chunks: usize },
    /// The frame does not fit the pool.
    Full,
    /// The pools are closed: the decoder is being released, or it stopped with a panic.
    Stopped,
}

/// A piece a frame holds.
#[derive(Clone, Copy)]
struct Piece {
    kind: Kind,
    first: u32,
    chunks: u32,
}

/// The chunks of one pool.
struct Chunks {
    /// The chunks a frame in parse behind another one is held against (the rule of [`Pools::take`]).
    rule: usize,
    /// One bit a chunk, set while it is free.
    free: Vec<u64>,
    total: usize,
    free_count: usize,
    /// The most chunks that were in use at once.
    peak: usize,
}

impl Chunks {
    fn new(total: usize, rule: usize) -> Chunks {
        let mut free = vec![0u64; total.div_ceil(64)];
        for chunk in 0..total {
            free[chunk / 64] |= 1 << (chunk % 64);
        }
        Chunks { rule: rule.min(total), free, total, free_count: total, peak: 0 }
    }

    fn is_free(&self, chunk: usize) -> bool {
        self.free[chunk / 64] & (1 << (chunk % 64)) != 0
    }

    /// Up to `want` free chunks that follow each other: the first run of `want`, or else the longest
    /// run there is (the first of that length).
    fn take(&mut self, want: usize) -> Option<(usize, usize)> {
        if self.free_count == 0 || want == 0 {
            return None;
        }
        let mut best: Option<(usize, usize)> = None;
        let mut chunk = 0;
        while chunk < self.total {
            let word = self.free[chunk / 64] >> (chunk % 64);
            if word == 0 {
                // Nothing free in the rest of this word.
                chunk = (chunk / 64 + 1) * 64;
                continue;
            }
            chunk += word.trailing_zeros() as usize;
            let first = chunk;
            while chunk < self.total && chunk - first < want && self.is_free(chunk) {
                chunk += 1;
            }
            let run = chunk - first;
            if best.is_none_or(|(_, len)| run > len) {
                best = Some((first, run));
                if run == want {
                    break;
                }
            }
        }
        let (first, run) = best?;
        for taken in first..first + run {
            self.free[taken / 64] &= !(1 << (taken % 64));
        }
        self.free_count -= run;
        self.peak = self.peak.max(self.total - self.free_count);
        Some((first, run))
    }

    fn give(&mut self, first: usize, chunks: usize) {
        for chunk in first..first + chunks {
            debug_assert!(!self.is_free(chunk), "a chunk is given back once");
            self.free[chunk / 64] |= 1 << (chunk % 64);
        }
        self.free_count += chunks;
    }
}

/// The frame of a pipeline slot, as the pools see it.
struct Frame {
    /// Its place in decode order among the frames that hold pieces; 0: the slot has no frame.
    order: u64,
    /// Its parse has not ended.
    in_parse: bool,
    /// The chunks it holds.
    held: [usize; KINDS],
    /// The most chunks it can hold when its parse has ended: what `held + cells not parsed` was at
    /// its lowest, the times it was looked at.
    bound: [usize; KINDS],
    pieces: Vec<Piece>,
}

struct State {
    chunks: [Chunks; KINDS],
    frames: [Frame; SLOTS],
    /// The frames that have begun.
    begun: u64,
    stop: bool,
    /// The test switch: see the module.
    test_wait: bool,
    /// The takes that waited: for a chunk to come back from the GPU, and for a frame in front.
    waits: [u64; 2],
}

/// What the pools did, for the "MEMORY pools" line of a decoder's release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub total: [usize; KINDS],
    /// The most chunks in use at once.
    pub peak: [usize; KINDS],
    /// The takes that waited for the GPU to finish an older frame.
    pub waits_for_gpu: u64,
    /// The takes that waited for a frame in parse in front of theirs.
    pub waits_for_frame: u64,
}

/// The pools of a decoder.
pub(crate) struct Pools {
    state: Mutex<State>,
    changed: Condvar,
    /// The threads that wait in [`Pools::take`]: the progress of a frame wakes them only when there are
    /// any.
    waiting: AtomicUsize,
    /// The cells of a slot's frame that are not parsed yet.
    remaining: [AtomicI32; SLOTS],
    /// The chunks of each pool.
    total: [usize; KINDS],
}

impl Pools {
    /// Pools of `total` chunks each. `rule`: the chunks of each that frames in parse share under the
    /// rule of [`Pools::take`] (at most the total; what is above it is for the oldest frame in parse
    /// alone).
    #[cfg(test)]
    pub(crate) fn new(total: [usize; KINDS], rule: [usize; KINDS]) -> Pools {
        Self::with_test_wait(total, rule, false)
    }

    /// [`Pools::new`], with the test switch (see the module).
    pub(crate) fn with_test_wait(total: [usize; KINDS], rule: [usize; KINDS], test_wait: bool) -> Pools {
        let frame =
            || Frame { order: 0, in_parse: false, held: [0; KINDS], bound: [0; KINDS], pieces: Vec::with_capacity(64) };
        Pools {
            state: Mutex::new(State {
                chunks: [0, 1, 2].map(|kind| Chunks::new(total[kind], rule[kind])),
                frames: [frame(), frame(), frame(), frame()],
                begun: 0,
                stop: false,
                test_wait,
                waits: [0; 2],
            }),
            changed: Condvar::new(),
            waiting: AtomicUsize::new(0),
            remaining: [const { AtomicI32::new(0) }; SLOTS],
            total,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The chunks a tile asks for when it needs room in a pool: a piece, an eighth of a pool that is
    /// smaller than eight pieces (the frames of a small decoder are small), and never more than the
    /// cells of the tile that are not parsed yet (a chunk is the worst case of a cell).
    pub(crate) fn piece(&self, kind: Kind, cells_left: usize) -> usize {
        PIECE_CHUNKS[kind as usize].min(self.total[kind as usize] / 8).min(cells_left).max(1)
    }

    /// The parse of a frame of `cells` cells begins on the slot: the frame is the youngest of those that
    /// hold pieces. Frames begin in decode order.
    pub(crate) fn begin(&self, slot: usize, cells: usize) {
        let mut state = self.state();
        state.begun += 1;
        let order = state.begun;
        let frame = &mut state.frames[slot];
        debug_assert!(frame.pieces.is_empty(), "the slot's last frame gave its pieces back");
        frame.order = order;
        frame.in_parse = true;
        frame.held = [0; KINDS];
        frame.bound = [cells; KINDS];
        self.remaining[slot].store(cells as i32, Ordering::Release);
    }

    /// `cells` more cells of the slot's frame are parsed. A tile's thread.
    pub(crate) fn progress(&self, slot: usize, cells: usize) {
        self.remaining[slot].fetch_sub(cells as i32, Ordering::AcqRel);
        if self.waiting.load(Ordering::Acquire) != 0 {
            // Under the lock, so that a thread that is about to wait does not miss it.
            let _state = self.state();
            self.changed.notify_all();
        }
    }

    /// The parse of the slot's frame has ended: it takes no more, and the frames behind it are held
    /// against the next frame in parse, or against none.
    pub(crate) fn parsed(&self, slot: usize) {
        let mut state = self.state();
        state.frames[slot].in_parse = false;
        self.remaining[slot].store(0, Ordering::Release);
        drop(state);
        self.changed.notify_all();
    }

    /// Up to `want` chunks that follow each other for the frame of the slot; waits until the frame may
    /// take a chunk and one is free (see the module).
    pub(crate) fn take(&self, slot: usize, kind: Kind, want: usize) -> Take {
        let k = kind as usize;
        let want = want.max(1);
        let mut state = self.state();
        let mut waited = [false; 2];
        let taken = loop {
            if state.stop {
                break Take::Stopped;
            }
            let order = state.frames[slot].order;
            // What the frames in parse in front of this one leave it.
            let mut allowed = want;
            for other in 0..SLOTS {
                let front = &state.frames[other];
                if !front.in_parse || front.order == 0 || front.order >= order {
                    continue;
                }
                let remaining = usize::try_from(self.remaining[other].load(Ordering::Acquire)).unwrap_or(0);
                let bound = front.bound[k].min(front.held[k] + remaining);
                let front_order = front.order;
                state.frames[other].bound[k] = bound;
                let behind: usize =
                    state.frames.iter().filter(|frame| frame.order > front_order).map(|frame| frame.held[k]).sum();
                allowed = allowed.min(state.chunks[k].rule.saturating_sub(bound + behind));
            }
            let in_front =
                state.frames.iter().any(|frame| frame.order != 0 && frame.order < order && frame.held[k] != 0);
            // The test switch: behind its first piece the frame finds the pool empty while a frame in
            // front holds something.
            let empty = state.test_wait && in_front && state.frames[slot].held[k] != 0;
            if allowed != 0 {
                let taken = if empty { None } else { state.chunks[k].take(allowed) };
                if let Some((first, chunks)) = taken {
                    let remaining = usize::try_from(self.remaining[slot].load(Ordering::Acquire)).unwrap_or(0);
                    let frame = &mut state.frames[slot];
                    frame.bound[k] = frame.bound[k].min(frame.held[k] + remaining);
                    frame.held[k] += chunks;
                    frame.pieces.push(Piece { kind, first: first as u32, chunks: chunks as u32 });
                    break Take::Piece { first, chunks };
                }
                // No chunk is free. The frames in front give theirs back by themselves; when none of
                // them holds one, nothing ever comes back to this frame.
                if !in_front {
                    break Take::Full;
                }
                waited[0] = true;
            } else {
                waited[1] = true;
            }
            self.waiting.fetch_add(1, Ordering::AcqRel);
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
            self.waiting.fetch_sub(1, Ordering::AcqRel);
        };
        state.waits[0] += u64::from(waited[0]);
        state.waits[1] += u64::from(waited[1]);
        taken
    }

    /// The piece of the slot's frame that starts at `first` keeps its first `keep` chunks; the rest is
    /// free again.
    pub(crate) fn shrink(&self, slot: usize, kind: Kind, first: usize, keep: usize) {
        let mut state = self.state();
        let state = &mut *state;
        let frame = &mut state.frames[slot];
        let Some(at) = frame.pieces.iter().rposition(|piece| piece.kind == kind && piece.first as usize == first)
        else {
            return;
        };
        let chunks = frame.pieces[at].chunks as usize;
        if keep >= chunks {
            return;
        }
        if keep == 0 {
            frame.pieces.swap_remove(at);
        } else {
            frame.pieces[at].chunks = keep as u32;
        }
        frame.held[kind as usize] -= chunks - keep;
        state.chunks[kind as usize].give(first + keep, chunks - keep);
        self.changed.notify_all();
    }

    /// The frame of the slot is through, on the GPU or because it failed: its pieces are free again.
    /// `each` gets every piece first (kind, first chunk, chunks), under the lock of the pools.
    pub(crate) fn release(&self, slot: usize, mut each: impl FnMut(Kind, usize, usize)) {
        let mut state = self.state();
        let state = &mut *state;
        let frame = &mut state.frames[slot];
        for piece in frame.pieces.drain(..) {
            each(piece.kind, piece.first as usize, piece.chunks as usize);
            state.chunks[piece.kind as usize].give(piece.first as usize, piece.chunks as usize);
        }
        frame.order = 0;
        frame.in_parse = false;
        frame.held = [0; KINDS];
        self.remaining[slot].store(0, Ordering::Release);
        self.changed.notify_all();
    }

    /// Closes the pools: nothing waits any more, and a take gives [`Take::Stopped`].
    pub(crate) fn stop(&self) {
        self.state().stop = true;
        self.changed.notify_all();
    }

    pub(crate) fn stats(&self) -> Stats {
        let state = self.state();
        Stats {
            total: [0, 1, 2].map(|kind| state.chunks[kind].total),
            peak: [0, 1, 2].map(|kind| state.chunks[kind].peak),
            waits_for_gpu: state.waits[0],
            waits_for_frame: state.waits[1],
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn piece(take: Take) -> (usize, usize) {
        match take {
            Take::Piece { first, chunks } => (first, chunks),
            other => panic!("no piece: {other:?}"),
        }
    }

    /// A take on another thread, and whether it has come back.
    fn take_on_thread(pools: &Arc<Pools>, slot: usize, want: usize) -> (thread::JoinHandle<Take>, Arc<AtomicBool>) {
        let done = Arc::new(AtomicBool::new(false));
        let (pools, flag) = (pools.clone(), done.clone());
        let handle = thread::spawn(move || {
            let taken = pools.take(slot, Kind::ModeInfo, want);
            flag.store(true, Ordering::Release);
            taken
        });
        // Long enough for the thread to be in its wait.
        thread::sleep(Duration::from_millis(100));
        (handle, done)
    }

    #[test]
    fn pieces_are_runs_of_chunks() {
        let pools = Pools::new([100, 10, 10], [100, 10, 10]);
        pools.begin(0, 100);
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 16)), (0, 16));
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 16)), (16, 16));
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 16)), (32, 16));
        // The pools are apart.
        assert_eq!(piece(pools.take(0, Kind::Coefs, 4)), (0, 4));
        // The end of a piece goes back and is where the next piece starts.
        pools.shrink(0, Kind::ModeInfo, 16, 3);
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 5)), (19, 5));
        // The first run that is long enough: 8 chunks are free in front of the piece at 32.
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 20)), (48, 20));
        // A piece that goes back whole. Then no run has 40 chunks: the longest one, and then the other.
        pools.shrink(0, Kind::ModeInfo, 19, 0);
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 40)), (68, 32));
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 40)), (19, 13));
        // Nothing is free, and no frame in front holds anything: the frame does not fit.
        assert_eq!(pools.take(0, Kind::ModeInfo, 1), Take::Full);
        let stats = pools.stats();
        assert_eq!((stats.total, stats.peak), ([100, 10, 10], [100, 4, 0]));
        assert_eq!((stats.waits_for_gpu, stats.waits_for_frame), (0, 0));
        // The frame is through: everything is free, and the pieces were named.
        let mut chunks = [0; KINDS];
        pools.release(0, |kind, _, count| chunks[kind as usize] += count);
        assert_eq!(chunks, [100, 4, 0]);
        pools.begin(0, 100);
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 100)), (0, 100));
    }

    #[test]
    fn a_take_waits_for_a_frame_on_the_gpu() {
        let pools = Arc::new(Pools::new([10, 10, 10], [10, 10, 10]));
        pools.begin(0, 10);
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 16)), (0, 10));
        pools.parsed(0);
        // The next frame: nothing is free until the first one is through.
        pools.begin(1, 10);
        let (handle, done) = take_on_thread(&pools, 1, 4);
        assert!(!done.load(Ordering::Acquire));
        pools.release(0, |_, _, _| {});
        assert_eq!(piece(handle.join().unwrap()), (0, 4));
        assert_eq!(pools.stats().waits_for_gpu, 1);
    }

    #[test]
    fn a_frame_behind_leaves_the_frame_in_front_what_it_can_need() {
        let pools = Arc::new(Pools::new([20, 20, 20], [20, 20, 20]));
        pools.begin(0, 20);
        pools.begin(1, 20);
        // The frame in front has parsed nothing: it can need all of the pool.
        let (handle, done) = take_on_thread(&pools, 1, 16);
        assert!(!done.load(Ordering::Acquire));
        // Five of its cells are parsed, and it holds one chunk: four chunks are what it cannot need.
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 1)), (0, 1));
        pools.progress(0, 5);
        assert_eq!(piece(handle.join().unwrap()), (1, 4));
        // No more for the frame behind, however much is free.
        let (handle, done) = take_on_thread(&pools, 1, 1);
        assert!(!done.load(Ordering::Acquire));
        // The frame in front takes what it asks for.
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 15)), (5, 15));
        assert!(!done.load(Ordering::Acquire));
        // Its end: the chunks it did not use go back, and the frame behind is the oldest in parse.
        pools.shrink(0, Kind::ModeInfo, 5, 2);
        pools.parsed(0);
        assert_eq!(piece(handle.join().unwrap()), (7, 1));
        assert_eq!(piece(pools.take(1, Kind::ModeInfo, 16)), (8, 12));
        let stats = pools.stats();
        assert_eq!((stats.waits_for_gpu, stats.waits_for_frame), (0, 2));
        assert_eq!(stats.peak[0], 20);
    }

    #[test]
    fn the_chunks_above_the_rule_are_the_oldest_frames() {
        // Two chunks more than the frames in parse share.
        let pools = Arc::new(Pools::new([12, 12, 12], [10, 10, 10]));
        pools.begin(0, 10);
        pools.begin(1, 10);
        pools.progress(0, 10);
        // The frame in front holds nothing and has parsed all its cells but is not at its end: the
        // frame behind gets the ten chunks of the rule, not the two above it.
        assert_eq!(piece(pools.take(1, Kind::ModeInfo, 16)), (0, 10));
        let (handle, done) = take_on_thread(&pools, 1, 1);
        assert!(!done.load(Ordering::Acquire));
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 16)), (10, 2));
        pools.stop();
        assert_eq!(handle.join().unwrap(), Take::Stopped);
        assert_eq!(pools.take(0, Kind::ModeInfo, 1), Take::Stopped);
    }

    #[test]
    fn the_test_switch_makes_a_frame_wait_behind_its_first_piece() {
        let pools = Arc::new(Pools::with_test_wait([100, 100, 100], [100, 100, 100], true));
        pools.begin(0, 100);
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 4)), (0, 4));
        // Nothing in front: the oldest frame takes as always.
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 4)), (4, 4));
        pools.parsed(0);
        pools.begin(1, 100);
        assert_eq!(piece(pools.take(1, Kind::ModeInfo, 4)), (8, 4));
        // Another pool: its first piece there.
        assert_eq!(piece(pools.take(1, Kind::Txb, 4)), (0, 4));
        let (handle, done) = take_on_thread(&pools, 1, 4);
        assert!(!done.load(Ordering::Acquire));
        pools.release(0, |_, _, _| {});
        assert_eq!(piece(handle.join().unwrap()), (0, 4));
        assert_eq!(pools.stats().waits_for_gpu, 1);
    }

    #[test]
    fn the_piece_a_tile_asks_for() {
        let pools = Pools::new([2040, 36, 4], [2040, 36, 4]);
        assert_eq!(pools.piece(Kind::ModeInfo, 500), 16);
        assert_eq!(pools.piece(Kind::ModeInfo, 3), 3);
        assert_eq!(pools.piece(Kind::ModeInfo, 0), 1);
        assert_eq!(pools.piece(Kind::Coefs, 30), 4);
        assert_eq!(pools.piece(Kind::Txb, 4), 1);
    }
}

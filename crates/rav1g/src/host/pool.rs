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
//! | coefficients | 7680 values | at most 6144 per 64x64 of 4:2:0, with room for the unused end of a piece |
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
//! its cells) and never rising. The rule ([`Pools::take_growing`]): a frame takes chunks only while, for every
//! frame in parse in front of it, that frame's bound and what the frames behind that frame hold fit
//! the pool. The oldest frame in parse takes whatever is free. So frames that are parsed side by side
//! share the room the worst case leaves, which is nearly all of it: a second frame in parse costs no
//! memory, and waits for the first one only where the first one really is a worst case.
//!
//! When the oldest frame needs more and nothing in front can return a chunk, the pool can grow. The
//! indices of existing pieces stay fixed. New pieces use the new backing buffer, and the recording
//! sees a single buffer after the old pieces of the frame are copied across (pool_buffers.rs).
//!
//! **A pool below the worst case of a frame** (the same `AV1_LOW_MEMORY_*`: half the mode info pool,
//! four fifths of the coefficient pool; the rule is the same for every pool, by its own chunks).
//! Held against `held + cells that are not parsed`, a frame in front could need more than the pool
//! until half of it is parsed, and the frame behind it would wait that long for its first chunk,
//! though the pool is nearly empty (measured: 1.4 to 1.8 ms a 4K frame). Such a pool holds a frame
//! only if the frame needs less than a chunk a cell, so that is what the frame in front is left: for
//! its cells that are not parsed, their share of the pool (`cells not parsed * pool / cells of the
//! frame`, [`Frame::can_need`]). At its begin that is the whole pool, as `cells` is in a pool of the
//! worst case, and it falls as the frame is parsed. What this gives up: a frame that has more than
//! that share in its last part, behind a part with less, can find the room taken by the frame behind
//! it and need growth, where alone it would have fit. A frame whose cells are no more than the pool is held
//! against its cells, as ever. The waits are counted by pool (`-memstats`, [`Pools::take_stat_waits`]).
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

impl Kind {
    pub(crate) const ALL: [Kind; KINDS] = [Kind::ModeInfo, Kind::Coefs, Kind::Txb];
}

/// The mode info records of a chunk: a 64x64 area of 4x4 blocks.
pub(crate) const MODE_INFO_CHUNK: usize = crate::sys::MI_POOL_CHUNK_RECORDS;
/// AV1 5.11 limits a transform to 32x32 coded coefficients. The sink rounds the last
/// coefficient's index plus one up to four, within the transform's area. A 64x64 4:2:0
/// area therefore writes at most 6144 values. An append that needs another piece leaves
/// fewer than 1024 values unused. Even at one chunk per piece, a tile that already
/// holds as many chunks as it has cells always has room for its next transform.
pub(crate) const COEF_CHUNK: usize = 256 * 30;
const _: () = assert!(COEF_CHUNK - 32 * 32 >= 64 * 64 + 2 * 32 * 32);
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

/// What [`Pools::take_growing`] gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Take {
    /// Chunks `first..first + chunks`.
    Piece { first: usize, chunks: usize, grown: bool },
    /// The frame does not fit the pool.
    Full,
    /// The replacement buffer did not fit the memory budget or could not be allocated.
    Refused,
    /// The pools are closed: the decoder is being released, or it stopped with a panic; or a flush drops
    /// the frames in parse ([`Pools::set_flushing`]).
    Stopped,
}

/// A piece a frame holds.
#[derive(Clone, Copy)]
struct Piece {
    kind: Kind,
    grown: bool,
    first: u32,
    chunks: u32,
}

/// The chunks of one pool.
struct Chunks {
    /// The chunks a frame in parse behind another one is held against (the rule of [`Pools::take_growing`]).
    rule: usize,
    /// One bit a chunk, set while it is free.
    free: Vec<u64>,
    total: usize,
    free_count: usize,
    /// The most chunks that were in use at once.
    peak: usize,
    /// The same since the statistics last asked (`Pools::take_stat_peak`).
    stat_peak: usize,
}

impl Chunks {
    fn new(total: usize, rule: usize) -> Chunks {
        let mut free = vec![0u64; total.div_ceil(64)];
        for chunk in 0..total {
            free[chunk / 64] |= 1 << (chunk % 64);
        }
        Chunks { rule: rule.min(total), free, total, free_count: total, peak: 0, stat_peak: 0 }
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
        self.stat_peak = self.stat_peak.max(self.total - self.free_count);
        Some((first, run))
    }

    fn give(&mut self, first: usize, chunks: usize) {
        for chunk in first..first + chunks {
            debug_assert!(!self.is_free(chunk), "a chunk is given back once");
            self.free[chunk / 64] |= 1 << (chunk % 64);
        }
        self.free_count += chunks;
    }

    fn grow(&mut self, total: usize) {
        self.free.resize(total.div_ceil(64), 0);
        self.give(self.total, total - self.total);
        self.total = total;
        self.rule = total;
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
    /// its lowest, the times it was looked at (in a pool below the frame's worst case: the share of
    /// the pool of the cells not parsed, [`Frame::can_need`]).
    bound: [usize; KINDS],
    /// Its cells.
    cells: usize,
    pieces: Vec<Piece>,
    /// Buffers prepared for recording on this slot, before or after growth.
    old_bindings: u8,
    new_bindings: u8,
    /// A growth refused for this frame is reported once, even when many tiles need it.
    refused: u8,
}

impl Frame {
    /// The chunks the frame is left for `remaining` cells that are not parsed, in a pool of which the
    /// frames in parse share `rule` chunks: a chunk a cell, which is the worst case, or in a pool that
    /// is smaller than the frame's cells the share of the pool those cells are of the frame, rounded
    /// up (see the module).
    fn can_need(&self, remaining: usize, rule: usize) -> usize {
        if self.cells <= rule { remaining } else { (remaining * rule).div_ceil(self.cells) }
    }
}

struct State {
    chunks: [Chunks; KINDS],
    frames: [Frame; SLOTS],
    /// The frames that have begun.
    begun: u64,
    stop: bool,
    /// A flush drops the frames in parse (`rav1g_decoder_flush`): until it is through, a take gives
    /// [`Take::Stopped`] and nothing waits, so that a tile that waits for room a frame in front holds
    /// returns and its thread parks, as the flush needs.
    flushing: bool,
    /// The test switch: see the module.
    test_wait: bool,
    grown: u8,
    retiring: u8,
    /// The takes that waited, by [`Kind`]: for a chunk to come back from the GPU, and for a frame in
    /// front.
    waits: [[u64; 2]; KINDS],
    /// The same since the statistics last asked ([`Pools::take_stat_waits`]).
    stat_waits: [[u64; 2]; KINDS],
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
    /// The threads that wait in [`Pools::take_growing`]: the progress of a frame wakes them only when there are
    /// any.
    waiting: AtomicUsize,
    /// The cells of a slot's frame that are not parsed yet.
    remaining: [AtomicI32; SLOTS],
    /// The chunks of each pool.
    total: [usize; KINDS],
}

impl Pools {
    /// Pools of `total` chunks each. `rule`: the chunks of each that frames in parse share under the
    /// rule of [`Pools::take_growing`] (at most the total; what is above it is for the oldest frame in parse
    /// alone).
    #[cfg(test)]
    pub(crate) fn new(total: [usize; KINDS], rule: [usize; KINDS]) -> Pools {
        Self::with_test_wait(total, rule, false)
    }

    /// [`Pools::new`], with the test switch (see the module).
    pub(crate) fn with_test_wait(total: [usize; KINDS], rule: [usize; KINDS], test_wait: bool) -> Pools {
        let frame = || Frame {
            order: 0,
            in_parse: false,
            held: [0; KINDS],
            bound: [0; KINDS],
            cells: 0,
            pieces: Vec::with_capacity(64),
            old_bindings: 0,
            new_bindings: 0,
            refused: 0,
        };
        Pools {
            state: Mutex::new(State {
                chunks: [0, 1, 2].map(|kind| Chunks::new(total[kind], rule[kind])),
                frames: [frame(), frame(), frame(), frame()],
                begun: 0,
                stop: false,
                flushing: false,
                test_wait,
                grown: 0,
                retiring: 0,
                waits: [[0; 2]; KINDS],
                stat_waits: [[0; 2]; KINDS],
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
        let rules = [0, 1, 2].map(|kind| state.chunks[kind].rule);
        let frame = &mut state.frames[slot];
        debug_assert!(frame.pieces.is_empty(), "the slot's last frame gave its pieces back");
        frame.order = order;
        frame.in_parse = true;
        frame.held = [0; KINDS];
        frame.cells = cells;
        frame.refused = 0;
        frame.bound = rules.map(|rule| frame.can_need(cells, rule));
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
    pub(crate) fn finish_parse(&self, slot: usize, ok: bool, mut copy: impl FnMut(Kind, usize, usize)) -> u8 {
        let mut state = self.state();
        let grown = state.grown;
        let frame = &mut state.frames[slot];
        frame.in_parse = false;
        if ok {
            if grown != 0 {
                for piece in &frame.pieces {
                    if !piece.grown && grown & (1 << piece.kind as usize) != 0 {
                        copy(piece.kind, piece.first as usize, piece.chunks as usize);
                    }
                }
            }
            frame.new_bindings = grown;
        }
        self.remaining[slot].store(0, Ordering::Release);
        drop(state);
        self.changed.notify_all();
        grown
    }

    /// Up to `want` chunks that follow each other for the frame of the slot; waits until the frame may
    /// take a chunk and one is free (see the module).
    pub(crate) fn take_growing(
        &self,
        slot: usize,
        kind: Kind,
        want: usize,
        worst: usize,
        mut grow: impl FnMut(usize) -> bool,
    ) -> Take {
        let k = kind as usize;
        let bit = 1 << k;
        let want = want.max(1);
        let mut state = self.state();
        let mut waited = [false; 2];
        let taken = loop {
            if state.stop || state.flushing {
                break Take::Stopped;
            }
            if state.frames[slot].refused & bit != 0 {
                break Take::Refused;
            }
            let order = state.frames[slot].order;
            let grown = state.grown & bit != 0;
            // What the frames in parse in front of this one leave it.
            let mut allowed = want;
            for other in 0..SLOTS {
                let front = &state.frames[other];
                if !front.in_parse || front.order == 0 || front.order >= order {
                    continue;
                }
                let bound = if grown {
                    front.cells
                } else {
                    let remaining = usize::try_from(self.remaining[other].load(Ordering::Acquire)).unwrap_or(0);
                    front.bound[k].min(front.held[k] + front.can_need(remaining, state.chunks[k].rule))
                };
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
                    let rule = state.chunks[k].rule;
                    let frame = &mut state.frames[slot];
                    frame.bound[k] = frame.bound[k].min(frame.held[k] + frame.can_need(remaining, rule));
                    frame.held[k] += chunks;
                    frame.pieces.push(Piece { kind, first: first as u32, chunks: chunks as u32, grown });
                    break Take::Piece { first, chunks, grown };
                }
                // No chunk is free. The frames in front give theirs back by themselves; when none of
                // them holds one, nothing ever comes back to this frame.
                if !in_front {
                    // The smaller pool let younger frames take pieces against a proportional bound.
                    // Keep their offsets and add room for the oldest frame's full bound.
                    let behind = state.chunks[k].total - state.frames[slot].held[k];
                    let total = worst + behind;
                    if !grown && worst != 0 && total > state.chunks[k].total {
                        if grow(total) {
                            state.chunks[k].grow(total);
                            state.grown |= bit;
                            state.retiring |= bit;
                            for frame in &mut state.frames {
                                if !frame.in_parse && frame.order != 0 {
                                    frame.old_bindings |= bit;
                                }
                            }
                            self.changed.notify_all();
                            continue;
                        }
                        state.frames[slot].refused |= bit;
                        break Take::Refused;
                    }
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
        for why in 0..2 {
            state.waits[k][why] += u64::from(waited[why]);
            state.stat_waits[k][why] += u64::from(waited[why]);
        }
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
    pub(crate) fn release_with(
        &self,
        slot: usize,
        mut each: impl FnMut(Kind, usize, usize, bool),
        mut retire: impl FnMut(Kind),
    ) {
        let mut state = self.state();
        let state = &mut *state;
        let frame = &mut state.frames[slot];
        for piece in frame.pieces.drain(..) {
            let grown = piece.grown || frame.new_bindings & (1 << piece.kind as usize) != 0;
            each(piece.kind, piece.first as usize, piece.chunks as usize, grown);
            state.chunks[piece.kind as usize].give(piece.first as usize, piece.chunks as usize);
        }
        frame.order = 0;
        frame.in_parse = false;
        frame.held = [0; KINDS];
        frame.old_bindings = 0;
        frame.new_bindings = 0;
        if state.retiring != 0 {
            for kind in Kind::ALL {
                let bit = 1 << kind as usize;
                if state.retiring & bit != 0
                    && !state.frames.iter().any(|frame| {
                        frame.old_bindings & bit != 0 || frame.pieces.iter().any(|p| p.kind == kind && !p.grown)
                    })
                {
                    retire(kind);
                    state.retiring &= !bit;
                }
            }
        }
        self.remaining[slot].store(0, Ordering::Release);
        self.changed.notify_all();
    }

    /// The chunks of a pool that the frames behind the frame of the slot hold: what a frame that does
    /// not fit ([`Take::Full`]) did not have for itself.
    pub(crate) fn held_behind(&self, slot: usize, kind: Kind) -> usize {
        let state = self.state();
        let order = state.frames[slot].order;
        state.frames.iter().filter(|frame| frame.order > order).map(|frame| frame.held[kind as usize]).sum()
    }

    /// The chunks the frame of the slot holds, by [`Kind`].
    pub(crate) fn held(&self, slot: usize) -> [usize; KINDS] {
        self.state().frames[slot].held
    }

    /// The most chunks of each pool that were in use at once since the last call; the count starts
    /// over with what is in use now. For the statistics of `mem_stats.rs`.
    pub(crate) fn take_stat_peak(&self) -> [usize; KINDS] {
        let mut state = self.state();
        [0, 1, 2].map(|kind| {
            let chunks = &mut state.chunks[kind];
            std::mem::replace(&mut chunks.stat_peak, chunks.total - chunks.free_count)
        })
    }

    /// The takes of each pool that waited since the last call: for the GPU to finish an older frame,
    /// and for a frame in parse in front. For the statistics of `mem_stats.rs`.
    pub(crate) fn take_stat_waits(&self) -> [[u64; 2]; KINDS] {
        std::mem::take(&mut self.state().stat_waits)
    }

    /// Closes the pools: nothing waits any more, and a take gives [`Take::Stopped`].
    pub(crate) fn stop(&self) {
        self.state().stop = true;
        self.changed.notify_all();
    }

    /// While a flush drops the frames in parse: a take gives [`Take::Stopped`] (the tile's frame fails,
    /// and it is dropped anyway) and the takes that wait return. Off again when the parser's threads
    /// have parked and the frames have ended.
    pub(crate) fn set_flushing(&self, flushing: bool) {
        self.state().flushing = flushing;
        self.changed.notify_all();
    }

    pub(crate) fn stats(&self) -> Stats {
        let state = self.state();
        Stats {
            total: [0, 1, 2].map(|kind| state.chunks[kind].total),
            peak: [0, 1, 2].map(|kind| state.chunks[kind].peak),
            waits_for_gpu: state.waits.iter().map(|waits| waits[0]).sum(),
            waits_for_frame: state.waits.iter().map(|waits| waits[1]).sum(),
        }
    }
}

#[cfg(test)]
impl Pools {
    fn take(&self, slot: usize, kind: Kind, want: usize) -> Take {
        self.take_growing(slot, kind, want, 0, |_| unreachable!())
    }

    fn parsed(&self, slot: usize) {
        self.finish_parse(slot, true, |_, _, _| unreachable!());
    }

    fn release(&self, slot: usize, mut each: impl FnMut(Kind, usize, usize)) {
        self.release_with(slot, |kind, first, chunks, _| each(kind, first, chunks), |_| unreachable!());
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
            Take::Piece { first, chunks, .. } => (first, chunks),
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
    fn a_flush_ends_the_wait_of_a_frame_behind_and_the_pools_go_on() {
        // A flush (rav1g_decoder_flush) parks the parser's threads: a tile of the frame behind that waits
        // for the frame in front would wait for ever, so the take returns while the flush is on.
        let pools = Arc::new(Pools::new([20, 20, 20], [20, 20, 20]));
        pools.begin(0, 20);
        pools.begin(1, 20);
        let (handle, done) = take_on_thread(&pools, 1, 16);
        assert!(!done.load(Ordering::Acquire));
        pools.set_flushing(true);
        assert_eq!(handle.join().unwrap(), Take::Stopped);
        assert_eq!(pools.take(0, Kind::ModeInfo, 1), Take::Stopped);
        // The dropped frames end and give back what they hold; the pools serve the next frame as ever.
        for slot in 0..2 {
            pools.finish_parse(slot, false, |_, _, _| unreachable!("a dropped frame copies nothing"));
            pools.release(slot, |_, _, _| {});
        }
        pools.set_flushing(false);
        pools.begin(2, 20);
        assert_eq!(piece(pools.take(2, Kind::ModeInfo, 4)), (0, 4));
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
    fn a_pool_below_the_worst_case_leaves_the_frame_in_front_its_share() {
        // Half the worst case of a frame of 40 cells.
        let pools = Arc::new(Pools::new([20, 40, 40], [20, 40, 40]));
        pools.begin(0, 40);
        pools.begin(1, 40);
        // The frame in front has parsed nothing: it can need all of the pool.
        let (handle, done) = take_on_thread(&pools, 1, 16);
        assert!(!done.load(Ordering::Acquire));
        // Ten of its cells are parsed and it holds one chunk: of the 20 chunks it can need the 15 of
        // its 30 cells and the one it holds, so four are the frame's behind. (Held against a chunk a
        // cell it could need 31: the frame behind would wait until 21 cells are parsed.)
        assert_eq!(piece(pools.take(0, Kind::ModeInfo, 1)), (0, 1));
        pools.progress(0, 10);
        assert_eq!(piece(handle.join().unwrap()), (1, 4));
        let (handle, done) = take_on_thread(&pools, 1, 16);
        assert!(!done.load(Ordering::Acquire));
        // All of it parsed with that one chunk: the rest of the pool is free for the frame behind.
        pools.progress(0, 30);
        assert_eq!(piece(handle.join().unwrap()), (5, 15));
        // The frame in front does not fit now, and the pools say what was not its own.
        assert_eq!(pools.take(0, Kind::ModeInfo, 1), Take::Full);
        assert_eq!(pools.held_behind(0, Kind::ModeInfo), 19);
        assert_eq!(pools.held_behind(1, Kind::ModeInfo), 0);
        let stats = pools.stats();
        assert_eq!((stats.waits_for_gpu, stats.waits_for_frame), (0, 2));
    }

    #[test]
    fn growth_keeps_younger_pieces_and_pending_bindings_live() {
        let kind = Kind::ModeInfo;
        let pools = Pools::new([8; KINDS], [8; KINDS]);
        pools.begin(0, 16);
        // Even a recording with no pieces can bind the initial buffer.
        pools.parsed(0);
        pools.begin(1, 16);
        assert_eq!(piece(pools.take(1, kind, 2)), (0, 2));
        pools.begin(2, 16);
        pools.progress(1, 8);
        assert_eq!(piece(pools.take(2, kind, 2)), (2, 2));
        assert_eq!(piece(pools.take(1, kind, 4)), (4, 4));
        assert_eq!(
            pools.take_growing(1, kind, 10, 16, |total| {
                assert_eq!(total, 16 + 2);
                true
            }),
            Take::Piece { first: 8, chunks: 10, grown: true }
        );
        let mut copies = Vec::new();
        assert_eq!(pools.finish_parse(1, true, |kind, first, chunks| copies.push((kind, first, chunks))), 1);
        assert_eq!(copies, [(kind, 0, 2), (kind, 4, 4)]);
        pools.release_with(1, |_, _, _, grown| assert!(grown), |_| panic!("slot 2 is parsing"));
        pools.release_with(2, |_, _, _, grown| assert!(!grown), |_| panic!("slot 0 is recorded"));
        let mut retired = Vec::new();
        pools.release_with(0, |_, _, _, _| unreachable!(), |kind| retired.push(kind));
        assert_eq!(retired, [kind]);
    }

    #[test]
    fn a_refused_growth_leaves_the_pool_usable() {
        let kind = Kind::Coefs;
        let pools = Pools::new([8; KINDS], [8; KINDS]);
        pools.begin(0, 16);
        assert_eq!(piece(pools.take(0, kind, 8)), (0, 8));
        assert_eq!(pools.take_growing(0, kind, 1, 24, |_| false), Take::Refused);
        assert_eq!(pools.take_growing(0, kind, 1, 24, |_| unreachable!()), Take::Refused);
        assert_eq!(pools.stats().total, [8; KINDS]);
        pools.release_with(0, |_, _, _, grown| assert!(!grown), |_| unreachable!());
        pools.begin(0, 16);
        assert_eq!(piece(pools.take(0, kind, 8)), (0, 8));
        assert_eq!(pools.take_growing(0, kind, 1, 24, |_| true), Take::Piece { first: 8, chunks: 1, grown: true });
    }

    #[test]
    fn the_share_of_a_frame_in_a_pool() {
        let frame = |cells| Frame {
            order: 1,
            in_parse: true,
            held: [0; KINDS],
            bound: [0; KINDS],
            cells,
            pieces: Vec::new(),
            old_bindings: 0,
            new_bindings: 0,
            refused: 0,
        };
        // A pool of the frame's cells, or of more: a chunk a cell.
        assert_eq!(frame(2040).can_need(2040, 2040), 2040);
        assert_eq!(frame(2040).can_need(700, 2040), 700);
        assert_eq!(frame(510).can_need(510, 1020), 510);
        // Half the worst case of the frame: the whole pool at its begin, then half a chunk a cell, rounded up.
        assert_eq!(frame(2040).can_need(2040, 1020), 1020);
        assert_eq!(frame(2040).can_need(1021, 1020), 511);
        assert_eq!(frame(2040).can_need(1, 1020), 1);
        assert_eq!(frame(2040).can_need(0, 1020), 0);
        // Four fifths (the coefficient pool of the low memory part).
        assert_eq!(frame(2040).can_need(100, 1632), 80);
        assert_eq!(frame(2040).can_need(2040, 1632), 1632);
        assert_eq!(frame(2040).can_need(1, 1632), 1);
        // A frame of 1080p in the decoder for 4K: its cells.
        assert_eq!(frame(510).can_need(510, 1632), 510);
    }

    #[test]
    fn the_coefficient_pool_at_four_fifths_leaves_the_frame_in_front_its_share() {
        // The pool of a decoder for 3840x2176 with `AV1_LOW_MEMORY_COEFS`: 1632 of 2040 chunks under the
        // rule and 63 for the ends of pieces, which are the oldest frame's alone.
        let pools = Arc::new(Pools::new([2040, 1695, 2040], [2040, 1632, 2040]));
        let coefs = |pools: &Arc<Pools>, slot: usize, want: usize| {
            let pools = pools.clone();
            let done = Arc::new(AtomicBool::new(false));
            let flag = done.clone();
            let handle = thread::spawn(move || {
                let taken = pools.take(slot, Kind::Coefs, want);
                flag.store(true, Ordering::Release);
                taken
            });
            thread::sleep(Duration::from_millis(100));
            (handle, done)
        };
        pools.begin(0, 2040);
        pools.begin(1, 2040);
        // The other pools hold the worst case: the frame behind waits for nothing but the bound.
        // The frame in front has parsed nothing: the whole of the rule is what it can need.
        let (handle, done) = coefs(&pools, 1, 16);
        assert!(!done.load(Ordering::Acquire));
        // A piece of 16 and 20 cells parsed: it can need the 16 it holds and 1616 for its 2020 cells,
        // which is the pool. The frame behind still waits.
        assert_eq!(piece(pools.take(0, Kind::Coefs, 16)), (0, 16));
        pools.progress(0, 20);
        assert!(!done.load(Ordering::Acquire));
        // 40 cells parsed with that piece: 16 + 1600 of 1632, and the frame behind has its piece. Held
        // against a chunk a cell it would get its first chunk when 425 cells are parsed.
        pools.progress(0, 20);
        assert_eq!(piece(handle.join().unwrap()), (16, 16));
        // Half of the frame in front is parsed with those 16 chunks. It takes a large piece for the
        // rest: at its end it can hold the 16 and the 816 of its 1020 cells, the piece is of those.
        pools.progress(0, 980);
        assert_eq!(piece(pools.take(0, Kind::Coefs, 304)), (32, 304));
        // So of the 1632 the frame behind can have 800: the 16 it holds, and 784 more.
        assert_eq!(piece(pools.take(1, Kind::Coefs, 400)), (336, 400));
        assert_eq!(piece(pools.take(1, Kind::Coefs, 400)), (736, 384));
        let (handle, done) = coefs(&pools, 1, 1);
        assert!(!done.load(Ordering::Acquire));
        // The oldest frame in parse takes what is free, the chunks above the rule too.
        assert_eq!(piece(pools.take(0, Kind::Coefs, 2000)), (1120, 575));
        assert_eq!(pools.take(0, Kind::Coefs, 1), Take::Full);
        assert_eq!(pools.held_behind(0, Kind::Coefs), 800);
        pools.stop();
        assert_eq!(handle.join().unwrap(), Take::Stopped);
        // The waits are counted by pool.
        assert_eq!(pools.take_stat_waits(), [[0, 0], [0, 2], [0, 0]]);
        assert_eq!(pools.take_stat_waits(), [[0; 2]; KINDS]);
        let stats = pools.stats();
        assert_eq!((stats.waits_for_gpu, stats.waits_for_frame), (0, 2));
        assert_eq!(stats.peak, [0, 1695, 0]);
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

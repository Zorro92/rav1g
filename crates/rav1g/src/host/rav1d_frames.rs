// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! More than one frame in parse (docs/phase2-rav1d.md, step 4): frames as values, the ordered queue of
//! their submission, and the references the host counts. The host of the sink (`rav1d_host.rs`) uses this
//! when the decoder was created for it (`Av1Core::frames_in_parse` above 1); with one frame in parse
//! nothing here exists and a frame is "the frame of the decoder" as before.
//!
//! **A frame is a value.** When its parse begins (the thread of the decode call, frames in decode order)
//! it gets an [`Entry`]: its pipeline slot, a copy of its header as the stages read it, the memory for
//! what its tiles write besides the slot's buffers (loop restoration units), its timers, and a reference
//! on its own frame description and on those of its seven references. The tiles of the frame are parsed
//! by rav1d's threads with that entry, next to the tiles of other frames. Nothing of it is in "the
//! current frame" of the decoder.
//!
//! **Recording and submission are a thread of their own, in decode order.** The entries are a queue in
//! the order the frames began. A frame whose parse ends is marked, whichever thread that happens on and
//! in whatever order; the submission thread takes the entry at the head of the queue when it is marked,
//! records the frame's stages into the decoder's command list, submits it and hands the frame to the
//! completion thread, exactly as the decode thread does with one frame in parse. So the GPU gets the
//! frames in decode order with their references complete, and the output is in the order of the stream.
//! A `show_existing_frame` is an entry in the same queue: its output copy is recorded when the frames
//! before it are.
//!
//! **A failed frame fails the frames that reference it.** rav1d goes on parsing a frame whose reference
//! failed in another thread; the host does not give such a frame to the GPU. A frame description has a
//! mark for "failed"; an entry is recorded only if its own parse was good and none of its references has
//! the mark, which is known by then: the frames before it in the queue are through. A frame that is not
//! recorded returns its slot and gets the mark itself. The frame description of a failed frame stays in
//! the reference map and counts as an empty slot there (with one frame in parse the slot is emptied; here
//! later frames may already hold it).
//!
//! **Slots and pools.** A frame in parse holds a pipeline slot, which its setup waits for, and takes
//! its mode info records, coefficients and transform block records from the pools of the decoder as it
//! is parsed (`pool.rs`). Frames wait for each other: a frame's parse waits for the rows of its
//! references, and a frame is recorded only behind the frames before it. So a frame that can only go on
//! with memory that later frames hold would wait for ever: the rule of `Pools::take` keeps what the
//! oldest frame in parse can still need within its reach. (Before the pools a frame took a second slot
//! for more room, and the rule was about slots.)

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use super::completion::{decode_sef_begin, rav1g_decode_sef_submit};
use super::frame::{
    av1_release_fb_callback, bind_frame_for_record, decode_frame_record, rav1g_decode_frame_submit, release_frame,
    release_unsubmitted_frame,
};
use super::log::log;
use super::perf::frame_timers;
use super::types::{Av1Core, av1_frame_thread_data, av1_perf_accum};
use crate::sys;
use crate::sys::parser::{
    AV1_COMMON, AV1Decoder, MACROBLOCKD, RefCntBuffer, RestorationUnitInfo, av1_decoded_frame_buffer_t,
};

/// The entries of the queue: frames between the begin of their parse and their submission, and shown
/// frames (`show_existing_frame`) that wait for their copy. A frame holds a pipeline slot, of which a
/// decoder has at most four; the rest is room for shown frames between them. A begin that finds no entry
/// free waits for the submission thread.
pub(super) const ENTRIES: usize = 8;
/// The frame descriptions of a decoder: the eight reference slots, and the frame of every entry; twice
/// that, so that a description is never looked for in vain.
pub(super) const FRAME_DESCRIPTIONS: usize = 2 * (8 + ENTRIES);

/// What a decode call failed with.
pub(super) struct Failure {
    pub code: sys::aom_codec_err_t,
    pub detail: String,
}

/// A cleared struct of the entropy decoder on the heap.
pub(super) fn zeroed_box<T>() -> Box<T> {
    let layout = std::alloc::Layout::new::<T>();
    // SAFETY: used for C structs of numbers and pointers, for which all zero bytes are a value; the
    // allocation has the layout of `T`.
    unsafe {
        let memory = std::alloc::alloc_zeroed(layout);
        if memory.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Box::from_raw(memory.cast::<T>())
    }
}

// ---------------------------------------------------------------------------------------------------------
// The frame descriptions.
// ---------------------------------------------------------------------------------------------------------

/// The frame descriptions of a decoder (frame buffer, order hint, film grain: `RefCntBuffer`), which the
/// reference map and the frames in parse refer to, with their reference counts. With one frame in parse
/// only the thread of the decode call uses them; with more, the submission thread gives the references
/// of a frame back, so the counts are kept under a lock.
pub(super) struct Descriptions {
    list: Box<[UnsafeCell<RefCntBuffer>]>,
    /// The frame of the description failed: it was not given to the GPU, and a frame that references it
    /// is not either.
    failed: Box<[AtomicBool]>,
    /// Guards the reference counts.
    counts: Mutex<()>,
}

// SAFETY: a description is written by the thread that sets its frame up, before any other thread has
// the pointer, and read after that; its reference count is read and written under `counts`; the fields of
// its frame buffer that the recording writes (`hw_show_image`, the visible width of a frame with
// superres) are the recording thread's while the frame is recorded.
unsafe impl Send for Descriptions {}
// SAFETY: as above.
unsafe impl Sync for Descriptions {}

impl Descriptions {
    pub(super) fn new() -> Descriptions {
        Descriptions {
            // SAFETY: a C struct for which all zero is a value.
            list: (0..FRAME_DESCRIPTIONS).map(|_| UnsafeCell::new(unsafe { std::mem::zeroed() })).collect(),
            failed: (0..FRAME_DESCRIPTIONS).map(|_| AtomicBool::new(false)).collect(),
            counts: Mutex::new(()),
        }
    }

    fn counts(&self) -> MutexGuard<'_, ()> {
        self.counts.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The mark of a description of this list.
    fn mark(&self, frame: *const RefCntBuffer) -> &AtomicBool {
        let first = self.list.as_ptr().cast::<RefCntBuffer>();
        // SAFETY: `frame` is a description of this list (the callers' contract), so both pointers are
        // in the one allocation.
        let index = unsafe { frame.offset_from(first) };
        &self.failed[index as usize]
    }

    /// A description nothing uses, cleared, with one reference.
    pub(super) fn new_frame(&self) -> Option<*mut RefCntBuffer> {
        let _counts = self.counts();
        for (cell, failed) in self.list.iter().zip(self.failed.iter()) {
            let frame = cell.get();
            // SAFETY: the description is read and, when nothing refers to it, written under the lock of
            // the counts.
            unsafe {
                if (*frame).ref_count == 0 {
                    frame.write(std::mem::zeroed());
                    (*frame).ref_count = 1;
                    failed.store(false, Ordering::Relaxed);
                    return Some(frame);
                }
            }
        }
        None
    }

    /// One more reference to a description.
    ///
    /// # Safety
    /// `frame` is a description of this list that has a reference.
    pub(super) unsafe fn add_ref(&self, frame: *mut RefCntBuffer) {
        let _counts = self.counts();
        // SAFETY: the caller's contract; the count is written under its lock.
        unsafe { (*frame).ref_count += 1 };
    }

    /// Gives a reference to a description back; with its last one the frame buffer returns to the pool,
    /// and an output image the frame still had goes back to the application.
    ///
    /// # Safety
    /// `frame` is null or a description of this list with a reference; `dec` is the decoder.
    pub(super) unsafe fn unref(&self, dec: *mut Av1Core, frame: *mut RefCntBuffer) {
        if frame.is_null() {
            return;
        }
        // SAFETY: the caller's contract.
        unsafe {
            {
                let _counts = self.counts();
                (*frame).ref_count -= 1;
                if (*frame).ref_count != 0 {
                    return;
                }
                // Nothing refers to it any more. It is not free before its frame buffer is back: the
                // count says "being released" until then, so that no thread takes the description.
                (*frame).ref_count = -1;
            }
            av1_release_fb_callback(dec.cast(), &raw mut (*frame).buf);
            let _counts = self.counts();
            (*frame).ref_count = 0;
        }
    }

    /// A description that was taken and is not used after all (its frame got no frame buffer).
    ///
    /// # Safety
    /// `frame` is a description of [`Self::new_frame`] that no other thread has.
    pub(super) unsafe fn abandon(&self, frame: *mut RefCntBuffer) {
        let _counts = self.counts();
        // SAFETY: the caller's contract.
        unsafe { (*frame).ref_count = 0 };
    }

    /// The frame of the description failed.
    ///
    /// # Safety
    /// `frame` is a description of this list.
    pub(super) unsafe fn fail(&self, frame: *const RefCntBuffer) {
        self.mark(frame).store(true, Ordering::Release);
    }

    /// Whether the frame of the description is known to have failed. A frame that is still in parse, or
    /// waits for its submission, has not failed yet.
    ///
    /// # Safety
    /// `frame` is a description of this list.
    pub(super) unsafe fn failed(&self, frame: *const RefCntBuffer) -> bool {
        self.mark(frame).load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------------------------------------
// The entries.
// ---------------------------------------------------------------------------------------------------------

/// The header of a frame as the stages read it: the block context and the frame header of the entropy
/// decoder's structs, which are the first two members of `AV1Decoder`.
#[repr(C)]
pub(super) struct Header {
    pub mb: MACROBLOCKD,
    pub common: AV1_COMMON,
}

/// A frame between the begin of its parse and its submission, or a shown frame that waits for its copy.
///
/// Written by the thread of the decode call when the frame begins, before the entry is in the queue and
/// before a tile of the frame is parsed; read by the threads that parse the frame's tiles (which add to
/// its timers as atomics, and write the loop restoration units, each unit by one tile); read and
/// released by the submission thread when the parse has ended.
pub(super) struct Entry {
    /// A `show_existing_frame`: no slot, no parse; `cur` is the frame that is shown.
    pub shown: bool,
    /// The header of the frame, as it was when the frame was set up.
    pub header: Box<Header>,
    /// The loop restoration units of the frame's planes (`rst_info[..].unit_info` of the header).
    pub lr: [Vec<RestorationUnitInfo>; 3],
    /// The pipeline slot of the frame.
    pub td: *mut av1_frame_thread_data,
    /// The description of the frame, and those of its references: the entry has a reference on each.
    pub cur: *mut RefCntBuffer,
    pub refs: [*mut RefCntBuffer; 7],
    /// The frame is the one of its temporal unit that is output (asked when it began, while the data of
    /// the decode call was there).
    pub is_output: bool,
    /// What the application gave the decode call of the frame.
    pub user_priv: *mut c_void,
    /// The start of the parse, for its timer.
    pub parse_start: i64,
    /// The timers of the frame.
    pub acc: av1_perf_accum,
}

/// What the threads share of the queue, under its lock.
struct Shared {
    /// The entries in decode order: those that are not submitted yet.
    order: VecDeque<usize>,
    /// The entry is a frame's, from its begin until the submission thread is through with it.
    taken: [bool; ENTRIES],
    /// How the parse of the entry's frame ended; none while it is in parse. A shown frame has "good"
    /// from the start.
    parsed: [Option<bool>; ENTRIES],
    /// What failed since the last decode call took one: an error of a frame's parse or of its
    /// submission, which the next decode call (or the drain) returns.
    failures: VecDeque<Failure>,
    /// The queue is closed: the decoder is released, or the submission thread ended with a panic.
    /// Nothing waits any more.
    stop: bool,
    /// The submission thread ended with a panic.
    broken: bool,
}

/// The frames in parse of a decoder and the queue of their submission.
pub(crate) struct Frames {
    dec: *mut Av1Core,
    entries: Box<[UnsafeCell<Entry>]>,
    shared: Mutex<Shared>,
    changed: Condvar,
    /// The entry of the frame with a number of the sink (`SinkHost`).
    by_sink: Box<[AtomicUsize]>,
    /// The header of the frame that is being recorded: the stages read a frame through the structs of
    /// the entropy decoder, and the submission thread loads the header of the entry here.
    rec: UnsafeCell<Box<AV1Decoder>>,
    pub(super) descriptions: std::sync::Arc<Descriptions>,
}

// SAFETY: an entry is used as its comment says, by one thread at a time or read-only; the decoder is
// used as the decode thread of a decoder with one frame in parse uses it, by the thread of the decode
// call for the setup of a frame and by the submission thread for its recording (see the module).
unsafe impl Send for Frames {}
// SAFETY: as above.
unsafe impl Sync for Frames {}

impl Frames {
    /// `sink_frames`: the frames the sink may have in parse at once.
    pub(super) fn new(dec: *mut Av1Core, sink_frames: usize, descriptions: std::sync::Arc<Descriptions>) -> Frames {
        let entries = (0..ENTRIES)
            .map(|_| {
                UnsafeCell::new(Entry {
                    shown: false,
                    header: zeroed_box(),
                    lr: [Vec::new(), Vec::new(), Vec::new()],
                    td: std::ptr::null_mut(),
                    cur: std::ptr::null_mut(),
                    refs: [std::ptr::null_mut(); 7],
                    is_output: false,
                    user_priv: std::ptr::null_mut(),
                    parse_start: 0,
                    // SAFETY: numbers; all zero is a value.
                    acc: unsafe { std::mem::zeroed() },
                })
            })
            .collect();
        let mut rec = zeroed_box::<AV1Decoder>();
        rec.gpu_decoder = dec.cast();
        Frames {
            dec,
            entries,
            shared: Mutex::new(Shared {
                order: VecDeque::with_capacity(ENTRIES),
                taken: [false; ENTRIES],
                parsed: [None; ENTRIES],
                failures: VecDeque::new(),
                stop: false,
                broken: false,
            }),
            changed: Condvar::new(),
            by_sink: (0..sink_frames).map(|_| AtomicUsize::new(0)).collect(),
            rec: UnsafeCell::new(rec),
            descriptions,
        }
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait<'a>(&self, shared: MutexGuard<'a, Shared>) -> MutexGuard<'a, Shared> {
        self.changed.wait(shared).unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An entry.
    pub(super) fn entry(&self, index: usize) -> *mut Entry {
        self.entries[index].get()
    }

    /// The entry of the frame with a number of the sink.
    pub(super) fn entry_of(&self, sink_frame: usize) -> usize {
        self.by_sink[sink_frame].load(Ordering::Acquire)
    }

    /// The frame with a number of the sink has this entry, until its parse ends.
    pub(super) fn bind(&self, sink_frame: usize, index: usize) {
        self.by_sink[sink_frame].store(index, Ordering::Release);
    }

    /// An entry for a frame that begins or is shown; waits for one when all are in the queue. The entry
    /// is the caller's to fill: it is not in the queue yet ([`Self::push`]). `None`: the queue is closed.
    pub(super) fn acquire(&self) -> Option<usize> {
        let mut shared = self.shared();
        loop {
            if shared.stop {
                return None;
            }
            if let Some(index) = shared.taken.iter().position(|taken| !taken) {
                shared.taken[index] = true;
                shared.parsed[index] = None;
                return Some(index);
            }
            shared = self.wait(shared);
        }
    }

    /// An entry that was acquired and is not in the queue is free again.
    pub(super) fn release(&self, index: usize) {
        let mut shared = self.shared();
        shared.taken[index] = false;
        drop(shared);
        self.changed.notify_all();
    }

    /// The entry is the next in decode order: it goes to the end of the queue. `parsed`: how its parse
    /// ended, for an entry that has none (a shown frame); none for a frame whose parse begins.
    pub(super) fn push(&self, index: usize, parsed: Option<bool>) {
        let mut shared = self.shared();
        shared.parsed[index] = parsed;
        shared.order.push_back(index);
        drop(shared);
        self.changed.notify_all();
    }

    /// A frame whose setup failed behind [`Self::push`] leaves the queue, of which it is the last, and
    /// its entry is free again.
    pub(super) fn abort(&self, index: usize) {
        let mut shared = self.shared();
        if shared.order.back() == Some(&index) {
            shared.order.pop_back();
        }
        shared.taken[index] = false;
        drop(shared);
        self.changed.notify_all();
    }

    /// The parse of the entry's frame has ended. Any thread.
    pub(super) fn parsed(&self, index: usize, ok: bool) {
        let mut shared = self.shared();
        shared.parsed[index] = Some(ok);
        drop(shared);
        self.changed.notify_all();
    }

    /// Something failed that a decode call has to return.
    pub(super) fn fail(&self, code: sys::aom_codec_err_t, detail: impl Into<String>) {
        self.shared().failures.push_back(Failure { code, detail: detail.into() });
    }

    /// The oldest failure no decode call has returned yet.
    pub(super) fn take_failure(&self) -> Option<Failure> {
        self.shared().failures.pop_front()
    }

    /// The submission thread ended with a panic.
    pub(super) fn broken(&self) -> bool {
        self.shared().broken
    }

    /// Waits until the queue is empty: every frame that has begun is submitted or has failed. Returns at
    /// once when the queue is closed. The caller has waited for the parse of the frames
    /// (`Parser::wait`).
    pub(super) fn drain(&self) {
        let mut shared = self.shared();
        while !shared.order.is_empty() && !shared.stop {
            shared = self.wait(shared);
        }
    }

    /// Closes the queue: the submission thread ends, and nothing waits for it any more.
    pub(super) fn stop(&self) {
        self.shared().stop = true;
        self.changed.notify_all();
    }

    /// The submission thread: takes the entries in decode order as their parse ends, until the queue is
    /// closed.
    ///
    /// # Safety
    /// The decoder outlives the thread, and no other thread records frames of it.
    pub(super) unsafe fn run(&self) {
        loop {
            let (index, ok) = {
                let mut shared = self.shared();
                loop {
                    if shared.stop {
                        return;
                    }
                    let head = shared.order.front().copied();
                    if let Some((index, ok)) = head.and_then(|index| Some((index, shared.parsed[index]?))) {
                        break (index, ok);
                    }
                    shared = self.wait(shared);
                }
            };
            // SAFETY: the entry at the head of the queue, whose parse has ended: it is this thread's.
            let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { self.finish(index, ok) }));
            let mut shared = self.shared();
            if finished.is_err() {
                // A bug. The frame is in a state nothing is known about; the decoder decodes nothing
                // more, and nothing waits for this thread.
                log(
                    true,
                    "PARSER the submission thread of the decoder stopped with a panic; the decoder decodes nothing more",
                );
                shared.failures.push_back(Failure {
                    code: sys::AOM_CODEC_ERROR,
                    detail: String::from("The submission of a frame stopped with a panic"),
                });
                shared.stop = true;
                shared.broken = true;
            } else {
                shared.order.pop_front();
                shared.taken[index] = false;
                shared.parsed[index] = None;
            }
            let stop = shared.stop;
            drop(shared);
            self.changed.notify_all();
            if stop {
                return;
            }
        }
    }

    /// The header of an entry becomes the header the stages record from.
    ///
    /// # Safety
    /// The submission thread, with an entry that is its own.
    unsafe fn load(&self, entry: &Entry) -> *mut AV1Decoder {
        // SAFETY: the caller's contract; the two members are the first two of the decoder's struct, in
        // this order (`Header`).
        unsafe {
            let rec: *mut AV1Decoder = &raw mut **self.rec.get();
            (&raw mut (*rec).mb).copy_from_nonoverlapping(&raw const entry.header.mb, 1);
            (&raw mut (*rec).common).copy_from_nonoverlapping(&raw const entry.header.common, 1);
            // The one pointer of the block context into the header it belongs to.
            (*rec).mb.global_motion = (*rec).common.global_motion.as_mut_ptr();
            (*rec).user_priv = entry.user_priv;
            rec
        }
    }

    /// The entry at the head of the queue, whose parse has ended with `ok`: its frame is recorded and
    /// submitted, or returns what it holds; a shown frame gets its output copy.
    ///
    /// # Safety
    /// The submission thread, with the entry at the head of the queue.
    unsafe fn finish(&self, index: usize, ok: bool) {
        // SAFETY: the caller's contract, for all of this function: the entry is this thread's, and the
        // functions are called as the decode thread of a decoder with one frame in parse calls them.
        unsafe {
            let dec = self.dec;
            let entry = &mut *self.entry(index);
            let descriptions = &*self.descriptions;
            // The timers of this thread are the frame's.
            let _timers = frame_timers(&raw mut entry.acc);
            if entry.shown {
                let shown = entry.cur;
                if descriptions.failed(shown) || (*shown).buf.hw_buffer.is_null() {
                    self.fail(
                        sys::AOM_CODEC_CORRUPT_FRAME,
                        "Failed to decode tile data: the frame a show_existing_frame names failed, so it is not shown",
                    );
                } else {
                    let rec = self.load(entry);
                    let mut fb: av1_decoded_frame_buffer_t = std::mem::zeroed();
                    let image_state = decode_sef_begin(rec, &mut fb, Some(entry.is_output));
                    if image_state == 2 {
                        release_unsubmitted_frame(dec);
                        self.fail(sys::AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
                    } else if image_state >= 0 {
                        rav1g_decode_sef_submit(rec, image_state, &fb);
                    }
                    (*rec).common.cur_frame = std::ptr::null_mut();
                }
                descriptions.unref(dec, shown);
                entry.cur = std::ptr::null_mut();
                return;
            }

            // The frames before this one are through, so every reference has its verdict.
            let failed_reference =
                entry.refs.iter().any(|&reference| !reference.is_null() && descriptions.failed(reference));
            if !ok || failed_reference {
                // The frame returns its slot and its buffers and is not submitted. Its description stays
                // where later frames have it, with the mark.
                release_frame(dec, entry.td);
                descriptions.fail(entry.cur);
                self.fail(
                    sys::AOM_CODEC_CORRUPT_FRAME,
                    if ok {
                        "Failed to decode tile data: a reference of the frame failed, so the frame is not decoded"
                    } else {
                        "Failed to decode tile data: the parse of the frame ended with an error"
                    },
                );
            } else {
                bind_frame_for_record(dec, entry.td);
                let rec = self.load(entry);
                let mut fb: av1_decoded_frame_buffer_t = std::mem::zeroed();
                let mut lap_start = 0;
                let mut image_state = decode_frame_record(rec, &mut fb, &mut lap_start, Some(entry.is_output));
                if image_state == 2 {
                    // The application has no image for the frame. The frame is decoded all the same: it
                    // is a reference of the frames that follow, and only its output is lost.
                    self.fail(sys::AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
                    image_state = 0;
                }
                rav1g_decode_frame_submit(rec, image_state, &fb, lap_start);
                (*rec).common.cur_frame = std::ptr::null_mut();
            }
            entry.td = std::ptr::null_mut();
            for reference in std::mem::replace(&mut entry.refs, [std::ptr::null_mut(); 7]) {
                descriptions.unref(dec, reference);
            }
            descriptions.unref(dec, std::mem::replace(&mut entry.cur, std::ptr::null_mut()));
        }
    }
}

// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Timing instrumentation of the test programs (`-gputime`, `dx/av1_perf.h`): CPU timers of the frame
//! that is being built, GPU timestamps behind the stages of its command list, and the record of a frame
//! that goes to the callback when the frame has completed on the GPU. Everything here is inert until
//! `av1_set_perf_callback` installs a callback; it never changes what is decoded.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicPtr, Ordering};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use super::core::commands;
use crate::gpu::d3d12::{com_ref, com_release, com_store, wrapped_ref};
use crate::gpu::{Device, Queue, Readback, TimestampQueries};
use crate::sys;
use crate::sys::host::{AV1Decoder, Av1Core, Av1PerfMaxMarks, av1_frame_thread_data, av1_perf_inter_detail};

const MAX_MARKS: usize = Av1PerfMaxMarks as usize;
const CPU_COUNT: usize = sys::AV1_PERF_CPU_COUNT as usize;
const GPU_STAGE_COUNT: usize = sys::AV1_PERF_GPU_STAGE_COUNT as usize;
const INTER_SECTION_COUNT: i32 = sys::AV1_PERF_INTER_SECTION_COUNT;
const INTER_LISTS: i32 = sys::AV1_PERF_INTER_LISTS;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
}

/// The callback of the timing run and its context (`av1_set_perf_callback`).
static PERF_CB: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static PERF_PRIV: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// `av1_set_perf_callback` (`dx/av1_perf.h`): the callback that gets the record of every frame, or none.
///
/// # Safety
/// `cb` is null or a function that may be called with `private` from the completion thread of any
/// decoder until it is replaced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_set_perf_callback(cb: sys::av1_perf_frame_cb_fn_t, private: *mut c_void) {
    PERF_PRIV.store(private, Ordering::SeqCst);
    PERF_CB.store(cb.map_or(std::ptr::null_mut(), |cb| cb as *mut c_void), Ordering::SeqCst);
}

/// The callback of the timing run with its context, when one is installed.
pub(crate) fn perf_callback() -> Option<(unsafe extern "C" fn(*mut c_void, *const sys::av1_perf_frame), *mut c_void)> {
    let cb = PERF_CB.load(Ordering::SeqCst);
    if cb.is_null() {
        return None;
    }
    // SAFETY: the pointer was stored from a function of this type.
    let cb = unsafe {
        std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void, *const sys::av1_perf_frame)>(cb)
    };
    Some((cb, PERF_PRIV.load(Ordering::SeqCst)))
}

pub(crate) fn perf_enabled() -> bool {
    !PERF_CB.load(Ordering::Relaxed).is_null()
}

/// Test player flag -gputimedetail (`av1_perf.h`): the inter prediction breakdown. 0 off, 1 timestamps
/// behind the sections of the inter stage and work counters, 2 also a UAV barrier and a timestamp behind
/// every dispatch. Nothing of it exists without a perf callback.
static PERF_DETAIL: AtomicI32 = AtomicI32::new(0);

/// `av1_set_perf_detail` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_perf_detail(mode: c_int) {
    PERF_DETAIL.store(mode.clamp(0, 2), Ordering::Relaxed);
}

/// The mode of the inter breakdown a frame is set up with: that of the flag while timing is on.
pub(crate) fn perf_detail_mode() -> c_int {
    if perf_enabled() { PERF_DETAIL.load(Ordering::Relaxed) } else { 0 }
}

/// Test flag -interfast (`dx/av1_perf.h`, `av1_set_inter_fast`; `dx/types.h`, `InterFastLists`): inter
/// blocks listed and predicted by the fraction of their motion vectors. A frame takes the path while this
/// is set, in a decoder that was created with room for the lists by class in its block maps
/// (`aom_codec_dec_cfg_t::inter_fast`), unless it has a scaled reference (the frame setup).
static INTER_FAST: AtomicI32 = AtomicI32::new(0);

/// `av1_set_inter_fast` (`dx/av1_perf.h`). `mode`: 0 off, or `AV1_INTER_FAST_ON` with the bits that keep
/// the original shader on the lists by class.
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_inter_fast(mode: c_int) {
    let on = mode & sys::AV1_INTER_FAST_ON as c_int != 0;
    INTER_FAST.store(if on { mode & 7 } else { 0 }, Ordering::Relaxed);
}

/// The mode of -interfast.
pub(crate) fn inter_fast_mode() -> c_int {
    INTER_FAST.load(Ordering::Relaxed)
}

/// The clock of the timers: `QueryPerformanceCounter`.
pub(crate) fn perf_now() -> i64 {
    let mut ticks = 0i64;
    // SAFETY: a valid out pointer; the call does not fail on the systems the decoder runs on.
    let _ = unsafe { QueryPerformanceCounter(&mut ticks) };
    ticks
}

/// Milliseconds per tick of [`perf_now`].
fn ticks_to_ms() -> f64 {
    let mut frequency = 0i64;
    // SAFETY: a valid out pointer.
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    1000.0 / frequency as f64
}

/// `av1_perf_ticks` (`dx/av1_core.h`): the start of a CPU timer; 0 while timing is off, and a start of 0
/// is ignored by the functions that add a time, so the callers need no checks.
#[unsafe(no_mangle)]
pub extern "C" fn av1_perf_ticks() -> i64 {
    if perf_enabled() { perf_now() } else { 0 }
}

/// A counter of the frame that is being built, which the tile threads add to as well.
///
/// # Safety
/// `counter` is a counter of the decoder's `perf_cur`, valid for `'a`.
unsafe fn counter<'a>(counter: *mut i64) -> &'a AtomicI64 {
    // SAFETY: the caller's contract; every thread uses these counters as atomics or while no other thread
    // has them (the snapshot at submit).
    unsafe { AtomicI64::from_ptr(counter) }
}

/// `av1_perf_add` of the C++: adds the time since `start_ticks` to a CPU timer of the frame that is being
/// built. Nothing for a start of 0.
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn perf_add(dec: *mut Av1Core, id: usize, start_ticks: i64) {
    if start_ticks == 0 {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe { counter(&raw mut (*dec).perf_cur.ticks[id]) }.fetch_add(perf_now() - start_ticks, Ordering::SeqCst);
}

/// `av1_perf_cpu_add` (`dx/av1_core.h`): [`perf_add`] for the entropy decoder.
///
/// # Safety
/// `pbi` is null or the entropy decoder's state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_perf_cpu_add(pbi: *mut AV1Decoder, id: c_int, start_ticks: i64) {
    // SAFETY: the caller's contract.
    unsafe {
        if start_ticks == 0 || pbi.is_null() || (*pbi).gpu_decoder.is_null() {
            return;
        }
        perf_add((*pbi).gpu_decoder, id as usize, start_ticks);
    }
}

/// Adds the time since `start_ticks` to a timer and returns the start of the next lap.
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn perf_lap(dec: *mut Av1Core, id: usize, start_ticks: i64) -> i64 {
    if start_ticks == 0 {
        return 0;
    }
    let now = perf_now();
    // SAFETY: the caller's contract.
    unsafe { counter(&raw mut (*dec).perf_cur.ticks[id]) }.fetch_add(now - start_ticks, Ordering::SeqCst);
    now
}

/// The time of a clear and the bytes it cleared.
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn perf_add_memset(dec: *mut Av1Core, id: usize, start_ticks: i64, bytes: usize) {
    if start_ticks == 0 {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe {
        counter(&raw mut (*dec).perf_cur.ticks[id]).fetch_add(perf_now() - start_ticks, Ordering::SeqCst);
        counter(&raw mut (*dec).perf_cur.memset_bytes).fetch_add(bytes as i64, Ordering::SeqCst);
    }
}

/// The timestamps of a slot's command list.
///
/// # Safety
/// `td` is a slot whose query heap field is null or holds a heap.
unsafe fn queries(td: &av1_frame_thread_data) -> Option<&TimestampQueries> {
    // SAFETY: the caller's contract.
    unsafe { wrapped_ref(&td.perf.query_heap) }
}

/// `av1_perf_gpu_mark` of the C++: a GPU timestamp at the current position of the frame's command list,
/// which closes `stage`.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn perf_gpu_mark(dec: *mut Av1Core, stage: c_int) {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(td) = (*dec).curr_frame_data.as_mut() else {
            return;
        };
        if td.perf.gpu_active == 0 || td.perf.mark_count >= MAX_MARKS as c_int {
            return;
        }
        let index = td.perf.mark_count as usize;
        commands(dec).timestamp(queries(td).expect("an active slot has its query heap"), index as u32);
        td.perf.mark_sub[index] = 0;
        td.perf.mark_stage[index] = stage as u8;
        td.perf.mark_count += 1;
    }
}

/// `av1_perf_inter_begin` of the C++: the inter breakdown of the frame of the current command list,
/// cleared for the stage to fill, or null when the frame has none.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn perf_inter_begin(dec: *mut Av1Core) -> *mut av1_perf_inter_detail {
    // SAFETY: the caller's contract; the record is memory of the C runtime, released with the decoder.
    unsafe {
        let Some(td) = (*dec).curr_frame_data.as_mut() else {
            return std::ptr::null_mut();
        };
        let p = &mut td.perf;
        if p.detail_mode == 0 {
            return std::ptr::null_mut();
        }
        if p.detail.is_null() {
            p.detail = malloc(size_of::<av1_perf_inter_detail>()).cast();
        }
        if p.detail.is_null() {
            p.detail_mode = 0;
            return std::ptr::null_mut();
        }
        p.detail.write_bytes(0, 1);
        (*p.detail).mode = p.detail_mode;
        p.detail_filled = 1;
        p.detail
    }
}

/// `av1_perf_inter_mark` of the C++: a timestamp behind a section of the inter stage, or behind one of
/// its dispatches (`list`, else -1). Its time is counted to `stage` as well, so the stage keeps the time
/// it has without the breakdown.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn perf_inter_mark(dec: *mut Av1Core, stage: c_int, section: c_int, list: c_int) {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(td) = (*dec).curr_frame_data.as_mut() else {
            return;
        };
        let p = &td.perf;
        if p.gpu_active == 0 || p.detail_mode == 0 || p.detail_filled == 0 || p.mark_count >= MAX_MARKS as c_int {
            return;
        }
        let index = td.perf.mark_count as usize;
        commands(dec).timestamp(queries(td).expect("an active slot has its query heap"), index as u32);
        td.perf.mark_sub[index] = ((section + 1) | ((list + 1) << 8)) as u16;
        td.perf.mark_stage[index] = stage as u8;
        td.perf.mark_count += 1;
        (*td.perf.detail).marks += 1;
    }
}

/// Start of a frame's command list: decides whether this frame is timed and creates the slot's query
/// heap and readback buffer on first use.
///
/// # Safety
/// `dec` is the decoder and `td` the slot (or output context) whose frame it starts to record.
pub(crate) unsafe fn perf_begin_command_buffer(dec: *mut Av1Core, td: *mut av1_frame_thread_data) {
    // SAFETY: the caller's contract.
    unsafe {
        let p = &mut (*td).perf;
        p.gpu_active = 0;
        p.mark_count = 0;
        p.detail_mode = 0;
        p.detail_filled = 0;
        p.collect = c_int::from(perf_enabled());
        if p.collect == 0 {
            return;
        }
        (*dec).perf_cur.t_record_start = perf_now();
        if (*dec).perf_cur.t_frame_start == 0 {
            (*dec).perf_cur.t_frame_start = (*dec).perf_cur.t_record_start;
        }

        if queries(&*td).is_none() && p.gpu_failed == 0 {
            // Timestamp queries need a direct or compute queue; the frequency must come from the queue
            // the decoder's lists execute on.
            let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device).map(Device::new);
            let frequency = com_ref::<_, ID3D12CommandQueue>(&(*dec).compute.queue)
                .and_then(|queue| Queue::new(queue).timestamp_frequency());
            let created = device.zip(frequency).and_then(|(device, frequency)| {
                let heap = device.create_timestamp_queries(MAX_MARKS as u32).ok()?;
                let readback = device.create_readback((size_of::<u64>() * MAX_MARKS) as u64).ok()?;
                Some((heap, readback, frequency))
            });
            match created {
                Some((heap, readback, frequency)) => {
                    com_store(&raw mut p.query_heap, heap.into_raw());
                    com_store(&raw mut p.readback, readback.into_raw());
                    (*dec).perf_gpu_freq = frequency;
                }
                None => {
                    com_release(&raw mut p.query_heap);
                    com_release(&raw mut p.readback);
                    p.gpu_failed = 1;
                }
            }
        }
        // The inter breakdown of a decoded frame, in the mode its tiles were counted with (the frame
        // setup). Its counters need no timestamps.
        if (*td).output_only == 0 {
            (*td).perf.detail_mode = (*dec).perf_inter_mode;
        }
        if queries(&*td).is_some() {
            (*td).perf.gpu_active = 1;
            perf_gpu_mark(dec, sys::AV1_PERF_GPU_BEGIN as c_int);
        }
    }
}

/// The timestamps of the frame's command list are resolved as its last command; they are read back only
/// after the frame's fence has completed.
///
/// # Safety
/// `dec` is the decoder while it records the frame of the slot `td`.
pub(crate) unsafe fn perf_resolve(dec: *mut Av1Core, td: *const av1_frame_thread_data) {
    // SAFETY: the caller's contract.
    unsafe {
        let p = &(*td).perf;
        if p.gpu_active == 0 || p.mark_count <= 0 {
            return;
        }
        let (Some(heap), Some(readback)) = (queries(&*td), wrapped_ref::<_, Readback>(&p.readback)) else {
            return;
        };
        commands(dec).resolve_timestamps(heap, p.mark_count as u32, readback);
    }
}

/// Completion thread, after the frame's fence: builds the record of the frame from the CPU snapshot taken
/// at submit and the resolved GPU timestamps.
///
/// # Safety
/// `dec` is the decoder and `td` the slot of a frame that has completed and is not back in its pool.
pub(crate) unsafe fn perf_collect(
    dec: *mut Av1Core,
    td: *mut av1_frame_thread_data,
    wait_start: i64,
    rec: &mut sys::av1_perf_frame,
) {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        let p = &mut (*td).perf;
        let now = perf_now();
        let to_ms = ticks_to_ms();

        *rec = std::mem::zeroed();
        rec.frame_number = p.acc.frame_number;
        rec.show_frame = p.acc.show_frame;
        rec.showable_frame = p.acc.showable_frame;
        rec.is_sef = p.acc.is_sef;
        rec.intra_frame = p.acc.intra_frame;
        rec.used_sec_slot = p.acc.used_sec_slot;
        rec.intra_iters = p.acc.intra_iters;
        rec.intra_v2 = p.acc.intra_v2;
        rec.intra_dispatches = p.acc.intra_stats[0];
        rec.intra_barriers = p.acc.intra_stats[1];
        rec.intra_groups = p.acc.intra_stats[2];
        rec.intra_passes = p.acc.intra_stats[3];
        rec.tile_count = p.acc.tile_count;
        for i in 0..CPU_COUNT {
            rec.cpu_ms[i] = p.acc.ticks[i] as f64 * to_ms;
        }
        rec.cpu_ms[sys::AV1_PERF_CPU_FENCE_WAIT as usize] = (now - wait_start) as f64 * to_ms;
        rec.cpu_ms[sys::AV1_PERF_CPU_GPU_LATENCY as usize] = (now - p.t_submit) as f64 * to_ms;
        rec.memset_bytes = p.acc.memset_bytes;
        rec.complete_ms = now as f64 * to_ms;
        rec.gpu_gap_ms = -1.0;

        // The inter breakdown goes to the callback as a copy this thread owns: the slot goes back to the
        // pool before the callback runs, and the decode thread may be recording the slot's next frame by
        // then.
        let mut detail: *mut sys::av1_perf_inter_detail = std::ptr::null_mut();
        if p.detail_mode != 0 && p.detail_filled != 0 && !p.detail.is_null() {
            if (*dec).perf_detail_out.is_null() {
                (*dec).perf_detail_out = malloc(size_of::<av1_perf_inter_detail>()).cast();
            }
            detail = (*dec).perf_detail_out.cast();
            if !detail.is_null() {
                std::ptr::copy_nonoverlapping(p.detail.cast::<sys::av1_perf_inter_detail>(), detail, 1);
            }
        }
        rec.inter = detail;

        let gpu_freq = (*dec).perf_gpu_freq;
        if p.gpu_active == 0 || p.mark_count <= 0 || gpu_freq == 0 {
            return;
        }
        let marks = p.mark_count as usize;
        let mut ts = [0u64; MAX_MARKS];
        let Some(readback) = wrapped_ref::<_, Readback>(&p.readback) else {
            return;
        };
        let read = readback.read(size_of::<u64>() * marks, |bytes| {
            for (value, chunk) in ts.iter_mut().zip(bytes.as_chunks::<8>().0) {
                *value = u64::from_ne_bytes(*chunk);
            }
        });
        if read.is_none() {
            return;
        }

        let gpu_to_ms = 1000.0 / gpu_freq as f64;
        for i in 1..marks {
            let stage = usize::from(p.mark_stage[i]);
            let sub = i32::from(p.mark_sub[i]);
            let ms = if ts[i] > ts[i - 1] { (ts[i] - ts[i - 1]) as f64 * gpu_to_ms } else { 0.0 };
            if stage < GPU_STAGE_COUNT {
                rec.gpu_stage_ms[stage] += ms;
                if sub == 0 {
                    rec.gpu_stage_hits[stage] += 1;
                }
            }
            rec.gpu_sum_ms += ms;
            if let Some(detail) = detail.as_mut() {
                // A timestamp of the breakdown, or the one that closes the inter stage (behind its last
                // barrier) or the reconstruction.
                let section = if sub != 0 {
                    (sub & 255) - 1
                } else if stage == sys::AV1_PERF_GPU_INTER as usize {
                    sys::AV1_PERF_INTER_SYNC2
                } else if stage == sys::AV1_PERF_GPU_RECON as usize {
                    sys::AV1_PERF_INTER_RECON
                } else {
                    -1
                };
                let list = if sub != 0 { (sub >> 8) - 1 } else { -1 };
                if (0..INTER_SECTION_COUNT).contains(&section) {
                    let section = &mut detail.sections[section as usize];
                    section.ms += ms;
                    section.timed = 1;
                    if (0..INTER_LISTS).contains(&list) {
                        section.lists[list as usize].ms += ms;
                    }
                }
            }
        }
        let first = ts[0];
        let last = ts[marks - 1];
        rec.gpu_span_ms = if last > first { (last - first) as f64 * gpu_to_ms } else { 0.0 };
        let last_end = (*dec).perf_gpu_last_end;
        if last_end != 0 {
            rec.gpu_gap_ms = if first > last_end { (first - last_end) as f64 * gpu_to_ms } else { 0.0 };
        }
        (*dec).perf_gpu_last_end = last;
        rec.gpu_marks = p.mark_count;
        rec.gpu_valid = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_take_the_values_of_the_header() {
        av1_set_perf_detail(5);
        assert_eq!(PERF_DETAIL.load(Ordering::Relaxed), 2);
        av1_set_perf_detail(-3);
        assert_eq!(PERF_DETAIL.load(Ordering::Relaxed), 0);
        // Without a callback a frame has no breakdown, whatever the flag says.
        av1_set_perf_detail(1);
        assert_eq!(perf_detail_mode(), 0);
        av1_set_perf_detail(0);

        av1_set_inter_fast(6);
        assert_eq!(inter_fast_mode(), 0);
        av1_set_inter_fast(7);
        assert_eq!(inter_fast_mode(), 7);
        av1_set_inter_fast(9);
        assert_eq!(inter_fast_mode(), 1);
        av1_set_inter_fast(0);
        assert_eq!(inter_fast_mode(), 0);
    }

    #[test]
    fn the_clock_runs_only_while_timing_is_on() {
        assert_eq!(av1_perf_ticks(), 0);
        let start = perf_now();
        assert!(perf_now() >= start);
    }
}

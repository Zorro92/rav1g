// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The process-wide switches of the decoder (`libav1/dx/av1_perf.h`): which shaders a stage uses, and the
//! instrumentation of the test programs. They hold for every decoder of the process and are read by the
//! decoders' threads.
//!
//! A switch is changed while no frame is in flight in any decoder: before the first temporal unit, or
//! after a drain, as the test player does at the start of a task. A change in between is not unsafe, but
//! a frame may then be decoded half with one setting and half with the other, which for the shader
//! switches gives a wrong frame.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Arc, RwLock};

use crate::sys;

/// A record of the timing run: one per command list of a decoder (`av1_perf_frame`).
pub type PerfFrame = sys::av1_perf_frame;
/// The buffers behind a GPU stage of a frame (`av1_stage_dump`).
pub type StageDump = sys::av1_stage_dump;

/// The fast inter path is on ([`set_inter_fast`]).
pub const INTER_FAST_ON: u32 = sys::AV1_INTER_FAST_ON as u32;
/// With [`INTER_FAST_ON`]: the blocks with one reference are listed and predicted by the original shader.
pub const INTER_FAST_SINGLE_ORIGINAL: u32 = sys::AV1_INTER_FAST_SINGLE_ORIGINAL as u32;
/// With [`INTER_FAST_ON`]: the same for the averaged compound blocks.
pub const INTER_FAST_COMPOUND_ORIGINAL: u32 = sys::AV1_INTER_FAST_COMPOUND_ORIGINAL as u32;
/// The lists of the fast inter path with the original shaders on all of them.
pub const INTER_FAST_LISTS_ONLY: u32 = sys::AV1_INTER_FAST_LISTS_ONLY as u32;

/// What [`set_poison_buffers`] fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(i32)]
pub enum Poison {
    #[default]
    Off = 0,
    /// The mode info pool.
    ModeInfo = 1,
    /// Every buffer the CPU writes for the shaders, and the constants of every stage in every frame.
    Buffers = 2,
}

/// CDEF through `cdef_filter_v2.hlsl` (true, the default) or `cdef_filter.hlsl`.
pub fn set_cdef_v2(enable: bool) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_cdef_v2(c_int::from(enable)) }
}

/// Intra prediction through `intra_v2.hlsl`; only in a decoder that was created with
/// [`crate::Settings::intra_v2`].
pub fn set_intra_v2(enable: bool) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_intra_v2(c_int::from(enable)) }
}

/// The optimised build of the original intra shader.
pub fn set_intra_opt(enable: bool) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_intra_opt(c_int::from(enable)) }
}

/// The clears of a pipeline slot on the completion thread instead of the decode thread. On by default;
/// `false` puts them back on the decode thread. The output is the same.
pub fn set_async_clear(enable: bool) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_async_clear(c_int::from(enable)) }
}

/// The fast inter path: 0, or [`INTER_FAST_ON`] with the `INTER_FAST_*` bits; only in a decoder that was
/// created with [`crate::Settings::inter_fast`].
pub fn set_inter_fast(mode: u32) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_inter_fast(mode as c_int) }
}

/// Experiment: how the horizontal loop filter is dispatched, 0 (one dispatch per plane), 1 or 2.
pub fn set_lf_h_split(mode: u32) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_lf_h_split(mode as c_int) }
}

/// Experiment: the 8-bit above edge filter bound of the intra shader.
pub fn set_intra_edge_fix(enable: bool) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_intra_edge_fix(c_int::from(enable)) }
}

/// Debugging: the decoders created after the call fill the mode info pool, or every buffer the CPU writes
/// for the shaders, with the byte 0xA5 when they create it, instead of leaving what the memory held; with
/// [`Poison::Buffers`] they also fill the constants of every stage again in every frame, right before the
/// stage writes them. The output must be the same.
pub fn set_poison_buffers(level: Poison) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_poison_buffers(level as c_int) }
}

/// The inter breakdown of the timing records: 0 off, 1 sections, 2 every dispatch alone.
pub fn set_perf_detail(mode: u32) {
    // SAFETY: sets a flag.
    unsafe { sys::av1_set_perf_detail(mode as c_int) }
}

/// Empty for the normal build of the shaders, `"uavsync"` for the experiment build.
pub fn shader_variant() -> String {
    // SAFETY: returns a static string.
    crate::error::text(unsafe { sys::av1_shader_variant() })
}

/// What gets the records of the timing run ([`set_perf_callback`]).
pub type PerfCallback = Arc<dyn Fn(&PerfFrame) + Send + Sync>;
/// What gets the stage dumps ([`set_stage_dump`]).
pub type DumpCallback = Arc<dyn Fn(&StageDump) + Send + Sync>;
/// What gets the log lines ([`set_log_callback`]): whether the line is an error, and the line.
pub type LogCallback = Arc<dyn Fn(bool, &str) + Send + Sync>;

static PERF: RwLock<Option<PerfCallback>> = RwLock::new(None);
static DUMP: RwLock<Option<DumpCallback>> = RwLock::new(None);
static LOG: RwLock<Option<LogCallback>> = RwLock::new(None);

fn current<T: Clone>(slot: &RwLock<Option<T>>) -> Option<T> {
    slot.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

fn store<T>(slot: &RwLock<Option<T>>, value: Option<T>) {
    *slot.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
}

unsafe extern "C" fn perf_frame(_context: *mut c_void, frame: *const sys::av1_perf_frame) {
    // SAFETY: the decoder passes a record that is valid during the call.
    if let (Some(callback), Some(frame)) = (current(&PERF), unsafe { frame.as_ref() }) {
        callback(frame);
    }
}

unsafe extern "C" fn stage_dump(_context: *mut c_void, dump: *const sys::av1_stage_dump) {
    // SAFETY: the decoder passes a dump that is valid during the call.
    if let (Some(callback), Some(dump)) = (current(&DUMP), unsafe { dump.as_ref() }) {
        callback(dump);
    }
}

unsafe extern "C" fn log_line(_context: *mut c_void, is_error: c_int, line: *const c_char) {
    if line.is_null() {
        return;
    }
    if let Some(callback) = current(&LOG) {
        // SAFETY: the decoder passes a NUL-terminated line.
        callback(is_error != 0, &unsafe { CStr::from_ptr(line) }.to_string_lossy());
    }
}

/// The timing run (the `-gputime` of the test programs): with a callback every decoder takes GPU
/// timestamps and CPU timers per stage and hands one record per command list to the callback, on its
/// completion thread. `None` switches the timing off.
pub fn set_perf_callback(callback: Option<PerfCallback>) {
    // SAFETY: the function given is valid for the life of the process and takes no context.
    unsafe {
        match callback {
            Some(callback) => {
                store(&PERF, Some(callback));
                sys::av1_set_perf_callback(Some(perf_frame), std::ptr::null_mut());
            }
            None => {
                sys::av1_set_perf_callback(None, std::ptr::null_mut());
                store(&PERF, None);
            }
        }
    }
}

/// The stage dumps (the `-dumpstages` of the test programs): the decoders copy the buffers behind every
/// GPU stage of the frames whose numbers (in decoding order, from 0 per decoder) are in `frames` and hand
/// them to the callback on their completion thread. `None`, or no frames, switches them off.
pub fn set_stage_dump(callback: Option<DumpCallback>, frames: &[i32]) {
    // SAFETY: the function given is valid for the life of the process and takes no context; the frame
    // numbers are copied by the call.
    unsafe {
        match callback.filter(|_| !frames.is_empty()) {
            Some(callback) => {
                store(&DUMP, Some(callback));
                sys::av1_set_stage_dump(Some(stage_dump), std::ptr::null_mut(), frames.as_ptr(), frames.len() as c_int);
            }
            None => {
                sys::av1_set_stage_dump(None, std::ptr::null_mut(), std::ptr::null(), 0);
                store(&DUMP, None);
            }
        }
    }
}

/// The log lines of the decoders ("MEMORY ...", errors): the callback gets whether the line is an error,
/// and the line, from whatever thread a decoder is on. Without one the lines go to the debugger output.
pub fn set_log_callback(callback: Option<LogCallback>) {
    // SAFETY: the function given is valid for the life of the process and takes no context.
    unsafe {
        match callback {
            Some(callback) => {
                store(&LOG, Some(callback));
                sys::av1_set_log_callback(Some(log_line), std::ptr::null_mut());
            }
            None => {
                sys::av1_set_log_callback(None, std::ptr::null_mut());
                store(&LOG, None);
            }
        }
    }
}

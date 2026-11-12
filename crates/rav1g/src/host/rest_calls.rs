// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What the C++ that is left of `av1_core.cpp` (`crates/rav1g-sys/glue/av1_core_rest.cpp`) calls of the
//! Rust host: `extern "C"` names for functions that are plain Rust functions otherwise. This module
//! shrinks as the callers become Rust and goes with the rest file.

use std::ffi::c_int;

use super::{perf, stage_dump};
use crate::sys;
use crate::sys::host::{AV1_COMMON, Av1Core, av1_frame_thread_data};

/// # Safety
/// As for [`perf::perf_add`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_perf_add(dec: *mut Av1Core, id: c_int, start_ticks: i64) {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_add(dec, id as usize, start_ticks) };
}

#[unsafe(no_mangle)]
pub extern "C" fn rav1g_perf_enabled() -> c_int {
    c_int::from(perf::perf_enabled())
}

#[unsafe(no_mangle)]
pub extern "C" fn rav1g_perf_detail_mode() -> c_int {
    perf::perf_detail_mode()
}

#[unsafe(no_mangle)]
pub extern "C" fn rav1g_inter_fast_mode() -> c_int {
    perf::inter_fast_mode()
}

#[unsafe(no_mangle)]
pub extern "C" fn rav1g_perf_now() -> i64 {
    perf::perf_now()
}

/// # Safety
/// As for [`perf::perf_lap`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_perf_lap(dec: *mut Av1Core, id: c_int, start_ticks: i64) -> i64 {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_lap(dec, id as usize, start_ticks) }
}

/// # Safety
/// As for [`perf::perf_add_memset`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_perf_add_memset(dec: *mut Av1Core, id: c_int, start_ticks: i64, bytes: usize) {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_add_memset(dec, id as usize, start_ticks, bytes) };
}

/// # Safety
/// As for [`perf::perf_begin_command_buffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_perf_begin_command_buffer(dec: *mut Av1Core, td: *mut av1_frame_thread_data) {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_begin_command_buffer(dec, td) };
}

/// # Safety
/// As for [`perf::perf_resolve`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_perf_resolve(dec: *mut Av1Core, td: *const av1_frame_thread_data) {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_resolve(dec, td) };
}

/// # Safety
/// As for [`perf::perf_collect`]; `rec` is valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_perf_collect(
    dec: *mut Av1Core,
    td: *mut av1_frame_thread_data,
    wait_start: i64,
    rec: *mut sys::av1_perf_frame,
) {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_collect(dec, td, wait_start, &mut *rec) };
}

/// The record of a frame goes to the callback of the timing run, when there is one.
///
/// # Safety
/// `rec` is the record of a frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_perf_deliver(rec: *const sys::av1_perf_frame) {
    if let Some((callback, context)) = perf::perf_callback() {
        // SAFETY: the callback of the application with its context and a record that is valid for the call.
        unsafe { callback(context, rec) };
    }
}

/// # Safety
/// As for [`stage_dump::stage_dump_begin`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_stage_dump_begin(td: *mut av1_frame_thread_data, is_sef: c_int) {
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_begin(&mut *td, is_sef) };
}

/// # Safety
/// As for [`stage_dump::stage_dump_frame_info`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_stage_dump_frame_info(dec: *mut Av1Core, cm: *const AV1_COMMON) {
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_frame_info(dec, cm) };
}

/// # Safety
/// As for [`stage_dump::stage_dump_deliver`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_stage_dump_deliver(td: *mut av1_frame_thread_data) {
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_deliver(&mut *td) };
}

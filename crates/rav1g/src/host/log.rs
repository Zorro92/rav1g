// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Log lines for the application (`av1_set_log_callback` of `dx/av1_perf.h`) and the reason a decoder
//! creation failed. From `libav1/dx/av1_memory.cpp`.

use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::Win32::System::Diagnostics::Debug::OutputDebugStringA;
use windows::core::{PCSTR, s};

use crate::sys;

/// The longest log line with its end, as `av1_log` of the C++ had it.
const LINE_SIZE: usize = 768;
/// The same for the reason of a failed creation.
const CREATE_ERROR_SIZE: usize = 512;

static LOG_CB: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static LOG_PRIV: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// The reason the last decoder creation failed, a C string. Written by the thread that creates a decoder
/// and read by the application after the creation returned, as in C++: no lock.
struct CreateError(UnsafeCell<[u8; CREATE_ERROR_SIZE]>);
// SAFETY: see the type: one writer, the creating thread, and readers after it is done.
unsafe impl Sync for CreateError {}
static CREATE_ERROR: CreateError = CreateError(UnsafeCell::new([0; CREATE_ERROR_SIZE]));

/// `av1_set_log_callback` (`dx/av1_perf.h`).
///
/// # Safety
/// `cb` is null or a function that may be called from any thread of the decoder with `private`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_set_log_callback(cb: sys::av1_log_cb_fn_t, private: *mut c_void) {
    LOG_PRIV.store(private, Ordering::Relaxed);
    LOG_CB.store(cb.map_or(std::ptr::null_mut(), |cb| cb as *mut c_void), Ordering::Relaxed);
}

/// `av1_last_create_error` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_last_create_error() -> *const c_char {
    CREATE_ERROR.0.get().cast()
}

/// One line to the callback, or to the debugger output without one.
///
/// # Safety
/// `line` is a C string.
unsafe fn log_line(is_error: c_int, line: *const c_char) {
    let cb = LOG_CB.load(Ordering::Relaxed);
    if cb.is_null() {
        // SAFETY: `line` is a C string.
        unsafe {
            OutputDebugStringA(PCSTR(line.cast()));
            OutputDebugStringA(s!("\n"));
        }
    } else {
        // SAFETY: the pointer was stored from a callback of this type by av1_set_log_callback, whose
        // caller says it may be called.
        unsafe {
            let cb: unsafe extern "C" fn(*mut c_void, c_int, *const c_char) = std::mem::transmute(cb);
            cb(LOG_PRIV.load(Ordering::Relaxed), is_error, line);
        }
    }
}

/// `text` as a C string of at most `N - 1` bytes, cut like `vsnprintf` cuts.
fn c_line<const N: usize>(text: &str) -> [u8; N] {
    let mut line = [0u8; N];
    let length = text.len().min(N - 1);
    line[..length].copy_from_slice(&text.as_bytes()[..length]);
    line
}

/// A log line of the decoder (`av1_log` of the C++). The lines about memory start with "MEMORY ".
pub(crate) fn log(is_error: bool, text: &str) {
    let line = c_line::<LINE_SIZE>(text);
    // SAFETY: `line` ends with a 0.
    unsafe { log_line(c_int::from(is_error), line.as_ptr().cast()) };
}

/// A log line that is also kept as the reason the decoder creation failed (`av1_log_create_error` of the
/// C++). Only the first reason of a creation is kept: it is the allocation that failed, later ones are
/// consequences.
pub(crate) fn log_create_error(text: &str) {
    let line = c_line::<CREATE_ERROR_SIZE>(text);
    // SAFETY: `line` ends with a 0.
    unsafe { rav1g_log_create_error_line(line.as_ptr().cast()) };
}

/// `av1_log_error` (`dx/av1_perf.h`): an error line of the entropy decoder.
///
/// # Safety
/// `line` is a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_log_error(line: *const c_char) {
    // SAFETY: the caller's contract.
    unsafe { log_line(1, line) };
}

/// A line the C++ host formatted (`av1_log` of the glue).
///
/// # Safety
/// `line` is a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_log_line(is_error: c_int, line: *const c_char) {
    // SAFETY: the caller's contract.
    unsafe { log_line(is_error, line) };
}

/// The same for `av1_log_create_error`.
///
/// # Safety
/// `line` is a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_log_create_error_line(line: *const c_char) {
    // SAFETY: the reason is written by the creating thread only (see CreateError); `line` is a C string,
    // copied up to its end or to the size of the reason, which keeps its last byte 0.
    unsafe {
        let reason = &mut *CREATE_ERROR.0.get();
        if reason[0] == 0 {
            let bytes = std::ffi::CStr::from_ptr(line).to_bytes();
            let length = bytes.len().min(CREATE_ERROR_SIZE - 1);
            reason[..length].copy_from_slice(&bytes[..length]);
            reason[length] = 0;
        }
        log_line(1, line);
    }
}

/// `av1_clear_create_error` of the C++: a decoder creation starts.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_clear_create_error() {
    // SAFETY: written by the creating thread only (see CreateError).
    unsafe { (*CREATE_ERROR.0.get())[0] = 0 };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_cut_like_vsnprintf() {
        let line = c_line::<8>("0123456789");
        assert_eq!(&line, b"0123456\0");
        let line = c_line::<8>("abc");
        assert_eq!(&line, b"abc\0\0\0\0\0");
    }
}

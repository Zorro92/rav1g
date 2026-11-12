// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The C++ this program shares with the test player and warp_decoder (shim/shim.h): stream readers,
//! timing summary, stage dump files, Shader Model 6 test, process settings.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use rav1g::sys;

type LineFn = unsafe extern "C" fn(ctx: *mut c_void, line: *const c_char);

#[repr(C)]
struct RdReader {
    _opaque: [u8; 0],
}
#[repr(C)]
struct RdPerf {
    _opaque: [u8; 0],
}
#[repr(C)]
struct RdDumps {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn rd_process_init();
    fn rd_set_log_verbose(verbose: c_int);

    fn rd_reader_open(path: *const c_char) -> *mut RdReader;
    fn rd_reader_next(reader: *mut RdReader, data: *mut *const u8, size: *mut usize) -> c_int;
    fn rd_reader_close(reader: *mut RdReader);

    fn rd_perf_new() -> *mut RdPerf;
    fn rd_perf_free(perf: *mut RdPerf);
    fn rd_perf_add_frame(perf: *mut RdPerf, frame: *const sys::av1_perf_frame);
    fn rd_perf_add_output(perf: *mut RdPerf, ms: f64);
    fn rd_perf_add_decode_call(perf: *mut RdPerf, ms: f64);
    fn rd_perf_lines(perf: *mut RdPerf, progress: c_int, line: LineFn, ctx: *mut c_void);

    fn rd_dump_parse_frames(text: *const c_char, frames: *mut c_int, capacity: c_int) -> c_int;
    fn rd_dumps_new() -> *mut RdDumps;
    fn rd_dumps_free(dumps: *mut RdDumps);
    fn rd_dumps_start(dumps: *mut RdDumps, dir: *const c_char, name: *const c_char);
    fn rd_dumps_next_sequence(dumps: *mut RdDumps);
    fn rd_dumps_callback(dumps: *mut c_void, dump: *const sys::av1_stage_dump);
    fn rd_dumps_take_lines(dumps: *mut RdDumps, line: LineFn, ctx: *mut c_void);
    fn rd_dumps_count(dumps: *mut RdDumps) -> c_int;
    fn rd_dumps_bytes(dumps: *mut RdDumps) -> u64;
    fn rd_dumps_error(dumps: *mut RdDumps, line: LineFn, ctx: *mut c_void) -> c_int;

    fn rd_sm6_log_caps(device: *mut c_void, line: LineFn, ctx: *mut c_void);
    fn rd_sm6_run_tests(device: *mut c_void, queue: *mut c_void, line: LineFn, ctx: *mut c_void) -> c_int;
}

fn c_string(text: &str) -> CString {
    // A NUL inside an argument cannot come from a command line; cut there if it does.
    let end = text.find('\0').unwrap_or(text.len());
    CString::new(&text[..end]).unwrap_or_default()
}

/// A line callback that collects into the `Vec<String>` behind `ctx`.
unsafe extern "C" fn collect_line(ctx: *mut c_void, line: *const c_char) {
    if ctx.is_null() || line.is_null() {
        return;
    }
    // SAFETY: ctx is the &mut Vec<String> of the caller below, line a NUL-terminated string.
    unsafe {
        let lines = &mut *ctx.cast::<Vec<String>>();
        lines.push(CStr::from_ptr(line).to_string_lossy().into_owned());
    }
}

/// A line callback that prints the line.
unsafe extern "C" fn say_line(_ctx: *mut c_void, line: *const c_char) {
    if line.is_null() {
        return;
    }
    // SAFETY: line is a NUL-terminated string.
    crate::say_text(&unsafe { CStr::from_ptr(line) }.to_string_lossy());
}

/// No error dialogs, unbuffered C stdout, and the crash line of warp_decoder.
pub fn process_init() {
    // SAFETY: no arguments; called once at the start of main.
    unsafe { rd_process_init() }
}

pub fn set_log_verbose(verbose: bool) {
    // SAFETY: sets a global of testing/common/log before any other thread exists.
    unsafe { rd_set_log_verbose(verbose as c_int) }
}

/// The stream reader of testing/common: IVF, WebM/Matroska, OBU, MP4.
pub struct Reader {
    reader: *mut RdReader,
    data: *const u8,
    size: usize,
}

impl Reader {
    /// `None` when the file cannot be opened or its container is none of the four.
    pub fn open(path: &str) -> Option<Reader> {
        let path = c_string(path);
        // SAFETY: path is a NUL-terminated string.
        let reader = unsafe { rd_reader_open(path.as_ptr()) };
        if reader.is_null() { None } else { Some(Reader { reader, data: std::ptr::null(), size: 0 }) }
    }

    /// Reads the next temporal unit. False at the end of the stream.
    pub fn next(&mut self) -> bool {
        // SAFETY: the reader is valid; data and size are written by the call.
        let have = unsafe { rd_reader_next(self.reader, &mut self.data, &mut self.size) } != 0;
        if !have {
            self.data = std::ptr::null();
            self.size = 0;
        }
        have
    }

    /// The temporal unit of the last `next` that returned true.
    pub fn data(&self) -> &[u8] {
        if self.data.is_null() {
            &[]
        } else {
            // SAFETY: the reader keeps the unit until the next call of next, which takes &mut self.
            unsafe { std::slice::from_raw_parts(self.data, self.size) }
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: opened by rd_reader_open, closed once.
        unsafe { rd_reader_close(self.reader) }
    }
}

/// -gputime: the records of a task (testing/player_common/perf_stats.h), behind a lock of their own.
pub struct Perf(*mut RdPerf);

// SAFETY: every function of rd_perf takes its lock.
unsafe impl Send for Perf {}
unsafe impl Sync for Perf {}

impl Perf {
    pub fn new() -> Perf {
        // SAFETY: no arguments. A NULL (no memory) is accepted by every function below.
        Perf(unsafe { rd_perf_new() })
    }
    /// One record per command list of the decoder.
    ///
    /// # Safety
    /// `frame` is the record the decoder passes to its perf callback, during that callback.
    pub unsafe fn add_frame(&self, frame: *const sys::av1_perf_frame) {
        // SAFETY: the caller's contract.
        unsafe { rd_perf_add_frame(self.0, frame) }
    }
    pub fn add_output(&self, ms: f64) {
        // SAFETY: the handle is valid until drop.
        unsafe { rd_perf_add_output(self.0, ms) }
    }
    pub fn add_decode_call(&self, ms: f64) {
        // SAFETY: the handle is valid until drop.
        unsafe { rd_perf_add_decode_call(self.0, ms) }
    }
    /// The summary lines of the player, and with `progress` its per-frame lines.
    pub fn lines(&self, progress: bool) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        // SAFETY: collect_line gets the Vec it expects, for the duration of the call.
        unsafe { rd_perf_lines(self.0, progress as c_int, collect_line, (&raw mut lines).cast()) };
        lines
    }
}

impl Drop for Perf {
    fn drop(&mut self) {
        // SAFETY: created by rd_perf_new, freed once.
        unsafe { rd_perf_free(self.0) }
    }
}

/// "0,3,10-12" -> 0 3 10 11 12. `None` when the text is not such a list, or has more than 256 frames.
pub fn parse_dump_frames(text: &str) -> Option<Vec<i32>> {
    let text = c_string(text);
    let mut frames = [0 as c_int; 256];
    // SAFETY: frames has room for the capacity given.
    let count = unsafe { rd_dump_parse_frames(text.as_ptr(), frames.as_mut_ptr(), frames.len() as c_int) };
    if count <= 0 { None } else { Some(frames[..count as usize].to_vec()) }
}

/// -dumpstages: the files of testing/common/stage_dump.h.
pub struct Dumps(*mut RdDumps);

impl Dumps {
    pub fn new() -> Dumps {
        // SAFETY: no arguments.
        let dumps = unsafe { rd_dumps_new() };
        assert!(!dumps.is_null(), "no memory for the stage dump writer");
        Dumps(dumps)
    }
    /// Creates `dir` when it is not there and starts the files of the stream `name`.
    pub fn start(&self, dir: &str, name: &str) {
        let (dir, name) = (c_string(dir), c_string(name));
        // SAFETY: valid handle and strings.
        unsafe { rd_dumps_start(self.0, dir.as_ptr(), name.as_ptr()) }
    }
    pub fn next_sequence(&self) {
        // SAFETY: valid handle.
        unsafe { rd_dumps_next_sequence(self.0) }
    }
    /// The callback and its context for `av1_set_stage_dump`. The writer must outlive the decoder's
    /// completion thread.
    pub fn callback(&self) -> (sys::av1_stage_dump_cb_fn_t, *mut c_void) {
        (Some(rd_dumps_callback), self.0.cast())
    }
    /// After the drain: one line per dump.
    pub fn take_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        // SAFETY: collect_line gets the Vec it expects.
        unsafe { rd_dumps_take_lines(self.0, collect_line, (&raw mut lines).cast()) };
        lines
    }
    pub fn count(&self) -> i32 {
        // SAFETY: valid handle.
        unsafe { rd_dumps_count(self.0) }
    }
    pub fn bytes(&self) -> u64 {
        // SAFETY: valid handle.
        unsafe { rd_dumps_bytes(self.0) }
    }
    /// The first write error, empty when there was none.
    pub fn error(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        // SAFETY: collect_line gets the Vec it expects.
        unsafe { rd_dumps_error(self.0, collect_line, (&raw mut lines).cast()) };
        lines.pop().unwrap_or_default()
    }
}

impl Drop for Dumps {
    fn drop(&mut self) {
        // SAFETY: created by rd_dumps_new, freed once, after the decoder that called its callback.
        unsafe { rd_dumps_free(self.0) }
    }
}

/// Prints what the device reports about itself ("CAPS ..." lines).
///
/// # Safety
/// `device` is a valid `ID3D12Device*`.
pub unsafe fn sm6_log_caps(device: *mut c_void) {
    // SAFETY: the caller's contract.
    unsafe { rd_sm6_log_caps(device, say_line, std::ptr::null_mut()) }
}

/// Runs the Shader Model 6 test shaders ("SM6TEST ..." lines). True when the test itself broke.
///
/// # Safety
/// `device` and `queue` are a valid `ID3D12Device*` and `ID3D12CommandQueue*`.
pub unsafe fn sm6_run_tests(device: *mut c_void, queue: *mut c_void) -> bool {
    // SAFETY: the caller's contract.
    unsafe { rd_sm6_run_tests(device, queue, say_line, std::ptr::null_mut()) != 0 }
}

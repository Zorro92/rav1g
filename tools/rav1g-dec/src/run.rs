// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! One task, as the player's `AV1Decoder::DecodeOne` and warp_decoder's `RunTask` do it: the decoder
//! configuration, the output buffer callbacks, the hash of every output frame, the result line.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use rav1g::{Config, Decoder, ErrorCode, PipelineCache, StreamInfo, switches, sys};

use crate::args::{self, EXIT_ERROR, EXIT_MISMATCH, EXIT_PASS, Task};
use crate::device::{Device, hex32};
use crate::md5::Md5;
use crate::pool::OutputPool;
use crate::say;
use crate::shim;

/// What a task collects, from the decoder's threads.
struct State {
    /// One per output frame, in output order.
    md5: Vec<String>,
    /// The reference, when there is one.
    reference: Vec<String>,
    have_ref: bool,
    matched: i32,
    first_bad: i32,
    mismatch_lines: i32,
    wrong_saved: i32,
    /// Of the first output frame.
    width: u32,
    height: u32,
    bit_depth: u32,
    size_changes: i32,
    pool_error: String,
    out_file: Option<BufWriter<File>>,
    io_error: String,
}

pub struct Run {
    task: Task,
    pool: OutputPool,
    state: Mutex<State>,
    // What the watchdog reads without the lock of the state (the task may hold it): kept in step under it.
    frames: AtomicI32,
    matched: AtomicI32,
    first_bad: AtomicI32,
    ref_frames: AtomicI32,
    width: AtomicU32,
    height: AtomicU32,
    bit_depth: AtomicU32,
    /// -gputime: the records of the decoder's completion thread, as the player keeps them.
    perf: shim::Perf,
    perf_start: Mutex<Instant>,
}

impl Run {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The result line of a task that ran too long, from the watchdog.
    pub fn timeout_line(&self, name: &str, timeout_s: i32, elapsed_s: f64) {
        let detail = format!(
            "no result after {} s; frames out so far are counted, {} output images rented",
            timeout_s,
            self.pool.rented()
        );
        result_line(
            name,
            "timeout",
            self.frames.load(Ordering::Relaxed),
            self.matched.load(Ordering::Relaxed),
            self.first_bad.load(Ordering::Relaxed),
            self.ref_frames.load(Ordering::Relaxed),
            elapsed_s * 1000.0,
            0.0,
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
            self.bit_depth.load(Ordering::Relaxed),
            &detail,
        );
    }
}

/// The task that is running, for the watchdog: what it prints must name it.
pub struct Current {
    pub name: String,
    pub run: Arc<Run>,
    pub start: Instant,
    pub timeout_s: i32,
}

pub static CURRENT: Mutex<Option<Current>> = Mutex::new(None);

struct ClearCurrent;

impl Drop for ClearCurrent {
    fn drop(&mut self) {
        *CURRENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

#[allow(clippy::too_many_arguments)]
fn result_line(
    name: &str,
    status: &str,
    frames: i32,
    matched: i32,
    first_bad: i32,
    ref_frames: i32,
    decode_ms: f64,
    create_ms: f64,
    width: u32,
    height: u32,
    bit_depth: u32,
    detail: &str,
) {
    let clean: String = detail.chars().map(|c| if c == '"' || c == '\r' || c == '\n' { '\'' } else { c }).collect();
    say!(
        "RESULT stream={} status={} frames={} matched={} first_bad={} ref_frames={} decode_ms={:.0} create_ms={:.0} \
         size={}x{} bitdepth={} detail=\"{}\"",
        name,
        status,
        frames,
        matched,
        first_bad,
        ref_frames,
        decode_ms,
        create_ms,
        width,
        height,
        bit_depth,
        clean
    );
}

/// The visible samples of a frame, in the order they are hashed: luma rows, then U, then V; one byte a
/// sample for 8-bit, two bytes (low byte first) above that. A monochrome frame is luma only, or, with
/// -aom_md5, luma and two chroma planes of the mid grey value, which is what aomdec writes for --i420 and
/// so what AOM's .md5 files of monochrome streams hash.
///
/// # Safety
/// `img` is an image the decoder handed to the frame-ready callback: its planes are readable for the
/// visible size at their strides.
unsafe fn for_each_row(img: &sys::aom_image_t, aom_md5: bool, mut sink: impl FnMut(&[u8])) {
    let bytes: usize = if img.bit_depth == 8 { 1 } else { 2 };
    let width = img.d_w as usize;
    let width_c = (img.d_w as usize).div_ceil(2);
    let height_c = (img.d_h as usize).div_ceil(2);
    let mut row = img.planes[0].cast_const();
    for _ in 0..img.d_h {
        // SAFETY: a visible row of the luma plane.
        sink(unsafe { std::slice::from_raw_parts(row, width * bytes) });
        row = row.wrapping_offset(img.stride[0] as isize);
    }
    if img.monochrome == 0 {
        for plane in 1..3 {
            let mut row = img.planes[plane].cast_const();
            for _ in 0..height_c {
                // SAFETY: a visible row of a chroma plane.
                sink(unsafe { std::slice::from_raw_parts(row, width_c * bytes) });
                row = row.wrapping_offset(img.stride[plane] as isize);
            }
        }
    } else if aom_md5 {
        let value: u32 = 1 << (img.bit_depth - 1);
        let mut grey = vec![0u8; width_c * bytes];
        for x in 0..width_c {
            if bytes == 1 {
                grey[x] = value as u8;
            } else {
                grey[2 * x] = (value & 0xff) as u8;
                grey[2 * x + 1] = (value >> 8) as u8;
            }
        }
        for _ in 0..2 * height_c {
            sink(&grey);
        }
    }
}

/// Writes a frame as it is hashed. False on a write error.
///
/// # Safety
/// As `for_each_row`.
unsafe fn write_frame(img: &sys::aom_image_t, aom_md5: bool, file: &mut impl Write) -> bool {
    let mut ok = true;
    // SAFETY: the caller's contract.
    unsafe {
        for_each_row(img, aom_md5, |row| {
            if ok && file.write_all(row).is_err() {
                ok = false;
            }
        })
    };
    ok
}

unsafe extern "C" fn get_buffer(
    context: *mut c_void,
    buf_size: usize,
    _width: u16,
    _height: u16,
    _fb_type: sys::frame_buffer_type,
    fb: *mut sys::av1_decoded_frame_buffer_t,
) -> c_int {
    if context.is_null() || fb.is_null() {
        return -1;
    }
    // SAFETY: the context is the Run of the task, alive until its decoder is destroyed; fb is the
    // decoder's.
    let (run, fb) = unsafe { (&*context.cast::<Run>(), &mut *fb) };
    match run.pool.rent(buf_size) {
        Ok(buffer) => {
            fb.dx12_buffer = buffer.resource.cast();
            fb.buffer_host_ptr = buffer.mem.cast();
            fb.buffer_size = buffer.size as _;
            fb.dx12_texture = [std::ptr::null_mut(); 3];
            fb.priv_ = buffer.token;
            0
        }
        Err(why) => {
            let mut state = run.state();
            if state.pool_error.is_empty() {
                state.pool_error = why;
            }
            -1
        }
    }
}

unsafe extern "C" fn release_buffer(context: *mut c_void, buffer_priv: *mut c_void) -> c_int {
    if context.is_null() {
        return -1;
    }
    // SAFETY: as in get_buffer.
    let run = unsafe { &*context.cast::<Run>() };
    run.pool.give_back(buffer_priv);
    0
}

/// -gputime: one record per command list of the decoder, from its GPU completion thread.
unsafe extern "C" fn perf_frame(context: *mut c_void, frame: *const sys::av1_perf_frame) {
    if context.is_null() || frame.is_null() {
        return;
    }
    // SAFETY: as in get_buffer; the record is valid during the callback.
    unsafe { (*context.cast::<Run>()).perf.add_frame(frame) };
}

/// The decoder's frame-ready callback: a frame whose GPU work is complete, or NULL after the last one.
unsafe extern "C" fn notify_frame_ready(context: *mut c_void, image: *mut c_void) -> c_int {
    if context.is_null() {
        return -1;
    }
    // SAFETY: as in get_buffer.
    let run = unsafe { &*context.cast::<Run>() };
    if image.is_null() {
        return 0;
    }
    // SAFETY: the decoder hands over an aom_image_t whose planes are in the output buffer of the frame.
    let img = unsafe { &*image.cast::<sys::aom_image_t>() };
    let task = &run.task;
    if task.gputime {
        let start = *run.perf_start.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        run.perf.add_output(ms_since(start));
    }
    let mut ctx = Md5::new();
    // SAFETY: img is the decoder's image.
    unsafe { for_each_row(img, task.aom_md5, |row| ctx.update(row)) };
    let md5 = ctx.finish_hex();
    {
        let mut state = run.state();
        let n = state.md5.len() as i32;
        if n == 0 {
            state.width = img.d_w;
            state.height = img.d_h;
            state.bit_depth = img.bit_depth;
            run.width.store(img.d_w, Ordering::Relaxed);
            run.height.store(img.d_h, Ordering::Relaxed);
            run.bit_depth.store(img.bit_depth, Ordering::Relaxed);
        } else if img.d_w != state.width || img.d_h != state.height {
            // Counted against the size of the first frame; the md5 of a frame is always over its own size.
            state.size_changes += 1;
        }
        let mut wrong = false;
        if state.have_ref {
            let expected = state.reference.get(n as usize).cloned();
            if expected.as_deref() == Some(md5.as_str()) {
                state.matched += 1;
                run.matched.store(state.matched, Ordering::Relaxed);
            } else {
                wrong = true;
                if state.first_bad < 0 {
                    state.first_bad = n;
                    run.first_bad.store(n, Ordering::Relaxed);
                }
                if state.mismatch_lines < 16 {
                    say!(
                        "MISMATCH frame {} ({}x{} {}-bit{}): expected {}, got {}",
                        n,
                        img.d_w,
                        img.d_h,
                        img.bit_depth,
                        if img.monochrome != 0 { " monochrome" } else { "" },
                        expected.as_deref().unwrap_or("no frame (the reference has ended)"),
                        md5
                    );
                } else if state.mismatch_lines == 16 {
                    say!("MISMATCH ... further frames are not listed");
                }
                state.mismatch_lines += 1;
            }
        }
        state.md5.push(md5);
        run.frames.store(n + 1, Ordering::Relaxed);
        if state.io_error.is_empty()
            && let Some(file) = state.out_file.as_mut()
            // SAFETY: img is the decoder's image.
            && !unsafe { write_frame(img, task.aom_md5, file) }
        {
            state.io_error = "write error".to_string();
        }
        if wrong && !task.save_wrong_yuv.is_empty() && (task.wrong_limit < 0 || state.wrong_saved < task.wrong_limit) {
            state.wrong_saved += 1;
            let path = format!("{}/{}_{}.yuv", task.save_wrong_yuv, task.name, n);
            let written = File::create(&path).is_ok_and(|file| {
                let mut file = BufWriter::new(file);
                // SAFETY: img is the decoder's image.
                unsafe { write_frame(img, task.aom_md5, &mut file) && file.flush().is_ok() }
            });
            if !written {
                say!("cannot write {}", path);
            }
        }
    }
    run.pool.give_back(img.fb2_priv);
    0
}

/// Reads a reference md5 file: the first 32 characters of every line that starts with 32 hex digits.
fn read_reference(path: &str) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    let mut out = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.len() < 32 || !line[..32].iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        out.push(String::from_utf8_lossy(&line[..32]).to_ascii_lowercase());
    }
    Some(out)
}

/// One line with the memory of the process and of the machine: the tasks of a list run in one process, and
/// a D3D12 device that runs out of memory is removed.
fn memory_line(when: &str) {
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut pmc =
        PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    let mut status = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: both structures are valid and carry their size.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut pmc).cast::<PROCESS_MEMORY_COUNTERS>(),
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
        .as_bool()
            && GlobalMemoryStatusEx(&mut status).is_ok()
    };
    if !ok {
        return;
    }
    let mib = 1024.0 * 1024.0;
    say!(
        "PROCESS {}: private {:.0} MiB (peak page file use {:.0} MiB), working set {:.0} MiB; machine: {:.0} MiB of \
         {:.0} MiB physical memory free, {:.0} MiB of {:.0} MiB commit limit free",
        when,
        pmc.PrivateUsage as f64 / mib,
        pmc.PeakPagefileUsage as f64 / mib,
        pmc.WorkingSetSize as f64 / mib,
        status.ullAvailPhys as f64 / mib,
        status.ullTotalPhys as f64 / mib,
        status.ullAvailPageFile as f64 / mib,
        status.ullTotalPageFile as f64 / mib
    );
}

/// The size the decoder is created for: the stream's largest frame size, rounded up to 128 as the decoder
/// does, and 8-bit buffers for an 8-bit stream; 4096x2176 10-bit with -nofitsize or for a larger stream.
/// A decoder that follows another one of the task (a new sequence that did not fit) is at least as large
/// as that one, so a stream that goes back and forth between two sizes gets one decoder.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct DecoderSize {
    width: u32,
    height: u32,
    bit_depth: u32,
}

impl DecoderSize {
    fn grow_for(&mut self, nofitsize: bool, info: &StreamInfo) {
        let mut width = args::DECODER_WIDTH;
        let mut height = args::DECODER_HEIGHT;
        let mut bit_depth = args::DECODER_BITDEPTH;
        if !nofitsize {
            let fit_width = info.width.wrapping_add(127) & !127;
            let fit_height = info.height.wrapping_add(127) & !127;
            if info.width != 0
                && info.height != 0
                && fit_width <= args::DECODER_WIDTH
                && fit_height <= args::DECODER_HEIGHT
            {
                width = fit_width;
                height = fit_height;
            }
            if info.bit_depth == 8 {
                bit_depth = 8;
            }
        }
        self.width = self.width.max(width);
        self.height = self.height.max(height);
        self.bit_depth = self.bit_depth.max(bit_depth);
    }
}

/// Creates the decoder for `size`; `info` is the sequence it is for. The error says what failed.
fn create_decoder(
    run: &Arc<Run>,
    dev: &Device,
    psos: &PipelineCache,
    size: DecoderSize,
    info: &StreamInfo,
    create_ms: &mut f64,
) -> Result<Decoder, String> {
    let task = &run.task;
    let create_start = Instant::now();
    let config = Config {
        width: size.width,
        height: size.height,
        bit_depth: size.bit_depth,
        threads: task.threads as u32,
        frame_slots: task.slots,
        intra_v2: task.intrav2,
        inter_fast: task.interfast != 0,
        low_memory: if task.lowmem_parts != 0 { task.lowmem_parts } else { sys::AV1_LOW_MEMORY_NONE as i32 },
        out_buffers: sys::av1_out_buffers_cb_t {
            get_out_buffer_cb: Some(get_buffer),
            release_out_buffer_cb: Some(release_buffer),
            notify_frame_ready_cb: Some(notify_frame_ready),
            out_buffers_priv: Arc::as_ptr(run).cast_mut().cast(),
        },
        device: dev.device_ptr(),
        command_queue: dev.queue_ptr(),
        pipeline_cache: psos.as_ptr(),
    };
    let memory = config.memory_requirements();
    say!(
        "TASK {}: stream {}x{} {}-bit, decoder {}x{} {}-bit ({}), slots {}, low memory parts {}, CDEF {}, intra {}{}, \
         clears {}, threads {}, output pool {}; device buffers {:.1} MiB, host pool {:.1} MiB",
        task.name,
        info.width,
        info.height,
        info.bit_depth,
        size.width,
        size.height,
        size.bit_depth,
        if task.nofitsize { "-nofitsize" } else { "fit to the stream" },
        task.slots,
        task.lowmem_parts,
        if task.cdefv1 { "cdef_filter (-cdefv1)" } else { "cdef_filter_v2" },
        if task.intrav2 { "intra_v2" } else { "intra_main + intra_filter" },
        if task.intraopt { " (intra_main_opt)" } else { "" },
        if task.asyncclear { "-asyncclear" } else { "decode thread" },
        task.threads,
        task.queue_size,
        memory.device_size as f64 / (1024.0 * 1024.0),
        memory.host_size as f64 / (1024.0 * 1024.0)
    );
    // SAFETY: the device, the queue and the pipeline states outlive the decoder (they are the program's);
    // the Run behind the callbacks is kept by run_task until the decoder is dropped.
    let mut decoder = unsafe { Decoder::new(&config) }?;
    // The layers of a stream that has them: one frame per temporal unit unless -alllayers, operating point
    // 0 unless -oppoint.
    if let Err(error) =
        decoder.set_output_all_layers(task.all_layers).and_then(|()| decoder.set_operating_point(task.oppoint))
    {
        return Err(format!("a layer control failed: {}", error.message));
    }
    *create_ms += ms_since(create_start);
    Ok(decoder)
}

/// One task. Returns the exit code of the task.
pub fn run_task(task: &Task, dev: &Device, psos: &PipelineCache) -> i32 {
    say!("BEGIN stream={}", task.name);
    memory_line("before the task");
    let run = Arc::new(Run {
        task: task.clone(),
        pool: OutputPool::new(&dev.device, task.queue_size),
        state: Mutex::new(State {
            md5: Vec::new(),
            reference: Vec::new(),
            have_ref: false,
            matched: 0,
            first_bad: -1,
            mismatch_lines: 0,
            wrong_saved: 0,
            width: 0,
            height: 0,
            bit_depth: 0,
            size_changes: 0,
            pool_error: String::new(),
            out_file: None,
            io_error: String::new(),
        }),
        frames: AtomicI32::new(0),
        matched: AtomicI32::new(0),
        first_bad: AtomicI32::new(-1),
        ref_frames: AtomicI32::new(0),
        width: AtomicU32::new(0),
        height: AtomicU32::new(0),
        bit_depth: AtomicU32::new(0),
        perf: shim::Perf::new(),
        perf_start: Mutex::new(Instant::now()),
    });
    *CURRENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(Current { name: task.name.clone(), run: run.clone(), start: Instant::now(), timeout_s: task.timeout_s });
    let _clear_current = ClearCurrent;

    let mut create_ms = 0.0f64;
    let mut decode_ms = 0.0f64;
    let fail = |detail: &str, decode_ms: f64, create_ms: f64| -> i32 {
        let state = run.state();
        result_line(
            &task.name,
            "error",
            state.md5.len() as i32,
            state.matched,
            state.first_bad,
            if state.have_ref { state.reference.len() as i32 } else { 0 },
            decode_ms,
            create_ms,
            state.width,
            state.height,
            state.bit_depth,
            detail,
        );
        EXIT_ERROR
    };

    if !task.check.is_empty() {
        let Some(reference) = read_reference(&task.check) else {
            return fail(&format!("cannot read the reference {}", task.check), decode_ms, create_ms);
        };
        if reference.is_empty() {
            return fail(&format!("no md5 lines in the reference {}", task.check), decode_ms, create_ms);
        }
        run.ref_frames.store(reference.len() as i32, Ordering::Relaxed);
        let mut state = run.state();
        state.reference = reference;
        state.have_ref = true;
    }

    let Some(mut reader) = shim::Reader::open(&task.input) else {
        return fail("cannot open the stream, or its container is not IVF, WebM, OBU or MP4", decode_ms, create_ms);
    };

    // The first key frame is looked for before the decoder exists: its sequence header says how large the
    // frames of the stream are and how many bits a sample has.
    let mut skipped = 0;
    let si = loop {
        if !reader.next() {
            return fail("end of the stream before a key frame", decode_ms, create_ms);
        }
        match rav1g::peek_stream_info(reader.data()) {
            Ok(si) if si.is_key_frame => break si,
            Ok(_) => skipped += 1,
            Err(_) => {
                return fail(
                    &format!("aom_codec_peek_stream_info failed on temporal unit {skipped}"),
                    decode_ms,
                    create_ms,
                );
            }
        }
    };
    let mut size = DecoderSize::default();
    size.grow_for(task.nofitsize, &si);

    if !task.out_yuv.is_empty() {
        match File::create(&task.out_yuv) {
            Ok(file) => run.state().out_file = Some(BufWriter::new(file)),
            Err(_) => return fail(&format!("cannot write {}", task.out_yuv), decode_ms, create_ms),
        }
    }

    // -dumpstages: the writer outlives the decoder's completion thread, which the drain below joins.
    let stage_dumps = shim::Dumps::new();
    let mut decoder = match create_decoder(&run, dev, psos, size, &si, &mut create_ms) {
        Ok(decoder) => Some(decoder),
        Err(create_error) => return fail(&create_error, decode_ms, create_ms),
    };

    // Process-wide switches, set while no frame is in flight, as the player does at the start of a task.
    // -gputime, -gputimedetail, -gputimeisolate: the timing run of the player.
    *run.perf_start.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    let context: *mut c_void = Arc::as_ptr(&run).cast_mut().cast();
    // SAFETY: no frame is in flight: the decoder has decoded nothing yet. The Run outlives the decoder, and
    // the callbacks are taken out again below, after the drain.
    unsafe {
        switches::set_perf_callback(if task.gputime { Some(perf_frame) } else { None }, context);
        switches::set_perf_detail(if task.gputime { task.gputime_detail } else { 0 });
    }
    if task.gputime {
        say!(
            "GPUTIME {}: timing run{}; on WARP the times are those of a CPU rasterizer",
            task.name,
            match task.gputime_detail {
                2 => ", inter breakdown with a UAV barrier and a timestamp behind every dispatch (-gputimeisolate)",
                1 => ", inter breakdown (-gputimedetail)",
                _ => "",
            }
        );
    }
    // SAFETY: as above.
    unsafe {
        switches::set_cdef_v2(!task.cdefv1);
        switches::set_intra_v2(task.intrav2);
        switches::set_intra_opt(task.intraopt);
        switches::set_async_clear(task.asyncclear);
        switches::set_lf_h_split(task.lfhsplit);
        switches::set_intra_edge_fix(task.intraedgefix);
        switches::set_inter_fast(task.interfast);
    }
    if task.interfast & sys::AV1_INTER_FAST_ON != 0 {
        say!(
            "INTERFAST {}: inter blocks by motion vector class, mode {} (one reference: {}; two: {}); frames with a \
             scaled reference keep the original lists and shaders",
            task.name,
            task.interfast & 7,
            if task.interfast & sys::AV1_INTER_FAST_SINGLE_ORIGINAL != 0 {
                "inter_main on every list"
            } else {
                "inter_fast"
            },
            if task.interfast & sys::AV1_INTER_FAST_COMPOUND_ORIGINAL != 0 {
                "inter_compound on every list"
            } else {
                "inter_compound_fast"
            }
        );
    }
    if task.lfhsplit != 0 || task.intraedgefix {
        say!(
            "EXPERIMENTS {}:{}{}",
            task.name,
            match task.lfhsplit {
                1 => " -lfhsplit 1 (horizontal loop filter: even rows of blocks, then odd)",
                2 => " -lfhsplit 2 (horizontal loop filter: row of blocks by row)",
                _ => "",
            },
            if task.intraedgefix { " -intraedgefix (8-bit above edge filter bound)" } else { "" }
        );
    }
    if !task.dump_frames.is_empty() {
        stage_dumps.start(&task.dumpdir, &task.name);
        let (callback, dump_context) = stage_dumps.callback();
        // SAFETY: no frame is in flight; the writer lives to the end of this function.
        unsafe { switches::set_stage_dump(callback, dump_context, &task.dump_frames) };
    } else {
        // SAFETY: no frame is in flight.
        unsafe { switches::set_stage_dump(None, std::ptr::null_mut(), &[]) };
    }

    let mut decode_error = String::new();
    let mut units = 0;
    let mut limit = task.limit;
    let mut decoders = 1;
    let decode_start = Instant::now();
    let mut have = true;
    while have && limit != 0 {
        limit = limit.wrapping_sub(1);
        let Some(current) = decoder.as_mut() else {
            break;
        };
        let call_start = Instant::now();
        let mut result = current.decode(reader.data());
        if task.gputime {
            run.perf.add_decode_call(ms_since(call_start));
        }
        if let Err(error) = &result
            && error.code == ErrorCode::NEEDS_RECONFIGURE
        {
            // A new sequence whose frames are larger, or have more bits, than the decoder was created for
            // (aom/aom_codec.h). Nothing of this temporal unit was decoded. The frames of the sequence
            // before are drained, the decoder is replaced by one for the new sequence, sized from this
            // temporal unit as the first one was from the first, and the temporal unit is passed again.
            let why = error.detail.clone();
            let unit = skipped + units;
            let new_si = match rav1g::peek_stream_info(reader.data()) {
                Ok(new_si) if new_si.is_key_frame => new_si,
                _ => {
                    decode_error = format!(
                        "temporal unit {unit} needs another decoder ({why}) and does not start a sequence with a key \
                         frame"
                    );
                    break;
                }
            };
            let old_size = size;
            size.grow_for(task.nofitsize, &new_si);
            if size == old_size {
                decode_error = format!(
                    "temporal unit {unit} needs a larger decoder ({why}) than this program creates{}",
                    if task.nofitsize { " with -nofitsize" } else { "" }
                );
                break;
            }
            if let Some(mut old) = decoder.take() {
                old.drain();
            }
            // -dumpstages: the drain has handed over the dumps of the sequence before; the frame numbers of
            // the new decoder start at 0 again, and its dumps go to files of their own.
            stage_dumps.next_sequence();
            let replacement = match create_decoder(&run, dev, psos, size, &new_si, &mut create_ms) {
                Ok(replacement) => decoder.insert(replacement),
                Err(create_error) => {
                    // Nothing to drain or destroy below.
                    decode_error = format!(
                        "temporal unit {unit}, new decoder for the sequence of {}x{}: {create_error}",
                        new_si.width, new_si.height
                    );
                    break;
                }
            };
            decoders += 1;
            say!(
                "RECONFIGURE {}: temporal unit {} starts a sequence of {}x{} {}-bit frames ({}); new decoder {}x{} \
                 {}-bit",
                task.name,
                unit,
                new_si.width,
                new_si.height,
                new_si.bit_depth,
                why,
                size.width,
                size.height,
                size.bit_depth
            );
            result = replacement.decode(reader.data());
        }
        if let Err(error) = result {
            decode_error = format!("aom_codec_decode failed on temporal unit {}: {}", skipped + units, error);
            break;
        }
        units += 1;
        have = reader.next();
    }
    // Drain: joins the decoder's GPU completion thread, so every frame that was submitted has been handed
    // over.
    if let Some(decoder) = decoder.as_mut() {
        decoder.drain();
    }
    decode_ms = ms_since(decode_start);
    drop(reader);
    drop(decoder);
    // -gputime: the completion thread is joined, so the records are complete. The summary of the player,
    // and with -progress its per-frame lines.
    // SAFETY: no decoder exists, so no frame is in flight.
    unsafe {
        switches::set_stage_dump(None, std::ptr::null_mut(), &[]);
        switches::set_perf_callback(None, std::ptr::null_mut());
        switches::set_perf_detail(0);
    }
    if task.gputime {
        for line in run.perf.lines(task.progress) {
            say!("GPUTIME {}", line);
        }
    }
    let mut dump_error = String::new();
    if !task.dump_frames.is_empty() {
        for line in stage_dumps.take_lines() {
            say!("{}", line);
        }
        say!(
            "DUMPSTAGES {}: {} dumps, {:.1} MiB, of frames {} in {}",
            task.name,
            stage_dumps.count(),
            stage_dumps.bytes() as f64 / (1024.0 * 1024.0),
            task.dumpstages,
            task.dumpdir
        );
        dump_error = stage_dumps.error();
    }

    // SAFETY: a plain call on a valid device.
    let removed = unsafe { dev.device.GetDeviceRemovedReason() };
    memory_line("after the task");
    let mut state = run.state();
    if let Some(mut file) = state.out_file.take()
        && file.flush().is_err()
    {
        state.io_error = "write error".to_string();
    }
    let frames = state.md5.len() as i32;
    if !task.save_md5.is_empty() {
        // The player's format: "<md5>   <n>" and a line end of CR LF.
        let mut text = String::with_capacity(state.md5.len() * 44);
        for (i, md5) in state.md5.iter().enumerate() {
            text.push_str(&format!("{md5}   {i}\r\n"));
        }
        if std::fs::write(&task.save_md5, text).is_err() && state.io_error.is_empty() {
            state.io_error = format!("cannot write {}", task.save_md5);
        }
    }
    if !dump_error.is_empty() && state.io_error.is_empty() {
        state.io_error = dump_error;
    }
    if !state.pool_error.is_empty() && !decode_error.is_empty() {
        decode_error = format!("{decode_error} ({})", state.pool_error);
    }
    if let Err(error) = removed {
        if !decode_error.is_empty() {
            decode_error.push_str("; ");
        }
        decode_error.push_str(&format!("the D3D12 device was removed, reason {}", hex32(error.code())));
    }
    let ref_frames = if state.have_ref { state.reference.len() as i32 } else { 0 };
    let sizes = format!(
        "{} temporal units in, {} frames out{}{}",
        units,
        frames,
        if state.size_changes != 0 { ", frame size changes within the stream" } else { "" },
        if decoders > 1 { ", a new decoder for a larger sequence" } else { "" }
    );
    let (width, height, bit_depth) = (state.width, state.height, state.bit_depth);
    if !decode_error.is_empty() {
        result_line(
            &task.name,
            "error",
            frames,
            state.matched,
            state.first_bad,
            ref_frames,
            decode_ms,
            create_ms,
            width,
            height,
            bit_depth,
            &format!("{decode_error}; {sizes}"),
        );
        return EXIT_ERROR;
    }
    if !state.io_error.is_empty() {
        result_line(
            &task.name,
            "error",
            frames,
            state.matched,
            state.first_bad,
            ref_frames,
            decode_ms,
            create_ms,
            width,
            height,
            bit_depth,
            &state.io_error,
        );
        return EXIT_ERROR;
    }
    if !state.have_ref {
        result_line(&task.name, "nocheck", frames, 0, -1, 0, decode_ms, create_ms, width, height, bit_depth, &sizes);
        return EXIT_PASS;
    }
    // The frame count is part of the comparison. A task with -limit decodes a part of the stream, so its
    // reference may go on; a stream that is decoded whole must give exactly the frames of the reference.
    let mut detail = sizes;
    let mut count_ok = frames <= ref_frames && frames > 0;
    if task.limit < 0 && frames != ref_frames {
        count_ok = false;
    }
    if !count_ok {
        detail.push_str(&format!("; the reference has {ref_frames} frames"));
        if state.first_bad < 0 {
            state.first_bad = frames.min(ref_frames);
        }
    }
    let pass = count_ok && state.matched == frames;
    result_line(
        &task.name,
        if pass { "pass" } else { "mismatch" },
        frames,
        state.matched,
        state.first_bad,
        ref_frames,
        decode_ms,
        create_ms,
        width,
        height,
        bit_depth,
        &detail,
    );
    if pass { EXIT_PASS } else { EXIT_MISMATCH }
}

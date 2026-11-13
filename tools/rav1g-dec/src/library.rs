// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The decoder as this program uses it, through either of its two interfaces: the Rust interface of the
//! `rav1g` crate, or, with `-capi`, the functions of `rav1g.h` (`rav1g::capi`), which is what a C or C++
//! application links, the UWP test player among them. The program is the same behind both, so a
//! conformance run with `-capi` holds the C interface to the references.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

use rav1g::capi::{self, Rav1gStatus};
use rav1g::switches::{self, PerfFrame, Poison};
use rav1g::{
    Decoder, ErrorCode, Frame, ImageRequest, LowMemory, MemoryAvailable, Output, OutputImage, Plane, Settings,
};
use rav1g::{StreamInfo, peek_stream_info};
use windows::core::Interface;

use crate::device::Device;
use crate::pool::Rented;
use crate::shim::Dumps;

/// A frame as the task hashes it, from either interface.
pub struct Picture<'a> {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u32,
    pub monochrome: bool,
    /// Luma, U, V; `None` for a plane the frame does not have in a mapped buffer.
    pub planes: [Option<Plane<'a>>; 3],
}

/// What a task is to its decoder: where the output images come from and where the frames go.
pub trait Sink: Send + Sync + 'static {
    /// An output image of `size` bytes; `None` fails the temporal unit.
    fn rent(&self, size: usize) -> Option<Rented>;
    /// The decoder is done with an image.
    fn give_back(&self, token: usize);
    /// A frame whose GPU work is complete, on the decoder's completion thread.
    fn picture(&self, picture: &Picture<'_>);
    /// -gputime: a record of the timing run.
    fn perf_frame(&self, frame: &PerfFrame);
}

/// What a decoder is created with.
#[derive(Clone, Copy)]
pub struct Spec {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u32,
    pub threads: u32,
    pub slots: u32,
    /// The parts of -lowmemparts; 0 is none.
    pub low_memory: u32,
    pub intra_v2: bool,
    pub inter_fast: bool,
    pub operating_point: u32,
    pub all_layers: bool,
    /// -parser rav1d: the parse pass of rav1d reads the bitstream.
    pub parser_rav1d: bool,
    /// -budget: what the budget callback answers, in bytes.
    pub budget: Option<u64>,
}

/// The shader choices of a task.
#[derive(Clone, Copy)]
pub struct Choices {
    pub cdef_v2: bool,
    pub intra_v2: bool,
    pub intra_opt: bool,
    pub async_clear: bool,
    pub lf_h_split: u32,
    pub intra_edge_fix: bool,
    pub inter_fast: u32,
}

/// A decode call that failed.
pub struct DecodeError {
    /// The stream went on with a sequence the decoder does not fit.
    pub needs_reconfigure: bool,
    /// What the decoder said about it.
    pub detail: String,
    /// The kind and the detail, as one text.
    pub text: String,
}

fn c_text(text: *const c_char) -> String {
    if text.is_null() {
        String::new()
    } else {
        // SAFETY: the library returns NUL-terminated strings.
        unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
    }
}

/// The task's sink as the output of a decoder of the Rust interface.
struct RustOutput<S: Sink>(Arc<S>);

fn picture_of(frame: &Frame) -> Picture<'_> {
    Picture {
        width: frame.width,
        height: frame.height,
        bit_depth: frame.bit_depth,
        monochrome: frame.monochrome,
        planes: [frame.plane(0), frame.plane(1), frame.plane(2)],
    }
}

impl<S: Sink> Output for RustOutput<S> {
    fn image(&self, request: &ImageRequest) -> Option<OutputImage> {
        let buffer = self.0.rent(request.buffer_size)?;
        // SAFETY: the buffer stays mapped for as long as the pool has it, and the pool keeps a buffer
        // that is rented; the sink with its pool outlives the decoders of the task.
        Some(unsafe { OutputImage::buffer(buffer.resource, buffer.mem, buffer.size, buffer.token) })
    }

    fn unused(&self, image: OutputImage) {
        self.0.give_back(image.token);
    }

    fn frame(&self, frame: Frame) {
        self.0.picture(&picture_of(&frame));
        self.0.give_back(frame.image.token);
    }
}

// The same three as callbacks of rav1g.h; the context is the sink.

unsafe extern "C" fn c_get_image<S: Sink>(
    context: *mut c_void,
    request: *const capi::Rav1gImageRequest,
    image: *mut capi::Rav1gImage,
) -> i32 {
    // SAFETY: the context is the sink of the task, kept by the task's decoder; the other two are the
    // library's, valid during the call.
    let (sink, request, image) = unsafe { (&*context.cast::<S>(), &*request, &mut *image) };
    let Some(buffer) = sink.rent(request.buffer_size) else {
        return -1;
    };
    // The pool keeps its reference of the buffer while it is rented.
    image.d3d12_buffer = buffer.resource.as_raw();
    image.memory = buffer.mem;
    image.buffer_size = buffer.size;
    image.token = buffer.token as *mut c_void;
    0
}

unsafe extern "C" fn c_unused<S: Sink>(context: *mut c_void, token: *mut c_void) {
    // SAFETY: the context is the sink of the task.
    unsafe { &*context.cast::<S>() }.give_back(token as usize);
}

unsafe extern "C" fn c_frame<S: Sink>(context: *mut c_void, frame: *const capi::Rav1gFrame) {
    // SAFETY: the context is the sink of the task; the frame is the library's, valid during the call.
    let (sink, frame) = unsafe { (&*context.cast::<S>(), &*frame) };
    let plane = |index: usize| {
        if frame.planes[index].is_null() {
            return None;
        }
        let (width, height) =
            if index == 0 { (frame.width, frame.height) } else { (frame.width.div_ceil(2), frame.height.div_ceil(2)) };
        let stride = frame.strides[index] as usize;
        // SAFETY: rav1g.h: a plane has its rows, a stride apart, in the mapping of the image's buffer.
        let data = unsafe { std::slice::from_raw_parts(frame.planes[index], stride * height as usize) };
        Some(Plane { data, stride, width, height })
    };
    sink.picture(&Picture {
        width: frame.width,
        height: frame.height,
        bit_depth: frame.bit_depth,
        monochrome: frame.monochrome,
        planes: [plane(0), plane(1), plane(2)],
    });
    sink.give_back(frame.image.token as usize);
}

unsafe extern "C" fn c_budget(bytes: *mut c_void) -> i64 {
    bytes as usize as i64
}

unsafe extern "C" fn c_perf_frame<S: Sink>(context: *mut c_void, frame: *const PerfFrame) {
    // SAFETY: the context is the sink of the task, set for as long as the callback is; the record is the
    // library's, valid during the call.
    unsafe { (*context.cast::<S>()).perf_frame(&*frame) };
}

/// The decoder's device: the program's D3D12 device and queue with the decoder's pipeline states, behind
/// the interface the program was started for.
pub enum Library {
    Rust(rav1g::Device),
    C(*mut capi::Rav1gDevice),
}

impl Library {
    /// Starts the creation of the pipeline states on `threads` threads and waits for them. `None` when
    /// they could not be created.
    pub fn open(dev: &Device, capi: bool, threads: u32) -> Option<Library> {
        if capi {
            // SAFETY: the program's device and queue, which outlive the library's device.
            let device = unsafe { capi::rav1g_device_create(dev.device_ptr(), dev.queue_ptr(), threads) };
            if device.is_null() {
                return None;
            }
            let library = Library::C(device);
            // SAFETY: a device of rav1g_device_create.
            (unsafe { capi::rav1g_device_wait_pipelines(device) } == Rav1gStatus::Ok).then_some(library)
        } else {
            let device = rav1g::Device::with_d3d12(&dev.device, Some(&dev.queue), threads).ok()?;
            device.wait_pipelines().is_ok().then_some(Library::Rust(device))
        }
    }

    fn is_c(&self) -> bool {
        matches!(self, Library::C(_))
    }

    /// The sequence header of a temporal unit.
    pub fn peek(&self, data: &[u8]) -> Option<StreamInfo> {
        if !self.is_c() {
            return peek_stream_info(data).ok();
        }
        let mut info = capi::Rav1gStreamInfo::default();
        // SAFETY: a valid buffer and a valid result.
        (unsafe { capi::rav1g_peek_stream_info(data.as_ptr(), data.len(), &mut info) } == Rav1gStatus::Ok).then_some(
            StreamInfo {
                width: info.width,
                height: info.height,
                is_key_frame: info.is_key_frame,
                bit_depth: info.bit_depth,
            },
        )
    }

    fn settings(spec: &Spec) -> Settings {
        Settings {
            width: spec.width,
            height: spec.height,
            bit_depth: spec.bit_depth,
            threads: spec.threads,
            frame_slots: spec.slots,
            // The program always says which parts it wants: no part is "none", not the library's default.
            low_memory: LowMemory(spec.low_memory),
            intra_v2: spec.intra_v2,
            inter_fast: spec.inter_fast,
            packed_10x3: false,
            operating_point: spec.operating_point,
            output_all_layers: spec.all_layers,
            parser: if spec.parser_rav1d { rav1g::Parser::Rav1d } else { rav1g::Parser::Libaom },
            memory_available: spec.budget.map(|bytes| -> MemoryAvailable { Arc::new(move || Some(bytes)) }),
        }
    }

    fn c_settings<S: Sink>(spec: &Spec, sink: Option<&Arc<S>>) -> capi::Rav1gSettings {
        // SAFETY: an all-zero Rav1gSettings is valid (no callbacks), and the call fills it.
        let mut settings: capi::Rav1gSettings = unsafe { std::mem::zeroed() };
        // SAFETY: valid for writing.
        unsafe { capi::rav1g_settings_default(&mut settings) };
        settings.width = spec.width;
        settings.height = spec.height;
        settings.bit_depth = spec.bit_depth;
        settings.threads = spec.threads;
        settings.frame_slots = spec.slots;
        settings.low_memory = spec.low_memory;
        settings.intra_v2 = spec.intra_v2;
        settings.inter_fast = spec.inter_fast;
        settings.operating_point = spec.operating_point;
        settings.output_all_layers = spec.all_layers;
        settings.parser = if spec.parser_rav1d { capi::RAV1G_PARSER_RAV1D } else { capi::RAV1G_PARSER_LIBAOM };
        if let Some(bytes) = spec.budget {
            // The number travels as the context of its callback.
            settings.memory_available = Some(c_budget);
            settings.memory_available_context = bytes as usize as *mut c_void;
        }
        if let Some(sink) = sink {
            settings.output = capi::Rav1gOutput {
                context: Arc::as_ptr(sink).cast_mut().cast(),
                get_image: Some(c_get_image::<S>),
                frame: Some(c_frame::<S>),
                unused: Some(c_unused::<S>),
                end_of_frames: None,
            };
        }
        settings
    }

    /// (device buffers, host pool) of a decoder, in bytes.
    pub fn memory_requirements(&self, spec: &Spec) -> (usize, usize) {
        if self.is_c() {
            let settings = Library::c_settings::<NoSink>(spec, None);
            let mut memory = capi::Rav1gMemoryRequirements::default();
            // SAFETY: valid settings and a valid result.
            unsafe { capi::rav1g_memory_requirements(&settings, &mut memory) };
            (memory.device_size, memory.host_size)
        } else {
            let memory = Library::settings(spec).memory_requirements();
            (memory.device_size, memory.host_size)
        }
    }

    /// Creates a decoder whose output is `sink`. The error says what failed.
    pub fn create<S: Sink>(&self, spec: &Spec, sink: &Arc<S>) -> Result<TaskDecoder<S>, String> {
        match self {
            Library::Rust(device) => Decoder::new(device, &Library::settings(spec), Arc::new(RustOutput(sink.clone())))
                .map(TaskDecoder::Rust)
                .map_err(|error| error.to_string()),
            Library::C(device) => {
                let settings = Library::c_settings(spec, Some(sink));
                let mut decoder = std::ptr::null_mut();
                // SAFETY: a device of rav1g_device_create, valid settings; the sink behind the callbacks is
                // kept by the TaskDecoder.
                if unsafe { capi::rav1g_decoder_create(*device, &settings, &mut decoder) } != Rav1gStatus::Ok {
                    return Err(c_text(capi::rav1g_last_error()));
                }
                Ok(TaskDecoder::C(decoder, sink.clone()))
            }
        }
    }

    /// The shader choices, set while no frame is in flight.
    pub fn set_choices(&self, choices: &Choices) {
        if self.is_c() {
            capi::rav1g_set_cdef_v2(choices.cdef_v2);
            capi::rav1g_set_intra_v2(choices.intra_v2);
            capi::rav1g_set_intra_opt(choices.intra_opt);
            capi::rav1g_set_async_clear(choices.async_clear);
            capi::rav1g_set_lf_h_split(choices.lf_h_split);
            capi::rav1g_set_intra_edge_fix(choices.intra_edge_fix);
            capi::rav1g_set_inter_fast(choices.inter_fast);
        } else {
            switches::set_cdef_v2(choices.cdef_v2);
            switches::set_intra_v2(choices.intra_v2);
            switches::set_intra_opt(choices.intra_opt);
            switches::set_async_clear(choices.async_clear);
            switches::set_lf_h_split(choices.lf_h_split);
            switches::set_intra_edge_fix(choices.intra_edge_fix);
            switches::set_inter_fast(choices.inter_fast);
        }
    }

    /// -poisonmi, -poisonbuf: for the decoders created after the call.
    pub fn set_poison(&self, level: Poison) {
        if self.is_c() {
            capi::rav1g_set_poison_buffers(level as u32);
        } else {
            switches::set_poison_buffers(level);
        }
    }

    /// -gputime: the records of the timing run go to `sink`, with the inter breakdown of `detail`; `None`
    /// switches the timing off.
    ///
    /// # Safety
    /// The sink outlives the use of the callback: the timing is switched off, behind the drain of the
    /// decoder, before the sink is dropped.
    pub unsafe fn set_perf<S: Sink>(&self, sink: Option<&Arc<S>>, detail: u32) {
        if self.is_c() {
            let context = sink.map_or(std::ptr::null_mut(), |sink| Arc::as_ptr(sink).cast_mut().cast());
            // SAFETY: the caller's contract.
            unsafe { capi::rav1g_set_perf_callback(sink.map(|_| c_perf_frame::<S> as _), context) };
            capi::rav1g_set_perf_detail(if sink.is_some() { detail } else { 0 });
        } else {
            switches::set_perf_callback(sink.map(|sink| -> switches::PerfCallback {
                let sink = sink.clone();
                Arc::new(move |frame| sink.perf_frame(frame))
            }));
            switches::set_perf_detail(if sink.is_some() { detail } else { 0 });
        }
    }

    /// -dumpstages: the stage dumps of `frames` go to `dumps`; `None` switches them off.
    ///
    /// # Safety
    /// The writer outlives the use of the callback: the dumps are switched off, behind the drain of the
    /// decoder, before the writer is dropped.
    pub unsafe fn set_stage_dump(&self, dumps: Option<&Dumps>, frames: &[i32]) {
        // SAFETY: the caller's contract, for both interfaces.
        unsafe {
            if self.is_c() {
                match dumps {
                    Some(dumps) => {
                        let (callback, context) = dumps.c_callback();
                        capi::rav1g_set_stage_dump(Some(callback), context, frames.as_ptr(), frames.len());
                    }
                    None => capi::rav1g_set_stage_dump(None, std::ptr::null_mut(), std::ptr::null(), 0),
                }
            } else {
                switches::set_stage_dump(dumps.map(|dumps| dumps.callback()), frames);
            }
        }
    }

    /// Empty for the normal build of the shaders.
    pub fn shader_variant(&self) -> String {
        if self.is_c() { c_text(capi::rav1g_shader_variant()) } else { switches::shader_variant() }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        if let Library::C(device) = self {
            // SAFETY: a device of rav1g_device_create, destroyed once.
            unsafe { capi::rav1g_device_destroy(*device) };
        }
    }
}

/// The log lines of the decoder go to `line`, through the interface the program was started for.
pub fn set_log(capi: bool, line: fn(bool, &str)) {
    unsafe extern "C" fn c_line(context: *mut c_void, is_error: bool, text: *const c_char) {
        // SAFETY: the context is the function `set_log` was given, as a pointer; the text is a C string.
        let line: fn(bool, &str) = unsafe { std::mem::transmute(context) };
        line(is_error, &c_text(text));
    }
    if capi {
        // SAFETY: the callback is a function of the program; its context is a function pointer.
        unsafe { capi::rav1g_set_log_callback(Some(c_line), line as *mut c_void) };
    } else {
        switches::set_log_callback(Some(Arc::new(line)));
    }
}

/// For the settings of a memory query, which have no output.
enum NoSink {}

impl Sink for NoSink {
    fn rent(&self, _size: usize) -> Option<Rented> {
        match *self {}
    }
    fn give_back(&self, _token: usize) {}
    fn picture(&self, _picture: &Picture<'_>) {}
    fn perf_frame(&self, _frame: &PerfFrame) {}
}

/// A decoder of a task.
pub enum TaskDecoder<S: Sink> {
    Rust(Decoder),
    /// With the sink its callbacks use, which is kept for them.
    C(*mut capi::Rav1gDecoder, #[allow(dead_code)] Arc<S>),
}

impl<S: Sink> TaskDecoder<S> {
    /// One temporal unit.
    pub fn decode(&mut self, data: &[u8]) -> Result<(), DecodeError> {
        match self {
            TaskDecoder::Rust(decoder) => decoder.decode(data, 0).map_err(|error| DecodeError {
                needs_reconfigure: error.code == ErrorCode::NeedsReconfigure,
                text: error.to_string(),
                detail: error.detail,
            }),
            TaskDecoder::C(decoder, _) => {
                // SAFETY: a decoder of rav1g_decoder_create, used by this thread; a valid buffer.
                let status = unsafe { capi::rav1g_decoder_decode(*decoder, data.as_ptr(), data.len(), 0) };
                if status == Rav1gStatus::Ok {
                    return Ok(());
                }
                // SAFETY: the decoder is valid.
                let (message, detail) = unsafe {
                    (c_text(capi::rav1g_decoder_error(*decoder)), c_text(capi::rav1g_decoder_error_detail(*decoder)))
                };
                let mut text = if message.is_empty() { "?".to_string() } else { message };
                if !detail.is_empty() {
                    text = format!("{text}: {detail}");
                }
                Err(DecodeError { needs_reconfigure: status == Rav1gStatus::NeedsReconfigure, detail, text })
            }
        }
    }

    /// Every frame that was submitted has been handed over.
    pub fn drain(&mut self) {
        match self {
            TaskDecoder::Rust(decoder) => decoder.drain(),
            // SAFETY: a decoder of rav1g_decoder_create, used by this thread.
            TaskDecoder::C(decoder, _) => unsafe { capi::rav1g_decoder_drain(*decoder) },
        }
    }
}

impl<S: Sink> Drop for TaskDecoder<S> {
    fn drop(&mut self) {
        if let TaskDecoder::C(decoder, _) = self {
            // SAFETY: a decoder of rav1g_decoder_create, destroyed once; its sink is dropped after it.
            unsafe { capi::rav1g_decoder_destroy(*decoder) };
        }
    }
}

// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The interface of the decoder for C: the functions and types of `rav1g.h`.
//!
//! `rav1g.h` (`crates/rav1g/include/rav1g.h`) is written from this file by cbindgen
//! (`cargo run -p rav1g-header`); the comments here are the comments there. Everything is the Rust
//! interface of this crate behind C types: a device, settings, the output callbacks, a decoder.
//!
//! This file is read by cbindgen on its own: every type of a signature is defined here or is a C type.

use std::cell::RefCell;
use std::ffi::{CString, c_char, c_void};
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::core::Interface;

use crate::sys::{av1_perf_frame, av1_stage_dump};
use crate::{Decoder, Device, Error, ErrorCode, Frame, ImageFormat, ImageRequest, LowMemory, Output, OutputImage};
use crate::{Settings, switches};

/// The shared back buffer: one back buffer for all pipeline slots and a pool of 10 frame buffers. The
/// default of `rav1g_settings_default`.
pub const RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER: u32 = 1;
/// The mode info of a frame in shares that a pipeline slot takes as it needs them.
pub const RAV1G_LOW_MEMORY_MODE_INFO: u32 = 2;
/// The coefficients of a frame in shares, likewise.
pub const RAV1G_LOW_MEMORY_COEFS: u32 = 4;
/// All memory reductions.
pub const RAV1G_LOW_MEMORY_ALL: u32 = 7;

/// `rav1g_set_inter_fast`: the fast inter path is on.
pub const RAV1G_INTER_FAST_ON: u32 = 1;
/// With `RAV1G_INTER_FAST_ON`: the blocks with one reference are listed and predicted by the original
/// shader.
pub const RAV1G_INTER_FAST_SINGLE_ORIGINAL: u32 = 2;
/// With `RAV1G_INTER_FAST_ON`: the same for the averaged compound blocks.
pub const RAV1G_INTER_FAST_COMPOUND_ORIGINAL: u32 = 4;
/// The lists of the fast inter path with the original shaders on all of them.
pub const RAV1G_INTER_FAST_LISTS_ONLY: u32 = 7;

/// `rav1g_set_poison_buffers`: the mode info pool.
pub const RAV1G_POISON_MODE_INFO: u32 = 1;
/// `rav1g_set_poison_buffers`: every buffer the CPU writes for the shaders, and the constants of every
/// stage in every frame.
pub const RAV1G_POISON_BUFFERS: u32 = 2;

/// The budget guard refuses a decoder whose device buffers would leave the application less than this
/// many bytes (`Rav1gSettings::memory_available`).
pub const RAV1G_MEMORY_GUARD_MARGIN: u64 = 50331648;

const _: () = {
    assert!(RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER == LowMemory::SHARED_BACK_BUFFER.0);
    assert!(RAV1G_LOW_MEMORY_MODE_INFO == LowMemory::MODE_INFO.0);
    assert!(RAV1G_LOW_MEMORY_COEFS == LowMemory::COEFS.0);
    assert!(RAV1G_LOW_MEMORY_ALL == LowMemory::ALL.0);
    assert!(RAV1G_INTER_FAST_ON == switches::INTER_FAST_ON);
    assert!(RAV1G_INTER_FAST_SINGLE_ORIGINAL == switches::INTER_FAST_SINGLE_ORIGINAL);
    assert!(RAV1G_INTER_FAST_COMPOUND_ORIGINAL == switches::INTER_FAST_COMPOUND_ORIGINAL);
    assert!(RAV1G_INTER_FAST_LISTS_ONLY == switches::INTER_FAST_LISTS_ONLY);
    assert!(RAV1G_POISON_MODE_INFO == switches::Poison::ModeInfo as u32);
    assert!(RAV1G_POISON_BUFFERS == switches::Poison::Buffers as u32);
    assert!(RAV1G_MEMORY_GUARD_MARGIN == 48 << 20);
};

/// How a call ended.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rav1gStatus {
    Ok = 0,
    /// An error without a kind of its own; also a device or a pipeline state that could not be created.
    Error = 1,
    /// Memory could not be had, or the budget guard refused the creation.
    Memory = 2,
    AbiMismatch = 3,
    Incapable = 4,
    /// The stream is not one the decoder reads.
    UnsupportedBitstream = 5,
    /// The stream uses something the decoder does not have.
    UnsupportedFeature = 6,
    /// The data of a frame is damaged or cut.
    CorruptFrame = 7,
    InvalidParam = 8,
    /// A new sequence whose frames are larger, or have more bits a sample, than the decoder was created
    /// for. Nothing of the temporal unit was decoded: drain and destroy the decoder, create one for the
    /// new sequence (`rav1g_peek_stream_info` on the temporal unit), pass the temporal unit again.
    NeedsReconfigure = 10,
}

impl From<ErrorCode> for Rav1gStatus {
    fn from(code: ErrorCode) -> Rav1gStatus {
        match code {
            ErrorCode::Error => Rav1gStatus::Error,
            ErrorCode::Memory => Rav1gStatus::Memory,
            ErrorCode::AbiMismatch => Rav1gStatus::AbiMismatch,
            ErrorCode::Incapable => Rav1gStatus::Incapable,
            ErrorCode::UnsupportedBitstream => Rav1gStatus::UnsupportedBitstream,
            ErrorCode::UnsupportedFeature => Rav1gStatus::UnsupportedFeature,
            ErrorCode::CorruptFrame => Rav1gStatus::CorruptFrame,
            ErrorCode::InvalidParam => Rav1gStatus::InvalidParam,
            ErrorCode::NeedsReconfigure => Rav1gStatus::NeedsReconfigure,
        }
    }
}

/// A device for decoders: a D3D12 device, the direct queue the decoders submit to, and the decoder's
/// compute pipeline states on that device.
pub struct Rav1gDevice {
    device: Device,
}

/// A decoder.
pub struct Rav1gDecoder {
    decoder: Decoder,
    message: CString,
    detail: CString,
}

/// What the sequence header of a temporal unit says about the stream.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Rav1gStreamInfo {
    /// Largest frame width and height of the sequence.
    pub width: u32,
    pub height: u32,
    /// Bits a sample: 8, 10 or 12; 0 when the temporal unit has no sequence header.
    pub bit_depth: u32,
    /// The temporal unit starts with a key frame.
    pub is_key_frame: bool,
}

/// The samples of an output image.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rav1gImageFormat {
    /// One byte a sample; textures of R8.
    Bits8 = 0,
    /// Two bytes a sample, the 10 bits in the low bits; textures of R16.
    Bits10 = 1,
    /// Three samples of 10 bits packed into 32 bits (`Rav1gSettings::packed_10x3`); textures of
    /// R10G10B10A2 with a third of the width.
    Packed10x3 = 2,
}

/// What the decoder needs an output image for: a frame that is going to be output.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rav1gImageRequest {
    /// Bytes of a buffer that takes the three planes of the frame.
    pub buffer_size: usize,
    /// Luma samples of a row, and rows, of the frame.
    pub width: u32,
    pub height: u32,
    pub format: Rav1gImageFormat,
}

/// An output image of the application: where the decoder's GPU writes a frame. A buffer that takes the
/// three planes one behind the other (to read the frame on the CPU), one texture per plane in the copy
/// destination state (to use it on the GPU), or both. The decoder holds a reference of each resource from
/// `Rav1gOutput::get_image` until it has handed the image back (`Rav1gOutput::frame`,
/// `Rav1gOutput::unused`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rav1gImage {
    /// `ID3D12Resource *` of the buffer, or null.
    pub d3d12_buffer: *mut c_void,
    /// The mapping of the buffer, or null. Valid for `buffer_size` bytes while the decoder has the image.
    pub memory: *mut u8,
    /// Bytes of the buffer.
    pub buffer_size: usize,
    /// `ID3D12Resource *` of the textures of the luma, U and V plane, or null.
    pub d3d12_textures: [*mut c_void; 3],
    /// The application's name for the image; the decoder only carries it.
    pub token: *mut c_void,
}

/// A decoded frame, in the image the application gave for it. Valid during `Rav1gOutput::frame`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rav1gFrame {
    /// The image, as `Rav1gOutput::get_image` filled it.
    pub image: Rav1gImage,
    /// Luma samples of a row, and rows.
    pub width: u32,
    pub height: u32,
    /// Bits a sample: 8 or 10.
    pub bit_depth: u32,
    /// The frame has a luma plane only.
    pub monochrome: bool,
    /// The samples are packed three to 32 bits (`Rav1gImageFormat::Packed10x3`).
    pub packed_10x3: bool,
    /// What was passed to `rav1g_decoder_decode` with the temporal unit of the frame.
    pub user_data: u64,
    /// The planes (luma, U, V) in the mapping of the image's buffer; null without a mapping, and for the
    /// chroma planes of a monochrome frame.
    pub planes: [*const u8; 3],
    /// Bytes from one row of a plane to the next.
    pub strides: [u32; 3],
    /// `ID3D12Fence *` of the command list that wrote the frame into its image, and the value the fence
    /// has when that list has run. The frame is handed over when the list has completed, so the fence has
    /// reached the value; the pair is here for an application that orders its GPU work by fence. The
    /// pointer is valid during the call (AddRef to keep it).
    pub d3d12_fence: *mut c_void,
    pub fence_value: u64,
}

/// The output of a decoder: where it gets its images and puts its frames. `get_image` and `unused` are
/// called on the thread that calls `rav1g_decoder_decode` (or destroys the decoder), `frame` and
/// `end_of_frames` on the decoder's completion thread. `get_image` and `frame` are needed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rav1gOutput {
    /// Passed to every callback.
    pub context: *mut c_void,
    /// An image for a frame that is going to be output: fills `image` and returns 0, or returns another
    /// value, which fails the temporal unit. The call may wait until the application has an image free;
    /// the decoder then waits with it.
    pub get_image: Option<
        unsafe extern "C" fn(context: *mut c_void, request: *const Rav1gImageRequest, image: *mut Rav1gImage) -> i32,
    >,
    /// A frame is ready: the GPU has written it into its image. Frames arrive in output order. The image
    /// is the application's again when the call returns.
    pub frame: Option<unsafe extern "C" fn(context: *mut c_void, frame: *const Rav1gFrame)>,
    /// An image the decoder took and did not write a frame into; `token` is `Rav1gImage::token`.
    pub unused: Option<unsafe extern "C" fn(context: *mut c_void, token: *mut c_void)>,
    /// Every frame that was submitted before a drain has been handed over.
    pub end_of_frames: Option<unsafe extern "C" fn(context: *mut c_void)>,
}

/// What a decoder is created with. `rav1g_settings_default` fills the defaults.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rav1gSettings {
    /// The frame size the buffers are made for: the largest frame of the sequence
    /// (`rav1g_peek_stream_info`). The decoder rounds both up to a multiple of 128.
    pub width: u32,
    pub height: u32,
    /// 8 for a decoder with 8-bit frame buffers (an 8-bit stream), 10 otherwise.
    pub bit_depth: u32,
    /// Tile threads.
    pub threads: u32,
    /// Pipeline slots: frames that can be between the start of their CPU decode and the end of their GPU
    /// work at the same time, 2 to 4. 0 is the default, 2. Each slot has buffers of its own.
    pub frame_slots: u32,
    /// The memory reductions: `RAV1G_LOW_MEMORY_*` values added up, 0 for none.
    pub low_memory: u32,
    /// Buffers for the second intra path (`rav1g_set_intra_v2`).
    pub intra_v2: bool,
    /// Room in the block maps for the lists of the fast inter path (`rav1g_set_inter_fast`).
    pub inter_fast: bool,
    /// 10-bit frames are put out with three samples packed into 32 bits.
    pub packed_10x3: bool,
    /// The frame of every spatial layer of a temporal unit is output, instead of the highest one.
    pub output_all_layers: bool,
    /// The operating point of a stream with layers.
    pub operating_point: u32,
    /// The budget guard, or null. On a console an application that goes over its memory limit loses its
    /// D3D12 device, so with this callback the decoder asks before it allocates. The callback returns the
    /// bytes the decoder may still take: what the application has left of its limit, less what it is
    /// going to allocate itself (output images that do not exist yet), or a negative value when that is
    /// not known. A creation whose device buffers would leave less than `RAV1G_MEMORY_GUARD_MARGIN` is
    /// refused (`Rav1gStatus::Memory`, a "MEMORY REFUSED" log line), and so is a palette buffer that would
    /// leave less than a third of that (a decode error). It is called on the threads that create, use and
    /// destroy the decoder, with `memory_available_context`.
    pub memory_available: Option<unsafe extern "C" fn(context: *mut c_void) -> i64>,
    pub memory_available_context: *mut c_void,
    /// The output of the decoder.
    pub output: Rav1gOutput,
}

/// What a decoder of some settings takes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Rav1gMemoryRequirements {
    /// Bytes of host memory (the decoder allocates them at creation).
    pub host_size: usize,
    /// Bytes of the device buffers the creation allocates.
    pub device_size: usize,
    /// Bytes of the device buffers that are only created when a stream needs them: one palette buffer per
    /// pipeline slot.
    pub device_size_deferred: usize,
}

/// A record of the timing run (`av1_perf_frame` of `dx/av1_perf.h` in the decoder's sources).
pub type Rav1gPerfCallback = Option<unsafe extern "C" fn(context: *mut c_void, frame: *const av1_perf_frame)>;
/// The buffers behind a GPU stage of a frame (`av1_stage_dump` of `dx/av1_perf.h`).
pub type Rav1gStageDumpCallback = Option<unsafe extern "C" fn(context: *mut c_void, dump: *const av1_stage_dump)>;
/// A log line of the decoder.
pub type Rav1gLogCallback = Option<unsafe extern "C" fn(context: *mut c_void, is_error: bool, line: *const c_char)>;

/// A context pointer of the application, carried into a callback on another thread.
#[derive(Clone, Copy)]
struct Context(*mut c_void);

// SAFETY: the pointer is only handed back to the application's callback, which the interface says is
// called from the decoder's threads.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', " ")).unwrap_or_default()
}

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_last_error(error: &Error) -> Rav1gStatus {
    LAST_ERROR.with(|last| *last.borrow_mut() = c_string(&error.to_string()));
    error.code.into()
}

/// The text of the last error on this thread of a call that has no decoder to ask:
/// `rav1g_device_create`, `rav1g_device_wait_pipelines`, `rav1g_decoder_create`. For a creation the
/// budget guard refused or that ran out of memory it ends with the decoder's "MEMORY REFUSED" or
/// "MEMORY FAILED" line. Valid until the next such call on the thread.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_last_error() -> *const c_char {
    LAST_ERROR.with(|last| last.borrow().as_ptr())
}

/// The version of the library, "0.4.0".
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Reads the sequence header of a temporal unit, without a decoder.
///
/// # Safety
/// `data` is valid for `size` bytes; `info` is valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_peek_stream_info(
    data: *const u8,
    size: usize,
    info: *mut Rav1gStreamInfo,
) -> Rav1gStatus {
    if data.is_null() || info.is_null() {
        return Rav1gStatus::InvalidParam;
    }
    // SAFETY: the caller's contract.
    match crate::peek_stream_info(unsafe { std::slice::from_raw_parts(data, size) }) {
        Ok(si) => {
            // SAFETY: the caller's contract.
            unsafe {
                *info = Rav1gStreamInfo {
                    width: si.width,
                    height: si.height,
                    bit_depth: si.bit_depth,
                    is_key_frame: si.is_key_frame,
                };
            }
            Rav1gStatus::Ok
        }
        Err(error) => error.code.into(),
    }
}

/// A device for decoders. `d3d12_device` is the application's `ID3D12Device *`, or null for a device of
/// the decoder's own on the first hardware adapter; `d3d12_queue` is the direct `ID3D12CommandQueue *` the
/// decoders submit to, or null for a queue of the decoder's own. The decoder's compute pipeline states
/// are created on `pipeline_threads` threads (6 at most) while the call returns
/// (`rav1g_device_wait_pipelines`), or by this call with 0. Null when the device, the queue or a pipeline
/// layout could not be created (`rav1g_last_error`).
///
/// # Safety
/// The pointers are null or valid interfaces of the kinds named.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_device_create(
    d3d12_device: *mut c_void,
    d3d12_queue: *mut c_void,
    pipeline_threads: u32,
) -> *mut Rav1gDevice {
    let device = if d3d12_device.is_null() {
        Device::new(pipeline_threads)
    } else {
        // SAFETY: the caller's contract.
        unsafe { Device::from_raw(d3d12_device, d3d12_queue, pipeline_threads) }
    };
    match device {
        Ok(device) => Box::into_raw(Box::new(Rav1gDevice { device })),
        Err(error) => {
            set_last_error(&error);
            std::ptr::null_mut()
        }
    }
}

/// Waits until every pipeline state of the device exists; the first decoder creation does the same. An
/// error when one could not be created; the log names it.
///
/// # Safety
/// `device` is a device of `rav1g_device_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_device_wait_pipelines(device: *const Rav1gDevice) -> Rav1gStatus {
    // SAFETY: the caller's contract.
    let Some(device) = (unsafe { device.as_ref() }) else {
        return Rav1gStatus::InvalidParam;
    };
    match device.device.wait_pipelines() {
        Ok(()) => Rav1gStatus::Ok,
        Err(error) => set_last_error(&error),
    }
}

/// The `ID3D12Device *` of the device; no reference is added.
///
/// # Safety
/// `device` is a device of `rav1g_device_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_device_d3d12_device(device: *const Rav1gDevice) -> *mut c_void {
    // SAFETY: the caller's contract.
    unsafe { device.as_ref() }.map_or(std::ptr::null_mut(), |device| device.device.d3d12_device().as_raw())
}

/// The `ID3D12CommandQueue *` the decoders of the device submit to; no reference is added.
///
/// # Safety
/// `device` is a device of `rav1g_device_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_device_d3d12_queue(device: *const Rav1gDevice) -> *mut c_void {
    // SAFETY: the caller's contract.
    unsafe { device.as_ref() }.map_or(std::ptr::null_mut(), |device| device.device.d3d12_queue().as_raw())
}

/// Releases the application's handle of a device. The D3D12 device, the queue and the pipeline states
/// live until the last decoder of the device is destroyed.
///
/// # Safety
/// `device` is null or a device of `rav1g_device_create`, which is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_device_destroy(device: *mut Rav1gDevice) {
    if !device.is_null() {
        // SAFETY: the caller's contract: the box of rav1g_device_create.
        drop(unsafe { Box::from_raw(device) });
    }
}

/// One log line with the video memory of the adapter of an `ID3D12Device *` ("MEMORY dxgi `label`: ...").
///
/// # Safety
/// `d3d12_device` is null or a valid `ID3D12Device *`; `label` is null or a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_log_video_memory(d3d12_device: *mut c_void, label: *const c_char) {
    // SAFETY: the caller's contract.
    unsafe { crate::sys::av1_log_video_memory_now(d3d12_device, label) };
}

/// The defaults: a decoder for 8-bit 1920x1088 frames with one tile thread, two pipeline slots and the
/// shared back buffer, no budget guard, and no output (the application sets `output`).
///
/// # Safety
/// `settings` is valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_settings_default(settings: *mut Rav1gSettings) {
    if settings.is_null() {
        return;
    }
    let defaults = Settings::default();
    // SAFETY: the caller's contract.
    unsafe {
        *settings = Rav1gSettings {
            width: defaults.width,
            height: defaults.height,
            bit_depth: defaults.bit_depth,
            threads: defaults.threads,
            frame_slots: defaults.frame_slots,
            low_memory: defaults.low_memory.0,
            intra_v2: defaults.intra_v2,
            inter_fast: defaults.inter_fast,
            packed_10x3: defaults.packed_10x3,
            output_all_layers: defaults.output_all_layers,
            operating_point: defaults.operating_point,
            memory_available: None,
            memory_available_context: std::ptr::null_mut(),
            output: Rav1gOutput {
                context: std::ptr::null_mut(),
                get_image: None,
                frame: None,
                unused: None,
                end_of_frames: None,
            },
        };
    }
}

fn settings_of(settings: &Rav1gSettings) -> Settings {
    let context = Context(settings.memory_available_context);
    Settings {
        width: settings.width,
        height: settings.height,
        bit_depth: settings.bit_depth,
        threads: settings.threads,
        frame_slots: settings.frame_slots,
        low_memory: LowMemory(settings.low_memory),
        intra_v2: settings.intra_v2,
        inter_fast: settings.inter_fast,
        packed_10x3: settings.packed_10x3,
        operating_point: settings.operating_point,
        output_all_layers: settings.output_all_layers,
        memory_available: settings.memory_available.map(|available| -> crate::MemoryAvailable {
            Arc::new(move || {
                let context = context;
                // SAFETY: the application's callback with its context, as the interface says.
                u64::try_from(unsafe { available(context.0) }).ok()
            })
        }),
    }
}

/// The memory a decoder of some settings takes.
///
/// # Safety
/// `settings` is valid; `requirements` is valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_requirements(
    settings: *const Rav1gSettings,
    requirements: *mut Rav1gMemoryRequirements,
) {
    // SAFETY: the caller's contract.
    let (Some(settings), Some(requirements)) = (unsafe { settings.as_ref() }, unsafe { requirements.as_mut() }) else {
        return;
    };
    let memory = settings_of(settings).memory_requirements();
    *requirements = Rav1gMemoryRequirements {
        host_size: memory.host_size,
        device_size: memory.device_size,
        device_size_deferred: memory.device_size_deferred,
    };
}

/// The application's output callbacks as the output of a decoder.
struct CallbackOutput(Rav1gOutput);

// SAFETY: the interface says that the callbacks are called from the decoder's threads with the context.
unsafe impl Send for CallbackOutput {}
unsafe impl Sync for CallbackOutput {}

impl Output for CallbackOutput {
    fn image(&self, request: &ImageRequest) -> Option<OutputImage> {
        let request = Rav1gImageRequest {
            buffer_size: request.buffer_size,
            width: request.width,
            height: request.height,
            format: match request.format {
                ImageFormat::Bits8 => Rav1gImageFormat::Bits8,
                ImageFormat::Bits10 => Rav1gImageFormat::Bits10,
                ImageFormat::Packed10x3 => Rav1gImageFormat::Packed10x3,
            },
        };
        let mut image = Rav1gImage {
            d3d12_buffer: std::ptr::null_mut(),
            memory: std::ptr::null_mut(),
            buffer_size: 0,
            d3d12_textures: [std::ptr::null_mut(); 3],
            token: std::ptr::null_mut(),
        };
        // SAFETY: the application's callback, called as the interface says.
        if unsafe { (self.0.get_image?)(self.0.context, &request, &mut image) } != 0 {
            return None;
        }
        // SAFETY: the application's resources: null or valid `ID3D12Resource *`; a reference of each is
        // taken for as long as the decoder has the image.
        let resource = |raw: &*mut c_void| unsafe { ID3D12Resource::from_raw_borrowed(raw) }.cloned();
        Some(OutputImage {
            buffer: resource(&image.d3d12_buffer),
            memory: image.memory,
            buffer_size: image.buffer_size,
            textures: [
                resource(&image.d3d12_textures[0]),
                resource(&image.d3d12_textures[1]),
                resource(&image.d3d12_textures[2]),
            ],
            token: image.token as usize,
        })
    }

    fn frame(&self, frame: Frame) {
        let Some(callback) = self.0.frame else {
            return;
        };
        let raw = |resource: Option<&ID3D12Resource>| resource.map_or(std::ptr::null_mut(), Interface::as_raw);
        let plane = |index: usize| {
            if frame.image.memory.is_null() || (index > 0 && frame.monochrome) {
                std::ptr::null()
            } else {
                frame.image.memory.wrapping_add(frame.offsets[index]).cast_const()
            }
        };
        let fence = frame.d3d12_fence();
        let c_frame = Rav1gFrame {
            image: Rav1gImage {
                d3d12_buffer: raw(frame.image.d3d12_buffer()),
                memory: frame.image.memory,
                buffer_size: frame.image.buffer_size,
                d3d12_textures: [
                    raw(frame.image.d3d12_texture(0)),
                    raw(frame.image.d3d12_texture(1)),
                    raw(frame.image.d3d12_texture(2)),
                ],
                token: frame.image.token as *mut c_void,
            },
            width: frame.width,
            height: frame.height,
            bit_depth: frame.bit_depth,
            monochrome: frame.monochrome,
            packed_10x3: frame.packed_10x3,
            user_data: frame.user_data,
            planes: [plane(0), plane(1), plane(2)],
            strides: [frame.strides[0] as u32, frame.strides[1] as u32, frame.strides[2] as u32],
            d3d12_fence: fence.map_or(std::ptr::null_mut(), |(fence, _)| fence.as_raw()),
            fence_value: fence.map_or(0, |(_, value)| value),
        };
        // SAFETY: the application's callback, called as the interface says; the frame and what it points
        // to are alive during the call.
        unsafe { callback(self.0.context, &c_frame) };
    }

    fn unused(&self, image: OutputImage) {
        if let Some(callback) = self.0.unused {
            // SAFETY: the application's callback, called as the interface says.
            unsafe { callback(self.0.context, image.token as *mut c_void) };
        }
    }

    fn end_of_frames(&self) {
        if let Some(callback) = self.0.end_of_frames {
            // SAFETY: the application's callback, called as the interface says.
            unsafe { callback(self.0.context) };
        }
    }
}

/// Creates a decoder on a device; waits for the pipeline states of the device when they are still being
/// created. `*decoder` is the decoder, or null with the reason as the result and in `rav1g_last_error`.
/// The context of the output callbacks and of the budget callback must stay valid until
/// `rav1g_decoder_destroy` has returned.
///
/// # Safety
/// `device` is a device of `rav1g_device_create`; `settings` is valid; `decoder` is valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_create(
    device: *const Rav1gDevice,
    settings: *const Rav1gSettings,
    decoder: *mut *mut Rav1gDecoder,
) -> Rav1gStatus {
    if decoder.is_null() {
        return Rav1gStatus::InvalidParam;
    }
    // SAFETY: the caller's contract.
    unsafe { *decoder = std::ptr::null_mut() };
    // SAFETY: the caller's contract.
    let (Some(device), Some(settings)) = (unsafe { device.as_ref() }, unsafe { settings.as_ref() }) else {
        return Rav1gStatus::InvalidParam;
    };
    if settings.output.get_image.is_none() || settings.output.frame.is_none() {
        return set_last_error(&Error::new(
            ErrorCode::InvalidParam,
            "the output has no get_image or no frame callback",
        ));
    }
    match Decoder::new(&device.device, &settings_of(settings), Arc::new(CallbackOutput(settings.output))) {
        Ok(created) => {
            let created = Rav1gDecoder { decoder: created, message: CString::default(), detail: CString::default() };
            // SAFETY: the caller's contract.
            unsafe { *decoder = Box::into_raw(Box::new(created)) };
            Rav1gStatus::Ok
        }
        Err(error) => set_last_error(&error),
    }
}

fn keep(decoder: &mut Rav1gDecoder, result: Result<(), Error>) -> Rav1gStatus {
    match result {
        Ok(()) => Rav1gStatus::Ok,
        Err(error) => {
            decoder.message = c_string(&error.message);
            decoder.detail = c_string(&error.detail);
            error.code.into()
        }
    }
}

/// Decodes one temporal unit. `user_data` comes back with the frames of the unit
/// (`Rav1gFrame::user_data`). The call returns when the frames of the unit are recorded and submitted to
/// the GPU, not when they are complete; it waits when every pipeline slot is in flight, and when the
/// output has no image. On an error `rav1g_decoder_error` and `rav1g_decoder_error_detail` say more.
///
/// # Safety
/// `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time; `data` is valid for
/// `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_decode(
    decoder: *mut Rav1gDecoder,
    data: *const u8,
    size: usize,
    user_data: u64,
) -> Rav1gStatus {
    // SAFETY: the caller's contract.
    let Some(decoder) = (unsafe { decoder.as_mut() }) else {
        return Rav1gStatus::InvalidParam;
    };
    if data.is_null() {
        return Rav1gStatus::InvalidParam;
    }
    // SAFETY: the caller's contract.
    let result = decoder.decoder.decode(unsafe { std::slice::from_raw_parts(data, size) }, user_data);
    keep(decoder, result)
}

/// The text of the kind of the last error of the decoder ("Corrupt frame detected", ...); valid until the
/// next call on the decoder.
///
/// # Safety
/// `decoder` is a decoder of `rav1g_decoder_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_error(decoder: *const Rav1gDecoder) -> *const c_char {
    // SAFETY: the caller's contract.
    unsafe { decoder.as_ref() }.map_or(c"".as_ptr(), |decoder| decoder.message.as_ptr())
}

/// What the decoder said about its last error, an empty string when it said nothing; valid until the next
/// call on the decoder.
///
/// # Safety
/// `decoder` is a decoder of `rav1g_decoder_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_error_detail(decoder: *const Rav1gDecoder) -> *const c_char {
    // SAFETY: the caller's contract.
    unsafe { decoder.as_ref() }.map_or(c"".as_ptr(), |decoder| decoder.detail.as_ptr())
}

/// Waits for the frames in flight: every frame that was submitted has been handed to the output, then
/// `Rav1gOutput::end_of_frames` is called. The decoder can go on decoding afterwards.
///
/// # Safety
/// `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_drain(decoder: *mut Rav1gDecoder) {
    // SAFETY: the caller's contract.
    if let Some(decoder) = unsafe { decoder.as_mut() } {
        decoder.decoder.drain();
    }
}

/// The frame of every spatial layer of a temporal unit is output, instead of the highest one.
///
/// # Safety
/// `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_set_output_all_layers(decoder: *mut Rav1gDecoder, enable: bool) -> Rav1gStatus {
    // SAFETY: the caller's contract.
    let Some(decoder) = (unsafe { decoder.as_mut() }) else {
        return Rav1gStatus::InvalidParam;
    };
    let result = decoder.decoder.set_output_all_layers(enable);
    keep(decoder, result)
}

/// The operating point of a stream with layers.
///
/// # Safety
/// `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_set_operating_point(
    decoder: *mut Rav1gDecoder,
    operating_point: u32,
) -> Rav1gStatus {
    // SAFETY: the caller's contract.
    let Some(decoder) = (unsafe { decoder.as_mut() }) else {
        return Rav1gStatus::InvalidParam;
    };
    let result = decoder.decoder.set_operating_point(operating_point);
    keep(decoder, result)
}

/// Waits for the frames in flight, hands them over and releases the decoder. With a budget callback the
/// release is logged step by step ("MEMORY destroy").
///
/// # Safety
/// `decoder` is null or a decoder of `rav1g_decoder_create`, which is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_decoder_destroy(decoder: *mut Rav1gDecoder) {
    if !decoder.is_null() {
        // SAFETY: the caller's contract: the box of rav1g_decoder_create.
        drop(unsafe { Box::from_raw(decoder) });
    }
}

// The process-wide switches: they hold for every decoder of the process and are changed while no frame
// is in flight in any of them (before the first temporal unit, or after a drain).

/// CDEF through `cdef_filter_v2.hlsl` (true, the default) or `cdef_filter.hlsl`.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_cdef_v2(enable: bool) {
    switches::set_cdef_v2(enable);
}

/// Intra prediction through `intra_v2.hlsl`; only in a decoder created with `Rav1gSettings::intra_v2`.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_intra_v2(enable: bool) {
    switches::set_intra_v2(enable);
}

/// The optimised build of the original intra shader.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_intra_opt(enable: bool) {
    switches::set_intra_opt(enable);
}

/// The clears of a pipeline slot on the completion thread instead of the decode thread.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_async_clear(enable: bool) {
    switches::set_async_clear(enable);
}

/// The fast inter path: 0, or `RAV1G_INTER_FAST_ON` with the `RAV1G_INTER_FAST_*` bits; only in a decoder
/// created with `Rav1gSettings::inter_fast`.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_inter_fast(mode: u32) {
    switches::set_inter_fast(mode);
}

/// Experiment: how the horizontal loop filter is dispatched, 0 (one dispatch per plane), 1 or 2.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_lf_h_split(mode: u32) {
    switches::set_lf_h_split(mode);
}

/// Experiment: the 8-bit above edge filter bound of the intra shader.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_intra_edge_fix(enable: bool) {
    switches::set_intra_edge_fix(enable);
}

/// Debugging: the decoders created after the call fill the mode info pool (`RAV1G_POISON_MODE_INFO`) or
/// every buffer the CPU writes for the shaders (`RAV1G_POISON_BUFFERS`) with the byte 0xA5 when they
/// create it; 0 switches it off. The output must be the same.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_poison_buffers(level: u32) {
    switches::set_poison_buffers(match level {
        RAV1G_POISON_MODE_INFO => switches::Poison::ModeInfo,
        RAV1G_POISON_BUFFERS => switches::Poison::Buffers,
        _ => switches::Poison::Off,
    });
}

/// The inter breakdown of the timing records: 0 off, 1 sections, 2 every dispatch alone.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_set_perf_detail(mode: u32) {
    switches::set_perf_detail(mode);
}

/// An empty string for the normal build of the shaders, "uavsync" for the experiment build.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_shader_variant() -> *const c_char {
    // SAFETY: returns a static string.
    unsafe { crate::sys::av1_shader_variant() }
}

/// The timing run: with a callback every decoder takes GPU timestamps and CPU timers per stage and hands
/// one record per command list to the callback, on its completion thread. Null switches the timing off.
///
/// # Safety
/// `callback` is null or a function that may be called from the decoders' threads with `context` until it
/// is replaced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_set_perf_callback(callback: Rav1gPerfCallback, context: *mut c_void) {
    let context = Context(context);
    switches::set_perf_callback(callback.map(|callback| -> switches::PerfCallback {
        Arc::new(move |frame| {
            let context = context;
            // SAFETY: the application's callback with its context, as the caller promised.
            unsafe { callback(context.0, frame) }
        })
    }));
}

/// The stage dumps: the decoders copy the buffers behind every GPU stage of the frames whose numbers (in
/// decoding order, from 0 per decoder) are the `count` values at `frames` and hand them to the callback on
/// their completion thread. Null, or no frames, switches them off.
///
/// # Safety
/// `callback` is null or a function that may be called from the decoders' threads with `context` until it
/// is replaced; `frames` is valid for `count` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_set_stage_dump(
    callback: Rav1gStageDumpCallback,
    context: *mut c_void,
    frames: *const i32,
    count: usize,
) {
    let context = Context(context);
    // SAFETY: the caller's contract.
    let frames = if frames.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(frames, count) } };
    switches::set_stage_dump(
        callback.map(|callback| -> switches::DumpCallback {
            Arc::new(move |dump| {
                let context = context;
                // SAFETY: the application's callback with its context, as the caller promised.
                unsafe { callback(context.0, dump) }
            })
        }),
        frames,
    );
}

/// The log lines of the decoders ("MEMORY ...", errors), from whatever thread a decoder is on. Without a
/// callback the lines go to the debugger output.
///
/// # Safety
/// `callback` is null or a function that may be called from any thread with `context` until it is
/// replaced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_set_log_callback(callback: Rav1gLogCallback, context: *mut c_void) {
    let context = Context(context);
    switches::set_log_callback(callback.map(|callback| -> switches::LogCallback {
        Arc::new(move |is_error, line| {
            let context = context;
            let line = c_string(line);
            // SAFETY: the application's callback with its context, as the caller promised.
            unsafe { callback(context.0, is_error, line.as_ptr()) }
        })
    }));
}

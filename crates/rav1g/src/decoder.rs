// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The decoder: its settings, its creation, a temporal unit in, frames out through [`Output`].

use std::ffi::{c_int, c_void};
use std::ops::BitOr;
use std::sync::Arc;

use windows::core::Interface;

use crate::color::{color_of_image, hdr_of_image};
use crate::device::Device;
use crate::error::{Error, ErrorCode, text};
use crate::host::{self, decoder_own_bytes, frame_fence};
use crate::output::{Frame, ImageFormat, ImageRequest, Output, OutputImage};
use crate::sys;

/// The memory reductions of a decoder, as a set ([`Settings::low_memory`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LowMemory(pub u32);

impl LowMemory {
    /// No reduction.
    pub const NONE: LowMemory = LowMemory(0);
    /// One back buffer for all pipeline slots, and a pool of 10 frame buffers.
    pub const SHARED_BACK_BUFFER: LowMemory = LowMemory(sys::AV1_LOW_MEMORY_SHARED_BACK_BUFFER);
    /// The mode info of a frame in shares that a slot takes as it needs them.
    pub const MODE_INFO: LowMemory = LowMemory(sys::AV1_LOW_MEMORY_MODE_INFO);
    /// The coefficients of a frame in shares, likewise.
    pub const COEFS: LowMemory = LowMemory(sys::AV1_LOW_MEMORY_COEFS);
    pub const ALL: LowMemory = LowMemory(sys::AV1_LOW_MEMORY_ALL);

    fn to_raw(self) -> c_int {
        // 0 in the C configuration is "the default", not "none".
        let bits = self.0 & sys::AV1_LOW_MEMORY_ALL;
        if bits == 0 { sys::AV1_LOW_MEMORY_NONE as c_int } else { bits as c_int }
    }
}

impl Default for LowMemory {
    /// The shared back buffer.
    fn default() -> LowMemory {
        LowMemory::SHARED_BACK_BUFFER
    }
}

impl BitOr for LowMemory {
    type Output = LowMemory;
    fn bitor(self, other: LowMemory) -> LowMemory {
        LowMemory(self.0 | other.0)
    }
}

/// The switches of the parser ([`Settings::parser_options`]): rav1d without the masks of its own
/// loop filter and a tile kept on its thread, which are both on by default, and the calls of the sink
/// timed. The frames are the same with every one.
pub use rav1g_parse::options::ParseOptions as ParserOptions;

/// What the application has left for the decoder, in bytes: its memory limit less what it uses and less
/// what it is going to allocate itself (output images that do not exist yet). `None` when it does not
/// know. Called on the threads that create, use and drop the decoder.
pub type MemoryAvailable = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// What a decoder is created with.
#[derive(Clone)]
pub struct Settings {
    /// The frame size the buffers are made for: the largest frame of the sequence
    /// ([`crate::StreamInfo`]). The decoder rounds both up to a multiple of 128.
    pub width: u32,
    pub height: u32,
    /// 8 for a decoder with 8-bit frame buffers (an 8-bit stream), 10 otherwise.
    pub bit_depth: u32,
    /// The threads that parse: the tiles of a frame and, with more than one frame in parse, the frames;
    /// at most 8. 0 is the default, by the frame size the decoder is created for: 1 up to 2560x1440, 2
    /// above.
    pub threads: u32,
    /// Frames whose parse may run at the same time: 1 to 4, and never more than
    /// the threads. 0 is the default: 2 for a decoder that is created for frames above 2560x1440 with at
    /// least 2 threads and 3 or more pipeline slots, else 1.
    ///
    /// With 1, [`Decoder::decode`] returns when the frames of its data are parsed, recorded and
    /// submitted. With more it returns when their parse has begun; the frames are parsed on the
    /// decoder's threads next to those of the next calls, and a thread of the decoder records and
    /// submits them in decode order. The frames and their order are the same. What an application sees
    /// of it: the error of a frame comes back from a later [`Decoder::decode`] or from
    /// [`Decoder::drain`], and [`Output::get_image`] is called on a thread of the decoder.
    ///
    /// A frame in parse holds a pipeline slot, and takes its mode info, coefficients and transform
    /// block records from pools all frames in flight share. With `frame_slots` 0 a decoder with more
    /// than one frame in parse has one slot more than frames in parse, at most 4; when [`Settings::memory_available`]
    /// says that such a decoder would not leave [`Settings::memory_headroom`], the decoder is created
    /// with fewer slots and frames in parse. [`Decoder::configuration`] says what was created.
    pub frames_in_parse: u32,
    /// Pipeline slots: frames that can be between the start of their CPU decode and the end of their GPU
    /// work at the same time, 2 to 4. 0 is the default: 2 with one frame in parse. Each slot has
    /// buffers of its own.
    pub frame_slots: u32,
    /// The memory reductions.
    pub low_memory: LowMemory,
    /// Buffers for the second intra path ([`crate::switches::set_intra_v2`]).
    pub intra_v2: bool,
    /// Room in the block maps for the lists of the fast inter path ([`crate::switches::set_inter_fast`]).
    pub inter_fast: bool,
    /// 10-bit frames are put out with three samples packed into 32 bits ([`ImageFormat::Packed10x3`]).
    pub packed_10x3: bool,
    /// The operating point of a stream with layers.
    pub operating_point: u32,
    /// The frame of every spatial layer of a temporal unit is output, instead of the highest one.
    pub output_all_layers: bool,
    /// The switches of the parser: ways of doing the same parse. The default has the two on that
    /// make it faster; [`ParserOptions::NONE`] turns them off.
    pub parser_options: ParserOptions,
    /// The budget guard. On a console an application that goes over its memory limit loses its D3D12
    /// device, so with this callback the decoder asks before it allocates: a creation whose device buffers
    /// would leave less than 48 MiB is refused ([`ErrorCode::Memory`], with a "MEMORY REFUSED" line in the
    /// log and in [`Error::detail`]), and so is a palette buffer that would leave less than 16 MiB (a
    /// decode error). Without it nothing is checked.
    pub memory_available: Option<MemoryAvailable>,
    /// With [`Settings::memory_available`]: the bytes a creation should leave of what the callback
    /// returns, behind its device buffers, so that the frames are not slowed by the memory running out
    /// (on the console a 4K decoder that left 77 MiB ran at 19.5 frames a second, one that left 128 at
    /// 45.7). 0 is the default, 128 MiB; a value below 48 MiB is 48 MiB. A decoder that would leave
    /// less is created with fewer slots and frames in parse, never refused for it: one slot less at a
    /// time down to 2, the first that leaves the headroom, or the smallest ("MEMORY headroom" log lines).
    /// [`Decoder::configuration`] says what was created.
    pub memory_headroom: u64,
}

impl Default for Settings {
    /// A decoder for 8-bit 1920x1088 frames with the shared back buffer, whose parser is rav1d's parse
    /// pass with its default switches, and whose threads, frames in parse and slots are the defaults for
    /// its frame size: one thread, one frame in parse and two slots at this size.
    fn default() -> Settings {
        Settings {
            width: 1920,
            height: 1088,
            bit_depth: 8,
            threads: 0,
            frames_in_parse: 0,
            frame_slots: 0,
            low_memory: LowMemory::default(),
            intra_v2: false,
            inter_fast: false,
            packed_10x3: false,
            operating_point: 0,
            output_all_layers: false,
            parser_options: ParserOptions::default(),
            memory_available: None,
            memory_headroom: 0,
        }
    }
}

/// What a decoder was created with, where its settings left the choice to it ([`Decoder::configuration`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Configuration {
    /// The threads that parse ([`Settings::threads`]).
    pub threads: u32,
    /// The frames whose parse may run at the same time ([`Settings::frames_in_parse`]): what the settings
    /// asked for or the default, unless the budget guard found that the slots of it do not fit, and then
    /// 1.
    pub frames_in_parse: u32,
    /// The pipeline slots ([`Settings::frame_slots`]).
    pub frame_slots: u32,
    /// The bytes of the device buffers the creation allocated.
    pub device_size: usize,
}

/// What a decoder of some settings takes.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRequirements {
    /// Bytes of host memory, which the decoder allocates at creation, before it asks the budget guard:
    /// the pool of the entropy decoder and the decoder itself.
    pub host_size: usize,
    /// Bytes of the device buffers the creation allocates: those of the decoder the settings ask for.
    /// The decoder that is created may have fewer slots and take less
    /// ([`Settings::frames_in_parse`], [`Decoder::configuration`]).
    pub device_size: usize,
    /// Bytes of the device buffers that are only created when a stream needs them: one palette buffer per
    /// slot.
    pub device_size_deferred: usize,
}

impl Settings {
    #[allow(clippy::field_reassign_with_default)]
    fn to_raw(&self) -> sys::aom_codec_dec_cfg_t {
        let mut cfg = sys::aom_codec_dec_cfg_t::default();
        cfg.width = self.width;
        cfg.height = self.height;
        cfg.bitdepth = self.bit_depth;
        cfg.threads = self.threads;
        cfg.host_size = 0;
        cfg.tryHDR10x3 = c_int::from(self.packed_10x3);
        cfg.frame_slots = self.frame_slots as c_int;
        cfg.frames_in_parse = self.frames_in_parse as c_int;
        cfg.intra_v2 = c_int::from(self.intra_v2);
        cfg.inter_fast = c_int::from(self.inter_fast);
        cfg.low_memory = self.low_memory.to_raw();
        cfg.parser_options = self.parser_options.bits() as c_int;
        cfg
    }

    /// The memory a decoder of these settings takes.
    pub fn memory_requirements(&self) -> MemoryRequirements {
        let mut cfg = self.to_raw();
        // SAFETY: cfg is a valid configuration; the call reads sizes and writes the three results.
        unsafe { host::av1_query_memory_requirements((&raw mut cfg).cast()) };
        MemoryRequirements {
            // The query gives the host pool; the decoder is an allocation next to it.
            host_size: cfg.host_size + decoder_own_bytes(),
            device_size: cfg.device_size,
            device_size_deferred: cfg.device_size_deferred,
        }
    }
}

/// What the callbacks of the C interface get as their context: it lives in a box of the decoder.
struct Shared {
    output: Arc<dyn Output>,
    memory_available: Option<MemoryAvailable>,
}

unsafe extern "C" fn memory_available(context: *mut c_void) -> i64 {
    // SAFETY: the context is the `Shared` of a decoder that is alive.
    let shared = unsafe { &*context.cast::<Shared>() };
    match shared.memory_available.as_ref().and_then(|available| available()) {
        Some(bytes) => i64::try_from(bytes).unwrap_or(i64::MAX),
        None => -1,
    }
}

unsafe extern "C" fn get_image(
    context: *mut c_void,
    buffer_size: usize,
    width: u16,
    height: u16,
    fb_type: sys::frame_buffer_type,
    fb: *mut sys::av1_decoded_frame_buffer_t,
) -> c_int {
    if context.is_null() || fb.is_null() {
        return -1;
    }
    // SAFETY: the context is the `Shared` of the decoder that calls; `fb` is the decoder's.
    let (shared, fb) = unsafe { (&*context.cast::<Shared>(), &mut *fb) };
    let format = match fb_type {
        sys::fbt10bit => ImageFormat::Bits10,
        sys::fbt10x3 => ImageFormat::Packed10x3,
        _ => ImageFormat::Bits8,
    };
    let request = ImageRequest { buffer_size, width: u32::from(width), height: u32::from(height), format };
    let Some(image) = shared.output.image(&request) else {
        return -1;
    };
    if image.buffer.is_none() && image.textures[0].is_none() {
        shared.output.unused(image);
        return -1;
    }
    let raw = |resource: &Option<_>| resource.as_ref().map_or(std::ptr::null_mut(), Interface::as_raw);
    fb.dx12_buffer = raw(&image.buffer);
    fb.buffer_host_ptr = image.memory.cast();
    fb.buffer_size = image.buffer_size as _;
    fb.dx12_texture = [raw(&image.textures[0]), raw(&image.textures[1]), raw(&image.textures[2])];
    // The image, with the references of its resources, travels with the frame and comes back in
    // `frame_ready` or in `release_image`.
    fb.priv_ = Box::into_raw(Box::new(image)).cast();
    0
}

unsafe extern "C" fn release_image(context: *mut c_void, image: *mut c_void) -> c_int {
    if context.is_null() || image.is_null() {
        return -1;
    }
    // SAFETY: the context is the `Shared` of the decoder; `image` is the box of `get_image`, handed back
    // once.
    let (shared, image) = unsafe { (&*context.cast::<Shared>(), Box::from_raw(image.cast::<OutputImage>())) };
    shared.output.unused(*image);
    0
}

unsafe extern "C" fn frame_ready(context: *mut c_void, image: *mut c_void) -> c_int {
    if context.is_null() {
        return -1;
    }
    // SAFETY: the context is the `Shared` of the decoder.
    let shared = unsafe { &*context.cast::<Shared>() };
    if image.is_null() {
        shared.output.end_of_frames();
        return 0;
    }
    // SAFETY: the decoder hands over an image description whose `fb2_priv` is the box of `get_image`.
    let (img, image) = unsafe {
        let img = &*image.cast::<sys::aom_image_t>();
        (img, Box::from_raw(img.fb2_priv.cast::<OutputImage>()))
    };
    let memory = image.memory as usize;
    let offset = |plane: usize| {
        if img.planes[plane].is_null() { 0 } else { (img.planes[plane] as usize).wrapping_sub(memory) }
    };
    let (mastering_display, content_light) = hdr_of_image(img);
    let frame = Frame {
        width: img.d_w,
        height: img.d_h,
        bit_depth: img.bit_depth,
        monochrome: img.monochrome != 0,
        packed_10x3: img.is_hdr10x3 != 0 && img.bit_depth > 8,
        user_data: img.user_priv as usize as u64,
        color: color_of_image(img),
        mastering_display,
        content_light,
        offsets: [offset(0), offset(1), offset(2)],
        strides: [img.stride[0] as usize, img.stride[1] as usize, img.stride[2] as usize],
        fence: frame_fence(),
        image: *image,
    };
    shared.output.frame(frame);
    0
}

/// A decoder on a [`Device`].
///
/// The order of use:
///
/// 1. [`crate::peek_stream_info`] on the temporal units of the stream until one starts with a key frame:
///    the frame size and bit depth of the sequence.
/// 2. [`Decoder::new`] with [`Settings`] for that sequence and the [`Output`] of the application.
/// 3. [`Decoder::decode`] per temporal unit. The decoder takes an image from the output for every frame
///    that will be shown and hands the frame over when the GPU has written it.
///    [`ErrorCode::NeedsReconfigure`] says that the stream went on with a sequence the decoder does not
///    fit: drain and drop it, create one for the new sequence, pass the temporal unit again.
/// 4. [`Decoder::drain`]: every frame that was submitted has been handed over. Then drop the decoder, or
///    go on decoding.
pub struct Decoder {
    // The decoder of the host; released in `drop`.
    core: *mut host::Av1Core,
    // What the layers of a stream are decoded with (`set_operating_point`, `set_output_all_layers`).
    operating_point: c_int,
    output_all_layers: bool,
    // Handed to the decoder at creation; freed after its release.
    _host_memory: Vec<u8>,
    // The context of the callbacks; freed after the release, which still calls them.
    _shared: Box<Shared>,
    // The device, the queue and the pipeline states the decoder uses.
    device: Device,
}

// SAFETY: the decoder is only used through &mut self; the library's own threads are its business, and
// what they call (`Shared`) is Send + Sync.
unsafe impl Send for Decoder {}

/// The text of a kind of error, as the interface of the entropy decoder had it (`aom_codec_err_to_string`).
fn kind_text(code: sys::aom_codec_err_t) -> &'static str {
    match code {
        sys::AOM_CODEC_OK => "Success",
        sys::AOM_CODEC_ERROR => "Unspecified internal error",
        sys::AOM_CODEC_MEM_ERROR => "Memory allocation error",
        sys::AOM_CODEC_ABI_MISMATCH => "ABI version mismatch",
        sys::AOM_CODEC_INCAPABLE => "Codec does not implement requested capability",
        sys::AOM_CODEC_UNSUP_BITSTREAM => "Bitstream not supported by this decoder",
        sys::AOM_CODEC_UNSUP_FEATURE => "Bitstream required feature not supported by this decoder",
        sys::AOM_CODEC_CORRUPT_FRAME => "Corrupt frame detected",
        sys::AOM_CODEC_INVALID_PARAM => "Invalid parameter",
        sys::AOM_CODEC_LIST_END => "End of iterated list",
        sys::AOM_CODEC_NEEDS_RECONFIGURE => "The sequence needs a decoder created for its frame size and bit depth",
        _ => "Unrecognized error code",
    }
}

/// The error of a call of the host, with what the host said about it.
fn failed(code: sys::aom_codec_err_t, detail: *const std::ffi::c_char) -> Error {
    Error { code: ErrorCode::from_raw(code), message: String::from(kind_text(code)), detail: text(detail) }
}

impl Decoder {
    /// Creates a decoder. Waits for the pipeline states of the device when they are still being created.
    pub fn new(device: &Device, settings: &Settings, output: Arc<dyn Output>) -> Result<Decoder, Error> {
        let mut cfg = settings.to_raw();
        // SAFETY: as in memory_requirements.
        unsafe { host::av1_query_memory_requirements((&raw mut cfg).cast()) };
        let mut host_memory: Vec<u8> = Vec::new();
        if host_memory.try_reserve_exact(cfg.host_size).is_err() {
            return Err(Error::new(
                ErrorCode::Memory,
                format!("decoder creation failed: no memory for the host pool of {} bytes", cfg.host_size),
            ));
        }
        host_memory.resize(cfg.host_size, 0);
        let shared = Box::new(Shared { output, memory_available: settings.memory_available.clone() });
        let context: *mut c_void = std::ptr::from_ref::<Shared>(&shared).cast_mut().cast();
        cfg.host_memory = host_memory.as_mut_ptr().cast();
        cfg.host_size = host_memory.len();
        cfg.out_buffers_cb = sys::av1_out_buffers_cb_t {
            get_out_buffer_cb: Some(get_image),
            release_out_buffer_cb: Some(release_image),
            notify_frame_ready_cb: Some(frame_ready),
            out_buffers_priv: context,
        };
        // Without a callback there is no budget guard: the application has no memory limit to report.
        if settings.memory_available.is_some() {
            cfg.memory_available_cb = Some(memory_available);
            cfg.memory_available_priv = context;
        }
        cfg.memory_headroom = usize::try_from(settings.memory_headroom).unwrap_or(usize::MAX);
        cfg.dx12device = device.d3d12_device().as_raw();
        cfg.dx12command_queue = device.d3d12_queue().as_raw();
        cfg.dxPsos = device.pipelines_ptr();
        let mut core = std::ptr::null_mut();
        // SAFETY: cfg is valid; the device, the queue and the pipeline states are kept by the decoder's
        // `Device`, the host memory and the context of the callbacks by the decoder.
        if unsafe { host::av1_create_gpu_decoder(&mut core, (&raw mut cfg).cast()) } != 0 {
            // The creation has released everything it built. The line that names what failed is the
            // detail.
            return Err(Error {
                code: ErrorCode::Memory,
                message: format!("decoder creation failed: {}", kind_text(sys::AOM_CODEC_MEM_ERROR)),
                detail: text(host::log::av1_last_create_error()),
            });
        }
        Ok(Decoder {
            core,
            operating_point: settings.operating_point as c_int,
            output_all_layers: settings.output_all_layers,
            _host_memory: host_memory,
            _shared: shared,
            device: device.clone(),
        })
    }

    /// The device of the decoder.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Decodes one temporal unit. `user_data` comes back with the frames of the unit
    /// ([`Frame::user_data`]): a presentation time, for one.
    ///
    /// The call returns when the frames of the unit are recorded and submitted to the GPU, not when they
    /// are complete; it waits when every pipeline slot is in flight, and when the output has no image.
    ///
    /// With more than one frame in parse ([`Settings::frames_in_parse`]) it returns when the parse of the
    /// unit's frames has begun, and it waits when as many frames are in parse as may be. An error is
    /// then that of this unit (a header that cannot be read, a frame whose references are missing) or
    /// that of a frame of an earlier call whose parse or submission failed since (when this unit has no error of its own): one error a call,
    /// the oldest first.
    pub fn decode(&mut self, data: &[u8], user_data: u64) -> Result<(), Error> {
        if data.is_empty() {
            return Err(failed(sys::AOM_CODEC_INVALID_PARAM, std::ptr::null()));
        }
        let mut detail = std::ptr::null();
        // SAFETY: data is valid for data.len() bytes for the duration of the call; the user value is
        // carried, never followed; the detail lives until the next call of the decoder.
        let code = unsafe {
            host::rav1g_rav1d_decode(
                self.core,
                data.as_ptr(),
                data.len(),
                user_data as usize as *mut c_void,
                self.operating_point,
                c_int::from(self.output_all_layers),
                &mut detail,
            )
        };
        if code == sys::AOM_CODEC_OK { Ok(()) } else { Err(failed(code, detail)) }
    }

    /// Waits for the frames in flight: every frame whose parse has begun is parsed and submitted, every
    /// frame that was submitted has been handed to the output, then [`Output::end_of_frames`] is called.
    /// The decoder can go on decoding afterwards.
    ///
    /// With one frame in parse the result is always `Ok`. With more ([`Settings::frames_in_parse`]) it is
    /// the error of a frame that was still in parse when the last [`Self::decode`] returned, if one
    /// failed: one error a call, so an application that counts them calls this until it is `Ok`.
    pub fn drain(&mut self) -> Result<(), Error> {
        let mut detail = std::ptr::null();
        // SAFETY: the decoder; the detail lives until the next call of the decoder.
        let code = unsafe {
            host::av1_drain_gpu_decoder(self.core);
            // With more than one frame in parse, a frame that was in parse when the last decode call
            // returned may have failed: the drain says so, one error a call.
            host::rav1g_rav1d_drain(self.core, &mut detail)
        };
        if code == sys::AOM_CODEC_OK { Ok(()) } else { Err(failed(code, detail)) }
    }

    /// What the decoder was created with: the threads, the frames in parse and the slots, with the
    /// defaults of the settings resolved and after the budget guard's choice, and its device buffers.
    pub fn configuration(&mut self) -> Configuration {
        let (mut frames_in_parse, mut threads, mut frame_slots, mut device_size) = (1, 1, 2, 0);
        // SAFETY: the decoder; the four are written.
        unsafe {
            host::av1_decoder_configuration(
                self.core,
                &mut frames_in_parse,
                &mut threads,
                &mut frame_slots,
                &mut device_size,
            );
        }
        Configuration {
            threads: threads.max(1) as u32,
            frames_in_parse: frames_in_parse.max(1) as u32,
            frame_slots: frame_slots.max(0) as u32,
            device_size,
        }
    }

    /// The frame of every spatial layer of a temporal unit, instead of the highest one. From the next
    /// [`Self::decode`] on.
    pub fn set_output_all_layers(&mut self, enable: bool) -> Result<(), Error> {
        self.output_all_layers = enable;
        Ok(())
    }

    /// The operating point of a stream with layers. From the next [`Self::decode`] on; the parser takes
    /// it at the next sequence header.
    pub fn set_operating_point(&mut self, operating_point: u32) -> Result<(), Error> {
        self.operating_point = operating_point as c_int;
        Ok(())
    }
}

impl Drop for Decoder {
    /// Waits for the frames in flight, hands them over and releases the decoder. With a budget callback
    /// the release is logged step by step ("MEMORY destroy").
    fn drop(&mut self) {
        // SAFETY: created by `new`, released once; the host memory and the context of the callbacks are
        // freed after this.
        unsafe { host::av1_destroy_gpu_decoder(self.core) };
    }
}

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
use crate::host::{decoder_own_bytes, frame_fence};
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

/// What reads the bitstream of a decoder ([`Settings::parser`]). Both give the same frames; they differ
/// in speed and in what a damaged stream does (docs/phase2-rav1d.md).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Parser {
    /// The entropy decoder of libaom (C). It stays until the last step of phase 2.
    Libaom,
    /// The parse pass of rav1d. The default.
    #[default]
    Rav1d,
}

/// The switches of [`Parser::Rav1d`] ([`Settings::parser_options`]): rav1d without the masks of its own
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
    /// Tile threads.
    pub threads: u32,
    /// Pipeline slots: frames that can be between the start of their CPU decode and the end of their GPU
    /// work at the same time, 2 to 4. 0 is the default, 2. Each slot has buffers of its own.
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
    /// What reads the bitstream: [`Parser::Rav1d`] unless the application says otherwise.
    pub parser: Parser,
    /// The switches of [`Parser::Rav1d`]: ways of doing the same parse. The default has the two on that
    /// make it faster; [`ParserOptions::NONE`] turns them off.
    pub parser_options: ParserOptions,
    /// The budget guard. On a console an application that goes over its memory limit loses its D3D12
    /// device, so with this callback the decoder asks before it allocates: a creation whose device buffers
    /// would leave less than 48 MiB is refused ([`ErrorCode::Memory`], with a "MEMORY REFUSED" line in the
    /// log and in [`Error::detail`]), and so is a palette buffer that would leave less than 16 MiB (a
    /// decode error). Without it nothing is checked.
    pub memory_available: Option<MemoryAvailable>,
}

impl Default for Settings {
    /// A decoder for 8-bit 1920x1088 frames with one tile thread, two slots and the shared back buffer,
    /// whose parser is rav1d's parse pass with its default switches.
    fn default() -> Settings {
        Settings {
            width: 1920,
            height: 1088,
            bit_depth: 8,
            threads: 1,
            frame_slots: 0,
            low_memory: LowMemory::default(),
            intra_v2: false,
            inter_fast: false,
            packed_10x3: false,
            operating_point: 0,
            output_all_layers: false,
            parser: Parser::default(),
            parser_options: ParserOptions::default(),
            memory_available: None,
        }
    }
}

/// What a decoder of some settings takes.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRequirements {
    /// Bytes of host memory, which the decoder allocates at creation, before it asks the budget guard:
    /// the pool of the entropy decoder and the decoder itself.
    pub host_size: usize,
    /// Bytes of the device buffers the creation allocates.
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
        cfg.cfg.ext_partition = 1;
        // SAFETY: returns a constant of the build.
        cfg.allow_lowbitdepth = unsafe { sys::rav1g_config_lowbitdepth() } as _;
        cfg.host_size = 0;
        cfg.tryHDR10x3 = c_int::from(self.packed_10x3);
        cfg.frame_slots = self.frame_slots as c_int;
        cfg.intra_v2 = c_int::from(self.intra_v2);
        cfg.inter_fast = c_int::from(self.inter_fast);
        cfg.low_memory = self.low_memory.to_raw();
        cfg.parser = match self.parser {
            Parser::Libaom => sys::AV1_PARSER_LIBAOM,
            Parser::Rav1d => sys::AV1_PARSER_RAV1D,
        } as c_int;
        cfg.parser_options = self.parser_options.bits() as c_int;
        cfg
    }

    /// The memory a decoder of these settings takes.
    pub fn memory_requirements(&self) -> MemoryRequirements {
        let mut cfg = self.to_raw();
        // SAFETY: cfg is a valid configuration; the call reads sizes and writes the three results.
        unsafe { sys::av1_codec_query_memory_requirements(&mut cfg) };
        MemoryRequirements {
            // The query of the C interface gives the host pool; the decoder is an allocation next to it.
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
    ctx: Box<sys::aom_codec_ctx_t>,
    // Handed to the decoder at creation; freed after aom_codec_destroy.
    _host_memory: Vec<u8>,
    // The context of the callbacks; freed after aom_codec_destroy, which still calls them.
    _shared: Box<Shared>,
    // The device, the queue and the pipeline states the decoder uses.
    device: Device,
}

// SAFETY: the context is only used through &mut self; the library's own threads are its business, and
// what they call (`Shared`) is Send + Sync.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Creates a decoder. Waits for the pipeline states of the device when they are still being created.
    pub fn new(device: &Device, settings: &Settings, output: Arc<dyn Output>) -> Result<Decoder, Error> {
        let mut cfg = settings.to_raw();
        // SAFETY: as in memory_requirements.
        unsafe { sys::av1_codec_query_memory_requirements(&mut cfg) };
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
        cfg.dx12device = device.d3d12_device().as_raw();
        cfg.dx12command_queue = device.d3d12_queue().as_raw();
        cfg.dxPsos = device.pipelines_ptr();
        let mut ctx = Box::new(sys::aom_codec_ctx_t::default());
        // SAFETY: ctx and cfg are valid; the device, the queue and the pipeline states are kept by the
        // decoder's `Device`, the host memory and the context of the callbacks by the decoder.
        let code = unsafe { sys::rav1g_codec_dec_init(&mut *ctx, &cfg, 0) };
        if code != sys::AOM_CODEC_OK {
            // SAFETY: a context that failed to initialise still answers the error calls.
            let (message, detail) =
                unsafe { (text(sys::aom_codec_error(&mut *ctx)), text(sys::aom_codec_error_detail(&mut *ctx))) };
            return Err(Error {
                code: ErrorCode::from_raw(code),
                message: format!("decoder creation failed: {}", if message.is_empty() { "?" } else { &message }),
                detail,
            });
        }
        let mut decoder = Decoder { ctx, _host_memory: host_memory, _shared: shared, device: device.clone() };
        decoder.set_output_all_layers(settings.output_all_layers)?;
        decoder.set_operating_point(settings.operating_point)?;
        Ok(decoder)
    }

    /// The device of the decoder.
    pub fn device(&self) -> &Device {
        &self.device
    }

    fn check(&mut self, code: sys::aom_codec_err_t) -> Result<(), Error> {
        if code == sys::AOM_CODEC_OK {
            return Ok(());
        }
        // SAFETY: the context is valid.
        unsafe {
            Err(Error {
                code: ErrorCode::from_raw(code),
                message: text(sys::aom_codec_error(&mut *self.ctx)),
                detail: text(sys::aom_codec_error_detail(&mut *self.ctx)),
            })
        }
    }

    /// Decodes one temporal unit. `user_data` comes back with the frames of the unit
    /// ([`Frame::user_data`]): a presentation time, for one.
    ///
    /// The call returns when the frames of the unit are recorded and submitted to the GPU, not when they
    /// are complete; it waits when every pipeline slot is in flight, and when the output has no image.
    pub fn decode(&mut self, data: &[u8], user_data: u64) -> Result<(), Error> {
        // SAFETY: data is valid for data.len() bytes for the duration of the call; the user value is
        // carried, never followed.
        let code = unsafe {
            sys::aom_codec_decode(&mut *self.ctx, data.as_ptr(), data.len(), user_data as usize as *mut c_void)
        };
        self.check(code)
    }

    /// Waits for the frames in flight: every frame that was submitted has been handed to the output, then
    /// [`Output::end_of_frames`] is called. The decoder can go on decoding afterwards.
    pub fn drain(&mut self) {
        // SAFETY: a NULL buffer of size 0 is the drain call of the interface.
        unsafe { sys::aom_codec_decode(&mut *self.ctx, std::ptr::null(), 0, std::ptr::null_mut()) };
    }

    /// The frame of every spatial layer of a temporal unit, instead of the highest one.
    pub fn set_output_all_layers(&mut self, enable: bool) -> Result<(), Error> {
        // SAFETY: the context is valid.
        let code = unsafe { sys::rav1g_codec_set_output_all_layers(&mut *self.ctx, c_int::from(enable)) };
        self.check(code)
    }

    /// The operating point of a stream with layers.
    pub fn set_operating_point(&mut self, operating_point: u32) -> Result<(), Error> {
        // SAFETY: the context is valid.
        let code = unsafe { sys::rav1g_codec_set_operating_point(&mut *self.ctx, operating_point as c_int) };
        self.check(code)
    }
}

impl Drop for Decoder {
    /// Waits for the frames in flight, hands them over and releases the decoder. With a budget callback
    /// the release is logged step by step ("MEMORY destroy").
    fn drop(&mut self) {
        // SAFETY: created by aom_codec_dec_init, destroyed once; the host memory and the context of the
        // callbacks are freed after this.
        unsafe { sys::aom_codec_destroy(&mut *self.ctx) };
    }
}

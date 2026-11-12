// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! rav1g: an AV1 decoder whose reconstruction runs in D3D12 compute shaders.
//!
//! Step 1 of the Rust port (`docs/rust-port.md`): this crate is today's interface of the decoder, the
//! `aom_codec_*` calls and the D3D12 additions of `libav1/aom/aom_decoder.h`, behind types that own what
//! they create. Nothing is decided here that the C interface does not decide; the safe `Decoder` of the
//! design (frames as D3D12 resources with fences, a C ABI) is step 5. The raw interface is [`sys`].
//!
//! The order of use is the one of the test player and of `rav1g-dec`:
//!
//! 1. [`PipelineCache::new`] on the D3D12 device, [`PipelineCache::wait`]: the compute pipeline states.
//! 2. [`peek_stream_info`] on the temporal units of the stream until one starts with a key frame.
//! 3. [`Decoder::new`] with a [`Config`] for the frame size and bit depth of that sequence.
//! 4. [`Decoder::decode`] per temporal unit; frames come back through the callbacks of the configuration.
//!    [`ErrorCode::NEEDS_RECONFIGURE`] says that the stream went on with a sequence the decoder does not
//!    fit: drain and drop the decoder, create one for the new sequence, pass the temporal unit again.
//! 5. [`Decoder::drain`], then drop the decoder.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::ptr::NonNull;

pub use rav1g_sys as sys;

/// An `aom_codec_err_t`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ErrorCode(pub sys::aom_codec_err_t);

impl ErrorCode {
    /// A new sequence whose frames are larger, or have more bits a sample, than the decoder was created
    /// for. Nothing of the temporal unit was decoded.
    pub const NEEDS_RECONFIGURE: ErrorCode = ErrorCode(sys::AOM_CODEC_NEEDS_RECONFIGURE);
}

/// A call of the decoder that failed: the code, and what `aom_codec_error` and `aom_codec_error_detail`
/// said about it.
#[derive(Clone, Debug)]
pub struct Error {
    pub code: ErrorCode,
    /// `aom_codec_error`: the text of the code.
    pub message: String,
    /// `aom_codec_error_detail`: what the decoder added, empty when it added nothing.
    pub detail: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.message.is_empty() { "?" } else { &self.message })?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

fn text(pointer: *const c_char) -> String {
    if pointer.is_null() {
        String::new()
    } else {
        // SAFETY: the decoder returns NUL-terminated strings that live as long as its context.
        unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned()
    }
}

/// What `aom_codec_peek_stream_info` reads from the sequence header of a temporal unit.
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamInfo {
    /// Largest frame width of the sequence.
    pub width: u32,
    /// Largest frame height of the sequence.
    pub height: u32,
    /// The temporal unit starts with a key frame.
    pub is_key_frame: bool,
    /// Bits a sample: 8, 10 or 12; 0 when the temporal unit has no sequence header.
    pub bit_depth: u32,
}

/// Reads the sequence header of a temporal unit without a decoder.
pub fn peek_stream_info(data: &[u8]) -> Result<StreamInfo, ErrorCode> {
    let mut si = sys::aom_codec_stream_info_t::default();
    // SAFETY: data is a valid buffer of data.len() bytes and si a valid aom_codec_stream_info_t.
    let code = unsafe { sys::rav1g_codec_peek_stream_info(data.as_ptr(), data.len(), &mut si) };
    if code != sys::AOM_CODEC_OK {
        return Err(ErrorCode(code));
    }
    Ok(StreamInfo { width: si.w, height: si.h, is_key_frame: si.is_kf != 0, bit_depth: si.bit_depth })
}

/// The compute pipeline states of the decoder on one D3D12 device (`av1_create_pipeline_cache_handle`).
/// Decoders on the device share them; they must outlive every decoder that was given them.
pub struct PipelineCache(NonNull<c_void>);

// SAFETY: the handle is used by the library from several threads; it has its own locks.
unsafe impl Send for PipelineCache {}
unsafe impl Sync for PipelineCache {}

impl PipelineCache {
    /// Starts creating the pipeline states on `threads` threads. `None` when the library refuses.
    ///
    /// # Safety
    /// `device` is a valid `ID3D12Device*` that outlives the cache.
    pub unsafe fn new(device: *mut c_void, threads: i32) -> Option<PipelineCache> {
        // SAFETY: the caller's contract.
        NonNull::new(unsafe { sys::av1_create_pipeline_cache_handle(device, threads) }).map(PipelineCache)
    }

    /// Waits until every pipeline state exists. False when one could not be created.
    pub fn wait(&self) -> bool {
        // SAFETY: the handle is valid until drop.
        unsafe { sys::av1_wait_pipeline_cache_handle(self.0.as_ptr()) == 0 }
    }

    /// The handle, for [`Config::pipeline_cache`].
    pub fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for PipelineCache {
    fn drop(&mut self) {
        // SAFETY: created by av1_create_pipeline_cache_handle, destroyed once.
        unsafe { sys::av1_destroy_pipeline_cache_handle(self.0.as_ptr()) }
    }
}

/// `av1_shader_variant`: empty for the normal build of the shaders, `"uavsync"` for the experiment build.
pub fn shader_variant() -> String {
    // SAFETY: returns a static string.
    text(unsafe { sys::av1_shader_variant() })
}

/// What a decoder is created with: the fields of `aom_codec_dec_cfg_t` an application sets.
#[derive(Clone, Copy)]
pub struct Config {
    /// Frame width the buffers are sized for.
    pub width: u32,
    /// Frame height the buffers are sized for.
    pub height: u32,
    /// 8 for a decoder with 8-bit frame buffers, 10 otherwise.
    pub bit_depth: u32,
    /// Tile threads.
    pub threads: u32,
    /// Pipeline slots, 2 to 4.
    pub frame_slots: i32,
    /// Buffers for the intra_v2 path.
    pub intra_v2: bool,
    /// Room in the block maps for the lists of the fast inter path.
    pub inter_fast: bool,
    /// `AV1_LOW_MEMORY_*` parts; `AV1_LOW_MEMORY_NONE` for none, 0 for the default.
    pub low_memory: i32,
    /// The output buffer callbacks and their context (`av1_out_buffers_cb_t`).
    pub out_buffers: sys::av1_out_buffers_cb_t,
    /// `ID3D12Device*`.
    pub device: *mut c_void,
    /// `ID3D12CommandQueue*`.
    pub command_queue: *mut c_void,
    /// [`PipelineCache::as_ptr`].
    pub pipeline_cache: *mut c_void,
}

/// What `av1_codec_query_memory_requirements` says a decoder of a configuration takes.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRequirements {
    /// Bytes of host memory the decoder is handed at creation.
    pub host_size: usize,
    /// Bytes of device buffers the decoder creates.
    pub device_size: usize,
}

impl Config {
    #[allow(clippy::field_reassign_with_default)]
    fn to_raw(self) -> sys::aom_codec_dec_cfg_t {
        let mut cfg = sys::aom_codec_dec_cfg_t::default();
        cfg.width = self.width;
        cfg.height = self.height;
        cfg.bitdepth = self.bit_depth;
        cfg.threads = self.threads;
        cfg.cfg.ext_partition = 1;
        // SAFETY: returns a constant of the build.
        cfg.allow_lowbitdepth = unsafe { sys::rav1g_config_lowbitdepth() } as _;
        cfg.out_buffers_cb = self.out_buffers;
        cfg.host_size = 0;
        cfg.tryHDR10x3 = 0;
        cfg.frame_slots = self.frame_slots;
        cfg.intra_v2 = self.intra_v2 as c_int;
        cfg.inter_fast = self.inter_fast as c_int;
        cfg.low_memory = self.low_memory;
        // No budget guard: the caller has no app memory limit to report.
        cfg.memory_available_cb = None;
        cfg.memory_available_priv = std::ptr::null_mut();
        cfg
    }

    /// The memory a decoder of this configuration takes.
    pub fn memory_requirements(&self) -> MemoryRequirements {
        let mut cfg = self.to_raw();
        // SAFETY: cfg is a valid configuration; the call only reads sizes and writes host_size, device_size.
        unsafe { sys::av1_codec_query_memory_requirements(&mut cfg) };
        MemoryRequirements { host_size: cfg.host_size, device_size: cfg.device_size }
    }
}

/// A decoder (`aom_codec_ctx_t` of `aom_codec_av1_dx`) and the host memory it was given.
pub struct Decoder {
    ctx: Box<sys::aom_codec_ctx_t>,
    // Handed to the decoder at creation; freed after aom_codec_destroy.
    _host_memory: Vec<u8>,
}

// SAFETY: the context is only used through &mut self; the library's own threads are its business.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Creates a decoder. The error is a text: what failed and the decoder's message.
    ///
    /// # Safety
    /// The device, the command queue and the pipeline cache of `config` are valid and outlive the decoder.
    /// The callbacks of `config.out_buffers` and their context are valid until the decoder is dropped, and
    /// are called from the decoder's threads.
    pub unsafe fn new(config: &Config) -> Result<Decoder, String> {
        let mut cfg = config.to_raw();
        // SAFETY: as in memory_requirements.
        unsafe { sys::av1_codec_query_memory_requirements(&mut cfg) };
        let mut host_memory: Vec<u8> = Vec::new();
        if host_memory.try_reserve_exact(cfg.host_size).is_err() {
            return Err(format!("no memory for the host pool of {} bytes", cfg.host_size));
        }
        host_memory.resize(cfg.host_size, 0);
        cfg.host_memory = host_memory.as_mut_ptr().cast();
        cfg.host_size = host_memory.len();
        cfg.dx12device = config.device;
        cfg.dx12command_queue = config.command_queue;
        cfg.dxPsos = config.pipeline_cache;
        let mut ctx = Box::new(sys::aom_codec_ctx_t::default());
        // SAFETY: ctx and cfg are valid; the pointers of cfg are the caller's contract.
        if unsafe { sys::rav1g_codec_dec_init(&mut *ctx, &cfg, 0) } != sys::AOM_CODEC_OK {
            // SAFETY: a context that failed to initialise still answers the error calls.
            let (message, detail) =
                unsafe { (text(sys::aom_codec_error(&mut *ctx)), text(sys::aom_codec_error_detail(&mut *ctx))) };
            let mut why = format!("decoder creation failed: {}", if message.is_empty() { "?" } else { &message });
            if !detail.is_empty() {
                why.push_str(": ");
                why.push_str(&detail);
            }
            return Err(why);
        }
        Ok(Decoder { ctx, _host_memory: host_memory })
    }

    fn error(&mut self, code: sys::aom_codec_err_t) -> Error {
        // SAFETY: the context is valid.
        unsafe {
            Error {
                code: ErrorCode(code),
                message: text(sys::aom_codec_error(&mut *self.ctx)),
                detail: text(sys::aom_codec_error_detail(&mut *self.ctx)),
            }
        }
    }

    fn check(&mut self, code: sys::aom_codec_err_t) -> Result<(), Error> {
        if code == sys::AOM_CODEC_OK { Ok(()) } else { Err(self.error(code)) }
    }

    /// Decodes one temporal unit. Frames are handed over through the callbacks of the configuration.
    pub fn decode(&mut self, data: &[u8]) -> Result<(), Error> {
        // SAFETY: data is valid for data.len() bytes for the duration of the call.
        let code = unsafe { sys::aom_codec_decode(&mut *self.ctx, data.as_ptr(), data.len(), std::ptr::null_mut()) };
        self.check(code)
    }

    /// `aom_codec_decode(ctx, NULL, 0, NULL)`: waits for the frames in flight and joins the decoder's GPU
    /// completion thread, so that every frame that was submitted has been handed over.
    pub fn drain(&mut self) {
        // SAFETY: a NULL buffer of size 0 is the drain call of the interface.
        unsafe { sys::aom_codec_decode(&mut *self.ctx, std::ptr::null(), 0, std::ptr::null_mut()) };
    }

    /// `AV1D_SET_OUTPUT_ALL_LAYERS`: the frame of every spatial layer of a temporal unit, instead of the
    /// highest one.
    pub fn set_output_all_layers(&mut self, enable: bool) -> Result<(), Error> {
        // SAFETY: the context is valid.
        let code = unsafe { sys::rav1g_codec_set_output_all_layers(&mut *self.ctx, enable as c_int) };
        self.check(code)
    }

    /// `AV1D_SET_OPERATING_POINT`.
    pub fn set_operating_point(&mut self, operating_point: i32) -> Result<(), Error> {
        // SAFETY: the context is valid.
        let code = unsafe { sys::rav1g_codec_set_operating_point(&mut *self.ctx, operating_point) };
        self.check(code)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by aom_codec_dec_init, destroyed once; the host memory is freed after this.
        unsafe { sys::aom_codec_destroy(&mut *self.ctx) };
    }
}

/// The process-wide switches of the host (`libav1/dx/av1_perf.h`): which shaders a stage uses, the test
/// instrumentation. They are globals of the library, read by its threads.
///
/// # Safety
/// Every function here must be called while no frame is in flight in any decoder of the process, as the
/// test player does at the start of a task. A callback and its context must stay valid until it is
/// replaced the same way.
#[allow(clippy::missing_safety_doc)] // one contract for the module, above
pub mod switches {
    use super::sys;
    use std::ffi::{c_int, c_void};

    /// CDEF through `cdef_filter_v2.hlsl` (true, the player's default) or `cdef_filter.hlsl`.
    pub unsafe fn set_cdef_v2(enable: bool) {
        unsafe { sys::av1_set_cdef_v2(enable as c_int) }
    }
    /// Intra prediction through `intra_v2.hlsl`; the decoder must have been created with `intra_v2`.
    pub unsafe fn set_intra_v2(enable: bool) {
        unsafe { sys::av1_set_intra_v2(enable as c_int) }
    }
    /// The optimised build of the original intra shader.
    pub unsafe fn set_intra_opt(enable: bool) {
        unsafe { sys::av1_set_intra_opt(enable as c_int) }
    }
    /// The slot's clears on the completion thread.
    pub unsafe fn set_async_clear(enable: bool) {
        unsafe { sys::av1_set_async_clear(enable as c_int) }
    }
    /// How the horizontal loop filter is dispatched: 0, 1 or 2.
    pub unsafe fn set_lf_h_split(mode: i32) {
        unsafe { sys::av1_set_lf_h_split(mode) }
    }
    /// The 8-bit above edge filter bound of the intra shader.
    pub unsafe fn set_intra_edge_fix(enable: bool) {
        unsafe { sys::av1_set_intra_edge_fix(enable as c_int) }
    }
    /// Debugging: the decoders created after the call fill the mode info pool (`AV1_POISON_MODE_INFO`) or
    /// every buffer the CPU writes for the shaders (`AV1_POISON_BUFFERS`) with the byte 0xA5 when they
    /// create it, instead of leaving what the memory held. With `AV1_POISON_BUFFERS` they also fill the
    /// constants of every stage again in every frame, right before the stage writes them. The output must be
    /// the same. 0: off, the default.
    pub unsafe fn set_poison_buffers(level: i32) {
        unsafe { sys::av1_set_poison_buffers(level) }
    }
    /// The fast inter path: `AV1_INTER_FAST_*` bits.
    pub unsafe fn set_inter_fast(mode: i32) {
        unsafe { sys::av1_set_inter_fast(mode) }
    }
    /// The per-frame timing records of `-gputime`.
    pub unsafe fn set_perf_callback(callback: sys::av1_perf_frame_cb_fn_t, context: *mut c_void) {
        unsafe { sys::av1_set_perf_callback(callback, context) }
    }
    /// The inter breakdown of the timing records: 0 off, 1 sections, 2 every dispatch alone.
    pub unsafe fn set_perf_detail(mode: i32) {
        unsafe { sys::av1_set_perf_detail(mode) }
    }
    /// The stage dumps of `-dumpstages` for the frames of `frames`; `None` switches them off.
    pub unsafe fn set_stage_dump(callback: sys::av1_stage_dump_cb_fn_t, context: *mut c_void, frames: &[i32]) {
        unsafe {
            if callback.is_some() && !frames.is_empty() {
                sys::av1_set_stage_dump(callback, context, frames.as_ptr(), frames.len() as c_int)
            } else {
                sys::av1_set_stage_dump(None, std::ptr::null_mut(), std::ptr::null(), 0)
            }
        }
    }
    /// The log lines of the host.
    pub unsafe fn set_log_callback(callback: sys::av1_log_cb_fn_t, context: *mut c_void) {
        unsafe { sys::av1_set_log_callback(callback, context) }
    }
}

// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What the stages share: the structs of the frame pipeline as a stage uses them, the constants of a
//! stage, and the timestamps and stage dumps a stage asks for (`perf.rs`, `stage_dump.rs`).
//!
//! The functions here turn the command list, the buffers and the pipelines of the decoder's structs into
//! the types of the GPU backend boundary (`crate::gpu`).

use std::ffi::{CStr, c_int};

use windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList;

use super::memory::poison_constants;
use super::types::{Av1Core, ComputeCommandBuffer, ComputeShader, GpuBufferObject, av1_stage_dump_region};
use super::{perf, stage_dump};
use crate::gpu::d3d12::com_ref;
use crate::gpu::{BufferLocation, BufferRef, Commands, LayoutRef, PipelineRef};
use crate::sys::parser::{AV1_COMMON, HwFrameBuffer, HwOutputImage, av1_perf_inter_detail};

/// The recording of the frame the decoder is building.
///
/// # Safety
/// `dec` is the decoder while it records a frame; its command list outlives the result.
pub(crate) unsafe fn commands<'a>(dec: *const Av1Core) -> Commands<'a> {
    // SAFETY: the COM pointer of the decoder's command list (the caller's contract).
    let list = unsafe { com_ref::<_, ID3D12GraphicsCommandList>(&(*dec).compute.command_list) };
    Commands::new(list.expect("the decoder has a command list"))
}

/// A buffer of the decoder.
///
/// # Safety
/// `object` is a buffer of the decoder's allocator that outlives the result.
pub(crate) unsafe fn buffer<'a>(object: *const GpuBufferObject) -> BufferRef<'a> {
    // SAFETY: the descriptor holds the resource (the caller's contract).
    unsafe { BufferRef::from_raw(&(*object).dev) }.expect("the buffer was created")
}

/// A pipeline of the shader library.
///
/// # Safety
/// `shader` is a field of the library of a decoder; the library outlives the result.
pub(crate) unsafe fn pipeline<'a>(shader: *const ComputeShader) -> PipelineRef<'a> {
    // SAFETY: the COM pointers of a created pipeline (the caller's contract).
    unsafe { PipelineRef::from_com(&(*shader).pso, &(*shader).signaturePtr) }.expect("the pipeline was created")
}

/// A layout of the shader library.
///
/// # Safety
/// `signature` is a root signature field of the library of a decoder; the library outlives the result.
pub(crate) unsafe fn layout<'a, S: 'a>(signature: *const S) -> LayoutRef<'a> {
    // SAFETY: the COM pointer of a created layout (the caller's contract).
    unsafe { LayoutRef::from_com(&*signature) }.expect("the layout was created")
}

/// The constants of one record of a stage: where the CPU writes them and where a shader finds them.
pub(crate) struct Constants<T> {
    /// The record in the upload memory. Not cleared: it holds what an earlier frame wrote at its place
    /// (or the byte of -poisonbuf), so every field a shader reads has to be written.
    pub data: *mut T,
    pub location: BufferLocation,
}

/// `ComputeCommandBuffer::Alloc` of the C++: the next piece of the constant memory of the frame's command
/// buffer, `size` bytes rounded up to 256.
///
/// # Safety
/// `cb` is the command buffer of the frame that is being recorded, by the thread that records it.
pub(crate) unsafe fn alloc_constant_bytes(cb: &mut ComputeCommandBuffer, size: usize) -> Constants<u8> {
    let size = (size + 255) & !255;
    // SAFETY: the constant memory of the command buffer, mapped (the caller's contract); the piece is
    // inside it, as asserted.
    unsafe {
        let memory = &*cb.cb_alloc;
        assert!(cb.cb_ptr + size < memory.size, "the constant memory of a frame is full");
        let at = cb.cb_ptr;
        cb.cb_ptr += size;
        let data = memory.host_ptr.cast::<u8>().add(at);
        poison_constants(data.cast(), size);
        Constants { data, location: buffer(cb.cb_alloc).location().offset(at as u64) }
    }
}

/// The constants of a record of the type `T`.
///
/// # Safety
/// As for [`alloc_constant_bytes`]. `T` is a struct of 32-bit words.
pub(crate) unsafe fn alloc_constants<T>(cb: &mut ComputeCommandBuffer) -> Constants<T> {
    // SAFETY: the caller's contract; a piece starts at a multiple of 256 bytes.
    let bytes = unsafe { alloc_constant_bytes(cb, size_of::<T>()) };
    Constants { data: bytes.data.cast(), location: bytes.location }
}

/// `av1_perf_gpu_mark`: a timestamp behind a stage (nothing without a timing callback).
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn perf_gpu_mark(dec: *mut Av1Core, stage: impl TryInto<c_int>) {
    let Ok(stage) = stage.try_into() else { unreachable!("a stage number") };
    // SAFETY: the caller's contract.
    unsafe { perf::perf_gpu_mark(dec, stage) };
}

/// `av1_stage_dump_residuals`: -dumpstages, the residual buffer behind a stage.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn stage_dump_residuals(dec: *mut Av1Core, stage: &'static CStr) {
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_residuals(dec, stage) };
}

/// `av1_perf_ticks`: the start of a CPU timer; 0 while timing is off.
pub(crate) fn perf_ticks() -> i64 {
    perf::av1_perf_ticks()
}

/// `av1_perf_add`: adds the time since `start_ticks` to a CPU timer of the frame (nothing for a start of
/// 0).
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn perf_add(dec: *mut Av1Core, id: impl TryInto<c_int>, start_ticks: i64) {
    let Ok(id) = id.try_into() else { unreachable!("a timer number") };
    // SAFETY: the caller's contract.
    unsafe { perf::perf_add(dec, id as usize, start_ticks) };
}

/// `av1_stage_dump_active`: -dumpstages wants the frame of the current command list.
///
/// # Safety
/// `dec` is the decoder.
pub(crate) unsafe fn stage_dump_active(dec: *const Av1Core) -> bool {
    // SAFETY: the caller's contract.
    unsafe { (*dec).curr_frame_data.as_ref().is_some_and(|td| td.dump_active != 0) }
}

/// `av1_stage_dump_fb`: -dumpstages, a frame buffer behind a stage, in its current layout. `dims`: the
/// buffer whose visible size the copy has when `fb` is a back buffer that only has a layout; null: `fb`.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `fb` and `dims` are frame buffers of it.
pub(crate) unsafe fn stage_dump_fb(
    dec: *mut Av1Core,
    stage: &'static CStr,
    fb: *const HwFrameBuffer,
    dims: *const HwFrameBuffer,
) {
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_fb(dec, stage, fb, dims) };
}

/// `av1_stage_dump_output`: -dumpstages, the output image as it lies in the back buffer.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `img` is the image of the frame.
pub(crate) unsafe fn stage_dump_output(
    dec: *mut Av1Core,
    stage: &'static CStr,
    img: *const HwOutputImage,
    size: usize,
) {
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_output(dec, stage, img, size) };
}

/// `av1_stage_dump_cb_region`: the region of a record of constants of the frame's command buffer.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `constants` are of that frame.
pub(crate) unsafe fn stage_dump_constants<T>(
    dec: *const Av1Core,
    constants: &Constants<T>,
    size: usize,
) -> av1_stage_dump_region {
    // SAFETY: the caller's contract: the record lies in the constant memory of the frame.
    unsafe {
        let memory = &*(*(*dec).curr_frame_data).command_buffer.cb_alloc;
        let offset = constants.data.cast::<u8>().offset_from(memory.host_ptr.cast::<u8>()) as usize;
        av1_stage_dump_region { src: memory.dev, offset, size, uav: 0 }
    }
}

/// `av1_stage_dump_data`: -dumpstages, the buffers a stage reads besides the frame. The regions are
/// copied one behind the other into one dump; `layout` is the text that describes them, cut to what the
/// C++ had room for (`limit` bytes with its end).
///
/// # Safety
/// `dec` is the decoder while it records a frame; the regions are of buffers of it.
pub(crate) unsafe fn stage_dump_data(
    dec: *mut Av1Core,
    stage: &'static CStr,
    layout: &str,
    limit: usize,
    regions: &[av1_stage_dump_region],
) {
    let mut end = layout.len().min(limit - 1);
    while !layout.is_char_boundary(end) {
        end -= 1;
    }
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_data(dec, stage, &layout[..end], regions) };
}

/// `av1_perf_inter_begin`: the inter breakdown of the frame of the current command list
/// (-gputimedetail), cleared, or null when the frame has none; the stage fills its dispatch counters.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn perf_inter_begin(dec: *mut Av1Core) -> *mut av1_perf_inter_detail {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_inter_begin(dec) }
}

/// `av1_perf_inter_mark`: a timestamp for a section of the inter breakdown (and for its dispatch `list`,
/// or -1), counted to `stage`.
///
/// # Safety
/// `dec` is the decoder while it records a frame with a breakdown.
pub(crate) unsafe fn perf_inter_mark(dec: *mut Av1Core, stage: c_int, section: c_int, list: c_int) {
    // SAFETY: the caller's contract.
    unsafe { perf::perf_inter_mark(dec, stage, section, list) };
}

/// `av1_stage_dump_mode_info`: -dumpstages, the frame's mode info as the stages read it: the grid, and
/// the records of the frame's tiles.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's.
pub(crate) unsafe fn stage_dump_mode_info(dec: *mut Av1Core, cm: *mut AV1_COMMON) {
    // SAFETY: the caller's contract.
    unsafe { stage_dump::stage_dump_mode_info(dec, cm) };
}

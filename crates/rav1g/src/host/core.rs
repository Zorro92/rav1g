// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! What the stages share: the structs of the C++ frame pipeline as a stage uses them, and the functions
//! of `av1_core.cpp` a stage calls.
//!
//! Until step 4 the frame pipeline is C++: it owns `Av1Core`, the data of the pipeline slots, the command
//! list and the buffers, and calls a stage with pointers to them. The functions here turn those into the
//! types of the GPU backend boundary (`crate::gpu`) and hand out the constants of a stage.

use std::ffi::{CStr, c_int};

use windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList;

use super::memory::rav1g_poison_constants;
use crate::gpu::d3d12::com_ref;
use crate::gpu::{BufferLocation, BufferRef, Commands, LayoutRef, PipelineRef};
use crate::sys::host::{Av1Core, ComputeCommandBuffer, ComputeShader, GpuBufferObject};

// The functions of av1_core.cpp the stages call, through crates/rav1g-sys/glue/av1_core_calls.cpp.
unsafe extern "C" {
    fn rav1g_core_perf_gpu_mark(dec: *mut Av1Core, stage: c_int);
    fn rav1g_core_stage_dump_residuals(dec: *mut Av1Core, stage: *const std::ffi::c_char);
}

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
        rav1g_poison_constants(data.cast(), size);
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
    unsafe { rav1g_core_perf_gpu_mark(dec, stage) };
}

/// `av1_stage_dump_residuals`: -dumpstages, the residual buffer behind a stage.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn stage_dump_residuals(dec: *mut Av1Core, stage: &'static CStr) {
    // SAFETY: the caller's contract.
    unsafe { rav1g_core_stage_dump_residuals(dec, stage.as_ptr()) };
}

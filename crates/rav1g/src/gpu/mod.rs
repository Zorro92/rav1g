// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The GPU backend boundary (`docs/rust-port.md`, Requirements): what the host modules ask of a GPU.
//!
//! The host modules (`crate::host`) do not call an API of a GPU themselves. They use the types of this
//! module, which a backend provides under the same names with the same methods:
//!
//! - [`Device`]: creates buffers ([`MemoryKind`] says who reads and writes one), pipeline layouts
//!   ([`LayoutDesc`]) and compute pipelines from the bytecode of a shader, fills buffers of constant data
//!   ([`Device::upload`], a blocking copy on a [`Queue`]), and reports the video memory of its adapter.
//!   [`OwnedDevice`] is the device for another thread.
//! - `Buffer`, [`BufferRef`]: a buffer the caller owns, and a buffer of the decoder where it is used.
//! - [`Layout`], `Pipeline`: what a dispatch needs besides its buffers; [`LayoutRef`] and [`PipelineRef`]
//!   are those of the decoder where they are used.
//! - [`Commands`]: the recording of a frame's commands: the layout and pipeline of the next dispatches,
//!   the buffers ([`BufferLocation`]: a place in a buffer) and constants at the binding slots of the
//!   layout, dispatches, barriers between dispatches that write and read the same buffer, state
//!   transitions ([`BufferState`]) and copies, between buffers and from a buffer into a texture of the
//!   application ([`TextureRef`], [`TextureSource`]).
//!
//! A binding slot is an index into the bindings of a layout in the order of [`LayoutDesc`]: the buffers
//! the shader reads, the buffers it writes, the constant blocks, then the inline constants. D3D12 has
//! them as the root parameters of a root signature; a backend with descriptor sets has them as the
//! bindings of one set, with the inline constants as push constants.
//!
//! There is one backend, D3D12 (`d3d12`). Fences, timestamp queries and readback are in the C++ of the
//! frame pipeline (`av1_core.cpp`) and join this interface with step 4.

pub(crate) mod d3d12;

pub use d3d12::{
    BufferLocation, BufferRef, CommandAllocator, CommandList, Commands, Device, Event, Fence, Layout, LayoutRef,
    OwnedDevice, PipelineRef, Queue, Readback, TextureRef, TimestampQueries, Upload, create_hardware_device,
};

/// Who reads and writes a buffer. The numbers are the `MemoryType` of the C++ host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum MemoryKind {
    /// Written and read by shaders only.
    DeviceOnly = 0,
    /// Written by the CPU through a mapping, read by shaders.
    DeviceUpload = 1,
    /// Read and written by the CPU through a cached mapping, read by shaders.
    HostRw = 2,
    /// Written by copies on the GPU, read by the CPU through a mapping.
    ReadBack = 3,
    /// Written once by a copy at creation, then read by shaders.
    DeviceOnlyConst = 4,
    /// Written by the CPU through a write-combined mapping and by shaders.
    UploadUav = 5,
}

impl MemoryKind {
    pub const COUNT: usize = 6;

    pub fn from_raw(value: i32) -> Option<MemoryKind> {
        Some(match value {
            0 => MemoryKind::DeviceOnly,
            1 => MemoryKind::DeviceUpload,
            2 => MemoryKind::HostRw,
            3 => MemoryKind::ReadBack,
            4 => MemoryKind::DeviceOnlyConst,
            5 => MemoryKind::UploadUav,
            _ => return None,
        })
    }

    /// The CPU has a mapping of a buffer of this kind for as long as the buffer lives.
    pub fn is_mapped(self) -> bool {
        !matches!(self, MemoryKind::DeviceOnly | MemoryKind::DeviceOnlyConst)
    }
}

/// The bindings of a pipeline layout, in the order of their slots.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LayoutDesc {
    /// Buffers the shader reads (`t0..`).
    pub read_buffers: u32,
    /// Buffers the shader writes (`u0..`).
    pub write_buffers: u32,
    /// Constant blocks (`b0..`).
    pub constant_blocks: u32,
    /// 32-bit constants set inline, in the constant block behind the others; 0: none.
    pub inline_constants: u32,
}

/// The state a buffer is in between commands, for the transitions around copies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferState {
    /// Shaders read and write it.
    ReadWrite,
    CopySource,
}

/// The format of a plane of an output image as a texture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextureFormat {
    /// One byte a sample.
    R8,
    /// Two bytes a sample.
    R16,
    /// Three samples of 10 bits in 32 bits.
    Rgb10A2,
}

/// A plane of an image in a buffer, as the source of a copy into a texture.
#[derive(Clone, Copy, Debug)]
pub struct TextureSource {
    /// Where the plane starts in the buffer, in bytes.
    pub offset: u64,
    pub format: TextureFormat,
    /// Texels of a row and rows.
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub row_pitch: u32,
}

/// A call of the backend that failed: which one, and its code (an `HRESULT` in D3D12).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub step: &'static str,
    pub code: i32,
}

/// One segment of the adapter's video memory.
#[derive(Clone, Copy, Default, Debug)]
pub struct MemorySegment {
    /// 0, or the code of the failed query; the numbers are 0 then.
    pub error: i32,
    pub budget: u64,
    pub usage: u64,
    pub available_for_reservation: u64,
    pub reservation: u64,
}

/// Budget and usage of the video memory of the adapter a device is on.
#[derive(Clone, Copy, Default, Debug)]
pub struct VideoMemory {
    /// 0, or the code of the step that failed before the segments could be asked for.
    pub adapter_error: i32,
    /// Memory on the adapter.
    pub local: MemorySegment,
    /// Shared system memory.
    pub non_local: MemorySegment,
}

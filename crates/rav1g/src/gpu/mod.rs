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
//!   For the pipeline cache on disk it says what it can keep of a compiled pipeline
//!   ([`ShaderCacheSupport`]), and keeps pipelines in a [`PipelineLibrary`] or gives the cached blob of
//!   one ([`Pipeline::cached_blob`]) to create it from again.
//!   [`OwnedDevice`] is the device for another thread.
//! - `Buffer`, [`BufferRef`]: a buffer the caller owns, and a buffer of the decoder where it is used.
//! - [`Layout`], `Pipeline`: what a dispatch needs besides its buffers; [`LayoutRef`] and [`PipelineRef`]
//!   are those of the decoder where they are used.
//! - [`Commands`]: the recording of a frame's commands: the layout and pipeline of the next dispatches,
//!   the buffers ([`BufferLocation`]: a place in a buffer) and constants at the binding slots of the
//!   layout, dispatches, barriers between dispatches that write and read the same buffer, state
//!   transitions ([`BufferState`]) and copies, between buffers and from a buffer into a texture of the
//!   application ([`TextureRef`], [`TextureSource`]). For the semi-planar output (docs/output.md) also the
//!   planes of a texture of the application as what a shader writes ([`PlaneViews`],
//!   [`Commands::bind_write_planes`]) and the states of that texture ([`TextureState`]).
//!
//! A binding slot is an index into the bindings of a layout in the order of [`LayoutDesc`]: the buffers
//! the shader reads, the buffers it writes, the constant blocks, then the inline constants. D3D12 has
//! them as the root parameters of a root signature; a backend with descriptor sets has them as the
//! bindings of one set, with the inline constants as push constants.
//!
//! There is one backend, D3D12 (`d3d12`). It has everything the host does with the GPU: the device and
//! its queue, buffers, layouts and pipelines, the command list with its allocators, fences and their
//! events, timestamp queries, and readback.

pub(crate) mod d3d12;

pub use d3d12::{
    BufferLocation, BufferRef, CommandAllocator, CommandList, Commands, Device, Event, Fence, Layout, LayoutRef,
    OwnedDevice, Pipeline, PipelineLibrary, PipelineRef, PlaneViews, Queue, Readback, TextureRef, TimestampQueries,
    Upload, create_hardware_device,
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
    /// Planes of a texture the shader writes, at the registers behind the write buffers (`u<write
    /// buffers>..`), as one binding behind all the others ([`Commands::bind_write_planes`]); 0: none.
    pub write_planes: u32,
}

/// The state a buffer is in between commands, for the transitions around copies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferState {
    /// Shaders read and write it.
    ReadWrite,
    CopySource,
    /// Read by shaders: what a buffer the CPU writes is in between the commands that write it on the
    /// GPU ([`MemoryKind::HostRw`] made with `gpu_writable`).
    Read,
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
    /// The luma plane of an NV12 texture: one byte a sample.
    Nv12Luma,
    /// The chroma plane of an NV12 texture: U and V, one byte each.
    Nv12Chroma,
    /// The luma plane of a P010 texture: two bytes a sample.
    P010Luma,
    /// The chroma plane of a P010 texture: U and V, two bytes each.
    P010Chroma,
}

/// A texture with a luma plane and a chroma plane of U and V side by side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SemiPlanar {
    /// 8 bits a sample.
    Nv12,
    /// 16 bits a sample, of which a 10-bit frame fills the high 10.
    P010,
}

impl SemiPlanar {
    pub fn name(self) -> &'static str {
        match self {
            SemiPlanar::Nv12 => "NV12",
            SemiPlanar::P010 => "P010",
        }
    }
}

/// What a device does with the textures of a [`SemiPlanar`] format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SemiPlanarSupport {
    /// The device has 2D textures of the format.
    pub texture: bool,
    /// Shaders can write the planes of such a texture.
    pub shader_writes: bool,
}

/// What a texture of the application is, as far as the output pass asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextureInfo {
    /// `None` for any other format.
    pub semi_planar: Option<SemiPlanar>,
    pub width: u32,
    pub height: u32,
    /// The texture was created for shaders to write it.
    pub shader_writes: bool,
}

/// The state a texture of the application is in between commands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextureState {
    /// The state a texture has between its uses on different queues and devices.
    Common,
    /// Shaders write it.
    ReadWrite,
    CopyDest,
}

/// A plane of an image in a buffer, as the source of a copy into a texture.
#[derive(Clone, Copy, Debug)]
pub struct TextureSource {
    /// Where the plane starts in the buffer, in bytes.
    pub offset: u64,
    /// The format of the plane; it also says which plane of a semi-planar texture is written.
    pub format: TextureFormat,
    /// Texels of a row and rows.
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub row_pitch: u32,
}

/// What a device can keep of a compiled pipeline, for the pipeline cache on disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShaderCacheSupport {
    /// Pipeline libraries: many pipelines by name in one blob.
    pub library: bool,
    /// The cached blob of one pipeline, from which the device creates it again.
    pub single_pipeline: bool,
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

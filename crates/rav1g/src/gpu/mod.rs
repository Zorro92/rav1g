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
//! - [`Device`]: creates buffers ([`MemoryKind`] says who reads and writes one) and reports the video
//!   memory of its adapter.
//! - `Buffer`, [`BufferRef`]: a buffer the caller owns, and a buffer of the decoder where it is used.
//!
//! There is one backend, D3D12 (`d3d12`). The interface grows with the modules of step 3 that need it:
//! pipeline layouts and compute pipelines with the shader library, the recording of commands (bindings,
//! dispatches, barriers, copies) with the stages. Fences, timestamp queries and readback are in the C++
//! of the frame pipeline (`av1_core.cpp`) and join with step 4.

pub(crate) mod d3d12;

pub use d3d12::{BufferRef, Device};

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

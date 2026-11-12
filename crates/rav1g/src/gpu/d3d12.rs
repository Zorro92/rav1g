// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The D3D12 backend of [`crate::gpu`].
//!
//! Until step 4 of the port the C++ frame pipeline owns the device, the queues and the command list, and
//! its structs hold the buffers and pipeline states as raw COM pointers. The types here therefore borrow
//! ([`Device`], [`BufferRef`]) or hand the pointer over ([`Buffer::into_raw`]) where a Rust owner would
//! hold the interface.

use std::ffi::c_void;
use std::ptr::NonNull;

use windows::Win32::Foundation::{E_FAIL, E_POINTER};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_MEMORY_SEGMENT_GROUP, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
    DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIFactory4,
};
use windows::core::Interface;

use super::{Error, MemoryKind, MemorySegment, VideoMemory};

/// A COM pointer field of a struct of the C++ host (`Microsoft::WRL::ComPtr<I>`, eight bytes: the
/// pointer) as a borrowed interface. `None` for a null pointer.
///
/// # Safety
/// `slot` is a `ComPtr<I>` (or an `I *`) that is null or holds a reference for `'a`.
pub unsafe fn com_ref<S, I: Interface>(slot: &S) -> Option<&I> {
    const { assert!(size_of::<S>() == size_of::<*mut c_void>()) };
    // SAFETY: the slot is one pointer; the caller says it is null or a live I.
    unsafe { I::from_raw_borrowed(&*std::ptr::from_ref(slot).cast::<*mut c_void>()) }
}

fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
    }
}

/// The device of the decoder.
#[derive(Clone, Copy)]
pub struct Device<'a> {
    device: &'a ID3D12Device,
}

impl<'a> Device<'a> {
    pub fn new(device: &'a ID3D12Device) -> Device<'a> {
        Device { device }
    }

    /// The name of the heap of a buffer kind in the decoder's log lines.
    pub fn heap_name(kind: MemoryKind) -> &'static str {
        ["DEFAULT", "UPLOAD", "CUSTOM L0 write-back", "READBACK", "DEFAULT", "CUSTOM L0 write-combine"][kind as usize]
    }

    /// A committed buffer of `size` bytes in the heap and the initial state of its kind, mapped when the
    /// kind is one the CPU reads or writes.
    pub fn create_buffer(&self, size: u64, kind: MemoryKind) -> Result<Buffer, Error> {
        let (heap_type, state, page) = match kind {
            MemoryKind::DeviceOnly => {
                (D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_CPU_PAGE_PROPERTY_UNKNOWN)
            }
            MemoryKind::DeviceUpload => {
                (D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_CPU_PAGE_PROPERTY_UNKNOWN)
            }
            MemoryKind::HostRw => {
                (D3D12_HEAP_TYPE_CUSTOM, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_CPU_PAGE_PROPERTY_WRITE_BACK)
            }
            MemoryKind::ReadBack => {
                (D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_CPU_PAGE_PROPERTY_UNKNOWN)
            }
            MemoryKind::DeviceOnlyConst => {
                (D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_CPU_PAGE_PROPERTY_UNKNOWN)
            }
            MemoryKind::UploadUav => {
                (D3D12_HEAP_TYPE_CUSTOM, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE)
            }
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: heap_type,
            CPUPageProperty: page,
            MemoryPoolPreference: if matches!(kind, MemoryKind::HostRw | MemoryKind::UploadUav) {
                D3D12_MEMORY_POOL_L0
            } else {
                D3D12_MEMORY_POOL_UNKNOWN
            },
            CreationNodeMask: 0,
            VisibleNodeMask: 0,
        };
        let flags = if matches!(kind, MemoryKind::DeviceOnly | MemoryKind::UploadUav) {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        };
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: valid heap properties and description; the out value is checked.
        unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &buffer_desc(size, flags),
                state,
                None,
                &mut resource,
            )
        }
        .map_err(|error| Error { step: "CreateCommittedResource", code: error.code().0 })?;
        let resource = resource.ok_or(Error { step: "CreateCommittedResource", code: E_FAIL.0 })?;
        let mut mapped: *mut c_void = std::ptr::null_mut();
        if kind.is_mapped() {
            let no_read = D3D12_RANGE { Begin: 0, End: 0 };
            // SAFETY: a buffer of a mappable heap; `mapped` receives the address. A failure releases the
            // resource with `resource`.
            unsafe { resource.Map(0, Some(&no_read), Some(&mut mapped)) }
                .map_err(|error| Error { step: "Map", code: error.code().0 })?;
        }
        Ok(Buffer { resource, mapped: NonNull::new(mapped.cast()) })
    }

    /// Budget and usage of the video memory of the adapter the device is on.
    pub fn video_memory(device: Option<&ID3D12Device>) -> VideoMemory {
        let mut info = VideoMemory {
            adapter_error: 0,
            local: MemorySegment { error: E_FAIL.0, ..Default::default() },
            non_local: MemorySegment { error: E_FAIL.0, ..Default::default() },
        };
        let Some(device) = device else {
            info.adapter_error = E_POINTER.0;
            return info;
        };
        // SAFETY: plain calls on a live device and on the interfaces they return.
        let adapter = unsafe {
            CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
                .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()))
        };
        let adapter = match adapter {
            Ok(adapter) => adapter,
            Err(error) => {
                info.adapter_error = error.code().0;
                return info;
            }
        };
        let segment = |group: DXGI_MEMORY_SEGMENT_GROUP| {
            let mut raw = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            // SAFETY: `raw` is the out value of the query.
            match unsafe { adapter.QueryVideoMemoryInfo(0, group, &mut raw) } {
                Ok(()) => MemorySegment {
                    error: 0,
                    budget: raw.Budget,
                    usage: raw.CurrentUsage,
                    available_for_reservation: raw.AvailableForReservation,
                    reservation: raw.CurrentReservation,
                },
                Err(error) => MemorySegment { error: error.code().0, ..Default::default() },
            }
        };
        info.local = segment(DXGI_MEMORY_SEGMENT_GROUP_LOCAL);
        info.non_local = segment(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL);
        info
    }
}

/// A buffer the caller owns.
pub struct Buffer {
    resource: ID3D12Resource,
    mapped: Option<NonNull<u8>>,
}

impl Buffer {
    /// The address of the mapping, for a kind that has one.
    pub fn mapped(&self) -> Option<NonNull<u8>> {
        self.mapped
    }

    /// The buffer as the pointer the structs of the C++ host hold (`ID3D12Resource *`, with the reference
    /// of this value). [`BufferRef::release`] gives the reference back.
    pub fn into_raw(self) -> *mut c_void {
        self.resource.into_raw()
    }
}

/// A buffer of the decoder in a command: the `ID3D12Resource` a `GpuBufferObject` of the C++ host holds.
#[derive(Clone, Copy)]
pub struct BufferRef<'a> {
    resource: &'a ID3D12Resource,
}

impl<'a> BufferRef<'a> {
    /// # Safety
    /// `raw` points to an `ID3D12Resource *` that holds a reference for `'a`.
    pub unsafe fn from_raw<P>(raw: &'a *mut P) -> Option<BufferRef<'a>> {
        // SAFETY: the caller's contract.
        unsafe { com_ref::<*mut P, ID3D12Resource>(raw) }.map(|resource| BufferRef { resource })
    }

    /// Ends the mapping of a mapped buffer.
    pub fn unmap(&self) {
        let nothing_written = D3D12_RANGE { Begin: 0, End: 0 };
        // SAFETY: the buffer is mapped; the caller does not use the mapping afterwards.
        unsafe { self.resource.Unmap(0, Some(&nothing_written)) };
    }

    /// Gives back the reference of [`Buffer::into_raw`].
    ///
    /// # Safety
    /// `raw` came from [`Buffer::into_raw`] and is not used afterwards.
    pub unsafe fn release(raw: *mut c_void) {
        // SAFETY: the caller's contract; dropping the interface releases the reference.
        drop(unsafe { ID3D12Resource::from_raw(raw) });
    }
}

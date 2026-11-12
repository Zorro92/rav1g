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
use std::mem::ManuallyDrop;
use std::ptr::NonNull;

use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_POINTER};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_MEMORY_SEGMENT_GROUP, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
    DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIFactory4,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObjectEx};
use windows::core::{IUnknown, Interface};

use super::{Error, LayoutDesc, MemoryKind, MemorySegment, VideoMemory};

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

/// Puts an interface into an empty COM pointer field of a struct of the C++ host, with its reference.
///
/// # Safety
/// `slot` is a `ComPtr` of the interface `raw` is, and is null.
pub unsafe fn com_store<S>(slot: *mut S, raw: *mut c_void) {
    const { assert!(size_of::<S>() == size_of::<*mut c_void>()) };
    // SAFETY: the slot is one pointer (the caller's contract).
    unsafe { slot.cast::<*mut c_void>().write(raw) };
}

/// Releases the interface of a COM pointer field of a struct of the C++ host and leaves the field null.
///
/// # Safety
/// `slot` is a `ComPtr` that is null or holds a reference nothing else gives back.
pub unsafe fn com_release<S>(slot: *mut S) {
    const { assert!(size_of::<S>() == size_of::<*mut c_void>()) };
    // SAFETY: the slot is one pointer, null or a COM interface with a reference to give back.
    unsafe {
        let raw = slot.cast::<*mut c_void>().replace(std::ptr::null_mut());
        if !raw.is_null() {
            drop(IUnknown::from_raw(raw));
        }
    }
}

fn failed(step: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |error| Error { step, code: error.code().0 }
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

    /// The device as a value that can go to another thread.
    pub fn owned(self) -> OwnedDevice {
        OwnedDevice { device: self.device.clone() }
    }

    /// What the device says about double precision in shaders; `None` when it does not say.
    pub fn double_precision_shader_ops(&self) -> Option<bool> {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: `options` is the out value of this feature, with its size.
        unsafe {
            self.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS>() as u32,
            )
        }
        .ok()
        .map(|()| options.DoublePrecisionFloatShaderOps.as_bool())
    }

    /// A pipeline layout: a root signature whose parameters are the bindings of `desc` in their order,
    /// each a root descriptor, and the inline constants as root constants in the constant register behind
    /// the constant blocks.
    pub fn create_layout(&self, desc: LayoutDesc) -> Result<Layout, Error> {
        let descriptor = |kind: D3D12_ROOT_PARAMETER_TYPE, register: u32| D3D12_ROOT_PARAMETER {
            ParameterType: kind,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: register, RegisterSpace: 0 },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let mut parameters: Vec<D3D12_ROOT_PARAMETER> = Vec::new();
        parameters.extend((0..desc.read_buffers).map(|i| descriptor(D3D12_ROOT_PARAMETER_TYPE_SRV, i)));
        parameters.extend((0..desc.write_buffers).map(|i| descriptor(D3D12_ROOT_PARAMETER_TYPE_UAV, i)));
        parameters.extend((0..desc.constant_blocks).map(|i| descriptor(D3D12_ROOT_PARAMETER_TYPE_CBV, i)));
        if desc.inline_constants != 0 {
            parameters.push(D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: desc.constant_blocks,
                        RegisterSpace: 0,
                        Num32BitValues: desc.inline_constants,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            });
        }
        let signature_desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: parameters.len() as u32,
            pParameters: parameters.as_ptr(),
            NumStaticSamplers: 0,
            pStaticSamplers: std::ptr::null(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
        };
        let mut blob: Option<ID3DBlob> = None;
        // SAFETY: the description points to `parameters`, which lives through the call.
        unsafe { D3D12SerializeRootSignature(&signature_desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None) }
            .map_err(failed("D3D12SerializeRootSignature"))?;
        let blob = blob.ok_or(Error { step: "D3D12SerializeRootSignature", code: E_FAIL.0 })?;
        // SAFETY: the blob holds the serialized signature for as long as it lives.
        let signature = unsafe {
            let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
            self.device.CreateRootSignature::<ID3D12RootSignature>(0, bytes)
        }
        .map_err(failed("CreateRootSignature"))?;
        Ok(Layout { signature })
    }

    /// A compute pipeline of the shader `bytecode` with the bindings of `layout`.
    pub fn create_pipeline(&self, layout: &Layout, bytecode: &[u8]) -> Result<Pipeline, Error> {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(layout.signature.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytecode.as_ptr().cast(), BytecodeLength: bytecode.len() },
            ..Default::default()
        };
        // SAFETY: the description points to the bytecode and holds the signature through the call.
        let state = unsafe { self.device.CreateComputePipelineState::<ID3D12PipelineState>(&desc) };
        // The reference the description held.
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        Ok(Pipeline { state: state.map_err(failed("CreateComputePipelineState"))? })
    }

    /// Fills buffers of the kind [`MemoryKind::DeviceOnlyConst`] and makes them readable for the shaders.
    /// Each buffer gets a staging buffer of its size, zeroed and then written by its `fill`; the copies
    /// run on `queue` and the call returns when they are done.
    pub fn upload(&self, queue: Queue<'_>, uploads: &mut [Upload<'_>]) -> Result<(), Error> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        };
        let mut staging: Vec<ID3D12Resource> = Vec::with_capacity(uploads.len());
        for upload in uploads.iter() {
            let mut resource: Option<ID3D12Resource> = None;
            // SAFETY: valid heap properties and description; the out value is checked.
            unsafe {
                self.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &buffer_desc(upload.size as u64, D3D12_RESOURCE_FLAG_NONE),
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut resource,
                )
            }
            .map_err(failed("CreateCommittedResource"))?;
            staging.push(resource.ok_or(Error { step: "CreateCommittedResource", code: E_FAIL.0 })?);
        }
        let no_read = D3D12_RANGE { Begin: 0, End: 0 };
        for (upload, resource) in uploads.iter_mut().zip(&staging) {
            let mut mapped: *mut c_void = std::ptr::null_mut();
            // SAFETY: an upload buffer of `upload.size` bytes; the mapping is used until Unmap below.
            unsafe {
                resource.Map(0, Some(&no_read), Some(&mut mapped)).map_err(failed("Map"))?;
                let bytes = std::slice::from_raw_parts_mut(mapped.cast::<u8>(), upload.size);
                bytes.fill(0);
                (upload.fill)(bytes);
                resource.Unmap(0, None);
            }
        }
        // SAFETY: plain calls on live interfaces; the barriers borrow the destination buffers, which
        // outlive the call, without taking a reference.
        unsafe {
            let allocator = self
                .device
                .CreateCommandAllocator::<ID3D12CommandAllocator>(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .map_err(failed("CreateCommandAllocator"))?;
            let list = self
                .device
                .CreateCommandList::<_, _, ID3D12GraphicsCommandList>(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &allocator,
                    None,
                )
                .map_err(failed("CreateCommandList"))?;
            for (upload, resource) in uploads.iter().zip(&staging) {
                list.CopyResource(upload.dst.resource, resource);
            }
            let barriers: Vec<D3D12_RESOURCE_BARRIER> = uploads
                .iter()
                .map(|upload| {
                    transition(upload.dst.resource, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_GENERIC_READ)
                })
                .collect();
            list.ResourceBarrier(&barriers);
            let _ = list.Close();
            queue
                .queue
                .ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().map_err(failed("QueryInterface"))?)]);

            let fence =
                self.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE).map_err(failed("CreateFence"))?;
            let _ = queue.queue.Signal(&fence, 1);
            if let Ok(event) = CreateEventW(None, false, false, None) {
                let _ = fence.SetEventOnCompletion(1, event);
                WaitForSingleObjectEx(event, INFINITE, false);
                let _ = CloseHandle(event);
            }
        }
        Ok(())
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

/// A transition barrier that borrows the resource: the barrier holds the pointer without a reference of
/// its own, for the length of the call it is passed to.
///
/// # Safety
/// `resource` outlives every use of the barrier.
unsafe fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a copy of the pointer in a field that is never dropped (the caller's contract).
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A device with a reference of its own, for another thread.
pub struct OwnedDevice {
    device: ID3D12Device,
}

impl OwnedDevice {
    pub fn device(&self) -> Device<'_> {
        Device { device: &self.device }
    }
}

/// The queue the blocking copies of [`Device::upload`] run on: the direct queue of the decoder.
#[derive(Clone, Copy)]
pub struct Queue<'a> {
    queue: &'a ID3D12CommandQueue,
}

impl<'a> Queue<'a> {
    pub fn new(queue: &'a ID3D12CommandQueue) -> Queue<'a> {
        Queue { queue }
    }
}

/// One buffer of [`Device::upload`].
pub struct Upload<'a> {
    pub dst: BufferRef<'a>,
    /// The size of the buffer in bytes.
    pub size: usize,
    /// Writes the content; the bytes start as zeros.
    pub fill: &'a mut dyn FnMut(&mut [u8]),
}

/// A pipeline layout: a root signature.
#[derive(Clone)]
pub struct Layout {
    signature: ID3D12RootSignature,
}

impl Layout {
    /// The layout as the pointer the structs of the C++ host hold (`ID3D12RootSignature *`, with the
    /// reference of this value).
    pub fn into_raw(self) -> *mut c_void {
        self.signature.into_raw()
    }
}

/// A compute pipeline: a pipeline state.
pub struct Pipeline {
    state: ID3D12PipelineState,
}

impl Pipeline {
    /// The pipeline as the pointer the structs of the C++ host hold (`ID3D12PipelineState *`, with the
    /// reference of this value).
    pub fn into_raw(self) -> *mut c_void {
        self.state.into_raw()
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

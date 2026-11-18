// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The D3D12 backend of [`crate::gpu`].
//!
//! The structs of the host keep an interface the way the C++ kept a COM pointer: one pointer in a field
//! (`crate::host::types`). The types here therefore come in two kinds: those that borrow ([`Device`],
//! [`BufferRef`], [`Queue`]) and those that are the interface and nothing else ([`CommandList`],
//! [`Fence`], [`Readback`] and the like), which such a field holds, or hands over as a raw pointer where a
//! buffer descriptor keeps one ([`Buffer::into_raw`]).

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;

use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_POINTER};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_TYPELESS, DXGI_FORMAT_R8_UINT,
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_TYPELESS, DXGI_FORMAT_R8G8_UINT, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_TYPELESS,
    DXGI_FORMAT_R16G16_UINT, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_MEMORY_SEGMENT_GROUP, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
    DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIFactory4,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObjectEx};
use windows::core::{IUnknown, Interface};

use super::{
    Error, LayoutDesc, MemoryKind, MemorySegment, SemiPlanar, SemiPlanarSupport, TextureInfo, TextureState, VideoMemory,
};

/// A field of a struct of the host that holds an interface or none (eight bytes: the pointer) as a
/// borrowed interface. `None` for a null pointer.
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

/// A type of this module that is one interface and nothing else, so that a COM pointer field of a struct
/// of the C++ host can be read as it.
///
/// # Safety
/// The type is `repr(transparent)` over one COM interface.
pub unsafe trait Wrapped {}

// SAFETY: each is `repr(transparent)` over its interface.
unsafe impl Wrapped for CommandQueue {}
unsafe impl Wrapped for CommandAllocator {}
unsafe impl Wrapped for CommandList {}
unsafe impl Wrapped for Fence {}

/// A COM pointer field of a struct of the C++ host as the type of this module that wraps its interface.
/// `None` for a null pointer.
///
/// # Safety
/// `slot` is a `ComPtr` of the interface `T` wraps, null or with a reference for `'a`.
pub unsafe fn wrapped_ref<S, T: Wrapped>(slot: &S) -> Option<&T> {
    const { assert!(size_of::<S>() == size_of::<*mut c_void>() && size_of::<Option<T>>() == size_of::<*mut c_void>()) };
    // SAFETY: the slot is one pointer, null or the interface (the caller's contract); `Option<T>` is that
    // pointer with null as `None`.
    unsafe { (*std::ptr::from_ref(slot).cast::<Option<T>>()).as_ref() }
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
    /// kind is one the CPU reads or writes. `gpu_writable`: a buffer of [`MemoryKind::HostRw`] that a
    /// command list may also write (in the state [`super::BufferState::ReadWrite`], from and back to
    /// [`super::BufferState::Read`]): the same heap and the same initial state, with unordered access
    /// allowed. A custom heap puts no limit on the states of its buffers, as the upload and the readback
    /// heap do; [`MemoryKind::UploadUav`] is such a buffer with the other page property.
    pub fn create_buffer(&self, size: u64, kind: MemoryKind, gpu_writable: bool) -> Result<Buffer, Error> {
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
        let flags = if matches!(kind, MemoryKind::DeviceOnly | MemoryKind::UploadUav)
            || (gpu_writable && kind == MemoryKind::HostRw)
        {
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
        // The planes of a texture: a table of descriptors, which is how D3D12 binds a texture.
        let planes = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: desc.write_planes,
            BaseShaderRegister: desc.write_buffers,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        };
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
        if desc.write_planes != 0 {
            parameters.push(D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &raw const planes,
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
        // SAFETY: the description points to `parameters` and `planes`, which live through the call.
        unsafe { D3D12SerializeRootSignature(&signature_desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None) }
            .map_err(failed("D3D12SerializeRootSignature"))?;
        let blob = blob.ok_or(Error { step: "D3D12SerializeRootSignature", code: E_FAIL.0 })?;
        // SAFETY: the blob holds the serialized signature for as long as it lives.
        let signature = unsafe {
            let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
            self.device.CreateRootSignature::<ID3D12RootSignature>(0, bytes)
        }
        .map_err(failed("CreateRootSignature"))?;
        // SAFETY: as above; the bytes are copied for the key of the pipeline cache.
        let serialized: std::sync::Arc<[u8]> =
            unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }.into();
        Ok(Layout { signature, serialized })
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
    /// The serialized root signature it was created from.
    serialized: std::sync::Arc<[u8]>,
}

impl Layout {
    /// The layout as the backend describes it to the device (in D3D12 the serialized root signature): a
    /// part of the key of the pipeline cache.
    pub fn serialized(&self) -> &[u8] {
        &self.serialized
    }

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

/// A pipeline of the decoder in a command: the pipeline state and the root signature a `ComputeShader`
/// of the C++ host holds.
#[derive(Clone, Copy)]
pub struct PipelineRef<'a> {
    state: &'a ID3D12PipelineState,
    signature: &'a ID3D12RootSignature,
}

impl<'a> PipelineRef<'a> {
    /// # Safety
    /// `state` and `signature` are the COM pointers of a created pipeline (`ID3D12PipelineState`,
    /// `ID3D12RootSignature`) and hold their references for `'a`.
    pub unsafe fn from_com<S, L>(state: &'a S, signature: &'a L) -> Option<PipelineRef<'a>> {
        // SAFETY: the caller's contract.
        unsafe { Some(PipelineRef { state: com_ref(state)?, signature: com_ref(signature)? }) }
    }

    /// The layout of the pipeline.
    pub fn layout(&self) -> LayoutRef<'a> {
        LayoutRef { signature: self.signature }
    }
}

/// A pipeline layout of the decoder in a command.
#[derive(Clone, Copy)]
pub struct LayoutRef<'a> {
    signature: &'a ID3D12RootSignature,
}

impl<'a> LayoutRef<'a> {
    /// # Safety
    /// `signature` is the COM pointer of a created layout (`ID3D12RootSignature`) and holds its reference
    /// for `'a`.
    pub unsafe fn from_com<L>(signature: &'a L) -> Option<LayoutRef<'a>> {
        // SAFETY: the caller's contract.
        unsafe { Some(LayoutRef { signature: com_ref(signature)? }) }
    }
}

/// A texture of the application that an output image is copied into.
#[derive(Clone, Copy)]
pub struct TextureRef<'a> {
    resource: &'a ID3D12Resource,
}

impl<'a> TextureRef<'a> {
    /// # Safety
    /// `raw` points to null or to an `ID3D12Resource *` of a texture that holds a reference for `'a`.
    pub unsafe fn from_raw<P>(raw: &'a *mut P) -> Option<TextureRef<'a>> {
        // SAFETY: the caller's contract.
        unsafe { com_ref::<*mut P, ID3D12Resource>(raw) }.map(|resource| TextureRef { resource })
    }

    /// What the texture is.
    pub fn info(&self) -> TextureInfo {
        // SAFETY: a plain call on a live resource.
        let desc = unsafe { self.resource.GetDesc() };
        let texture = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        TextureInfo {
            semi_planar: match desc.Format {
                DXGI_FORMAT_NV12 if texture => Some(SemiPlanar::Nv12),
                DXGI_FORMAT_P010 if texture => Some(SemiPlanar::P010),
                _ => None,
            },
            width: desc.Width as u32,
            height: desc.Height,
            shader_writes: desc.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
        }
    }
}

fn semi_planar_format(format: SemiPlanar) -> DXGI_FORMAT {
    match format {
        SemiPlanar::Nv12 => DXGI_FORMAT_NV12,
        SemiPlanar::P010 => DXGI_FORMAT_P010,
    }
}

fn texture_state(state: TextureState) -> D3D12_RESOURCE_STATES {
    match state {
        TextureState::Common => D3D12_RESOURCE_STATE_COMMON,
        TextureState::ReadWrite => D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        TextureState::CopyDest => D3D12_RESOURCE_STATE_COPY_DEST,
    }
}

/// The views of the planes of the application's textures that the output pass writes: a ring of pairs
/// (luma, chroma) in a descriptor heap. A pair is written when the frame is recorded and has to stay
/// until the frame's commands have run, so the ring has more pairs than a decoder has frames in flight.
#[repr(transparent)]
pub struct PlaneViews {
    heap: ID3D12DescriptorHeap,
}

// SAFETY: `repr(transparent)` over its interface.
unsafe impl Wrapped for PlaneViews {}

impl PlaneViews {
    /// The pairs of the ring.
    pub const PAIRS: u32 = 32;

    /// Hands the reference over as a raw `ID3D12DescriptorHeap *`.
    pub fn into_raw(self) -> *mut c_void {
        self.heap.into_raw()
    }
}

impl Device<'_> {
    /// What the device does with textures of `format`.
    pub fn semi_planar_support(&self, format: SemiPlanar) -> SemiPlanarSupport {
        let support = |format: DXGI_FORMAT| {
            let mut data = D3D12_FEATURE_DATA_FORMAT_SUPPORT { Format: format, ..Default::default() };
            // SAFETY: the struct of the feature, with its size.
            unsafe {
                self.device.CheckFeatureSupport(
                    D3D12_FEATURE_FORMAT_SUPPORT,
                    (&raw mut data).cast(),
                    size_of::<D3D12_FEATURE_DATA_FORMAT_SUPPORT>() as u32,
                )
            }
            .ok()
            .map(|()| data.Support1)
        };
        let Some(texture) = support(semi_planar_format(format)) else {
            return SemiPlanarSupport { texture: false, shader_writes: false };
        };
        // A shader writes a plane through a view of an integer format: one for luma, one for chroma.
        let views = match format {
            SemiPlanar::Nv12 => [DXGI_FORMAT_R8_UINT, DXGI_FORMAT_R8G8_UINT],
            SemiPlanar::P010 => [DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16_UINT],
        };
        let writable = |support: Option<D3D12_FORMAT_SUPPORT1>| {
            support.is_some_and(|support| support.contains(D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW))
        };
        SemiPlanarSupport {
            texture: texture.contains(D3D12_FORMAT_SUPPORT1_TEXTURE2D),
            shader_writes: texture.contains(D3D12_FORMAT_SUPPORT1_TEXTURE2D)
                && writable(Some(texture))
                && views.into_iter().all(|view| writable(support(view))),
        }
    }

    /// The ring of plane views of a decoder.
    pub fn create_plane_views(&self) -> Result<PlaneViews, Error> {
        let desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: PlaneViews::PAIRS * 2,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        };
        // SAFETY: a plain call on a live device.
        let heap = unsafe { self.device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&desc) };
        Ok(PlaneViews { heap: heap.map_err(failed("CreateDescriptorHeap"))? })
    }

    /// Writes the views of the luma and the chroma plane of `texture` into the pair `pair` of the ring.
    pub fn write_plane_views(&self, views: &PlaneViews, pair: u32, texture: TextureRef<'_>, format: SemiPlanar) {
        let formats = match format {
            SemiPlanar::Nv12 => [DXGI_FORMAT_R8_UINT, DXGI_FORMAT_R8G8_UINT],
            SemiPlanar::P010 => [DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16_UINT],
        };
        // SAFETY: plain calls on a live device, heap and texture; the handles are inside the heap.
        unsafe {
            let step = self.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let start = views.heap.GetCPUDescriptorHandleForHeapStart();
            for (plane, format) in formats.into_iter().enumerate() {
                let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_UAV { MipSlice: 0, PlaneSlice: plane as u32 },
                    },
                };
                let handle = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: start.ptr + (pair as usize * 2 + plane) * step };
                self.device.CreateUnorderedAccessView(texture.resource, None, Some(&desc), handle);
            }
        }
    }
}

/// A place in a buffer, as a binding takes it: the GPU virtual address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BufferLocation {
    address: u64,
}

impl BufferLocation {
    /// The place `bytes` further on.
    pub fn offset(self, bytes: u64) -> BufferLocation {
        BufferLocation { address: self.address + bytes }
    }
}

/// The recording of a frame's commands: the compute command list.
///
/// A binding slot is a root parameter index: the bindings of the layout in the order of
/// [`LayoutDesc`](super::LayoutDesc).
#[derive(Clone, Copy)]
pub struct Commands<'a> {
    list: &'a ID3D12GraphicsCommandList,
}

/// A barrier for a buffer shaders write: the next command sees what the commands before it wrote.
///
/// # Safety
/// The buffer outlives every use of the barrier.
unsafe fn write_barrier(buffer: BufferRef<'_>) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: a copy of the pointer in a field that is never dropped (the caller's contract).
                pResource: unsafe { std::mem::transmute_copy(buffer.resource) },
            }),
        },
    }
}

fn resource_state(state: super::BufferState) -> D3D12_RESOURCE_STATES {
    match state {
        super::BufferState::ReadWrite => D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        super::BufferState::CopySource => D3D12_RESOURCE_STATE_COPY_SOURCE,
        super::BufferState::Read => D3D12_RESOURCE_STATE_GENERIC_READ,
    }
}

impl<'a> Commands<'a> {
    pub fn new(list: &'a ID3D12GraphicsCommandList) -> Commands<'a> {
        Commands { list }
    }

    /// The layout of the bindings that follow.
    #[inline]
    pub fn set_layout(&self, layout: LayoutRef<'_>) {
        // SAFETY: a live list and root signature.
        unsafe { self.list.SetComputeRootSignature(layout.signature) };
    }

    /// The pipeline of the dispatches that follow.
    #[inline]
    pub fn set_pipeline(&self, pipeline: PipelineRef<'_>) {
        // SAFETY: a live list and pipeline state.
        unsafe { self.list.SetPipelineState(pipeline.state) };
    }

    /// A buffer the shader reads, at a slot of the layout.
    #[inline]
    pub fn bind_read(&self, slot: u32, location: BufferLocation) {
        // SAFETY: a live list; the address is one of a buffer of the decoder.
        unsafe { self.list.SetComputeRootShaderResourceView(slot, location.address) };
    }

    /// A buffer the shader writes, at a slot of the layout.
    #[inline]
    pub fn bind_write(&self, slot: u32, location: BufferLocation) {
        // SAFETY: as for bind_read.
        unsafe { self.list.SetComputeRootUnorderedAccessView(slot, location.address) };
    }

    /// A constant block, at a slot of the layout.
    #[inline]
    pub fn bind_constants(&self, slot: u32, location: BufferLocation) {
        // SAFETY: as for bind_read.
        unsafe { self.list.SetComputeRootConstantBufferView(slot, location.address) };
    }

    /// The inline constants of the layout, from the `offset`th on.
    #[inline]
    pub fn set_inline_constants<T: InlineConstant>(&self, slot: u32, values: &[T], offset: u32) {
        // SAFETY: a live list; `values` are 32-bit words.
        unsafe {
            self.list.SetComputeRoot32BitConstants(slot, values.len() as u32, values.as_ptr().cast(), offset);
        }
    }

    #[inline]
    pub fn dispatch(&self, x: u32, y: u32, z: u32) {
        // SAFETY: a live list.
        unsafe { self.list.Dispatch(x, y, z) };
    }

    /// The commands that follow see what the commands before wrote into `buffer`.
    #[inline]
    pub fn write_barrier(&self, buffer: BufferRef<'_>) {
        // SAFETY: the barrier is used during the call, while the buffer is borrowed.
        unsafe { self.list.ResourceBarrier(&[write_barrier(buffer)]) };
    }

    /// The same for two buffers, as one command.
    #[inline]
    pub fn write_barrier2(&self, first: BufferRef<'_>, second: BufferRef<'_>) {
        // SAFETY: the barriers are used during the call, while the buffers are borrowed.
        unsafe { self.list.ResourceBarrier(&[write_barrier(first), write_barrier(second)]) };
    }

    /// Moves a buffer from one state to another, around a copy.
    #[inline]
    pub fn transition(&self, buffer: BufferRef<'_>, before: super::BufferState, after: super::BufferState) {
        // SAFETY: the barrier is used during the call, while the buffer is borrowed.
        unsafe {
            self.list.ResourceBarrier(&[transition(buffer.resource, resource_state(before), resource_state(after))]);
        }
    }

    /// Copies `size` bytes between buffers.
    #[inline]
    pub fn copy_buffer(&self, dst: BufferRef<'_>, dst_offset: u64, src: BufferRef<'_>, src_offset: u64, size: u64) {
        // SAFETY: live buffers and list.
        unsafe { self.list.CopyBufferRegion(dst.resource, dst_offset, src.resource, src_offset, size) };
    }

    /// The planes of a texture the shader writes, at the last slot of a layout with
    /// [`LayoutDesc::write_planes`](super::LayoutDesc): the pair `pair` of the ring.
    pub fn bind_write_planes(&self, slot: u32, device: Device<'_>, views: &PlaneViews, pair: u32) {
        // SAFETY: a live list and heap; the handle is inside the heap.
        unsafe {
            let step = device.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as u64;
            let start = views.heap.GetGPUDescriptorHandleForHeapStart();
            self.list.SetDescriptorHeaps(&[Some(views.heap.clone())]);
            self.list.SetComputeRootDescriptorTable(
                slot,
                D3D12_GPU_DESCRIPTOR_HANDLE { ptr: start.ptr + u64::from(pair) * 2 * step },
            );
        }
    }

    /// Moves a texture of the application from one state to another.
    pub fn transition_texture(&self, texture: TextureRef<'_>, before: TextureState, after: TextureState) {
        // SAFETY: the barrier is used during the call, while the texture is borrowed.
        unsafe {
            self.list.ResourceBarrier(&[transition(texture.resource, texture_state(before), texture_state(after))]);
        }
    }

    /// Copies a plane of an image that lies in a buffer into a texture of the application: into the
    /// texture of the plane, or into the plane of a semi-planar texture that the format names.
    #[inline]
    pub fn copy_buffer_to_texture(&self, texture: TextureRef<'_>, src: BufferRef<'_>, source: &super::TextureSource) {
        use super::TextureFormat;
        // The planes of a semi-planar texture are its subresources 0 and 1, and a copy names them in
        // the formats D3D12 gives their footprints (GetCopyableFootprints).
        let (format, subresource) = match source.format {
            TextureFormat::R8 => (DXGI_FORMAT_R8_UNORM, 0),
            TextureFormat::R16 => (DXGI_FORMAT_R16_UNORM, 0),
            TextureFormat::Rgb10A2 => (DXGI_FORMAT_R10G10B10A2_UNORM, 0),
            TextureFormat::Nv12Luma => (DXGI_FORMAT_R8_TYPELESS, 0),
            TextureFormat::Nv12Chroma => (DXGI_FORMAT_R8G8_TYPELESS, 1),
            TextureFormat::P010Luma => (DXGI_FORMAT_R16_TYPELESS, 0),
            TextureFormat::P010Chroma => (DXGI_FORMAT_R16G16_TYPELESS, 1),
        };
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: source.offset,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: format,
                Width: source.width,
                Height: source.height,
                Depth: 1,
                RowPitch: source.row_pitch,
            },
        };
        // SAFETY: the copy locations hold the pointers without a reference of their own (the fields are
        // never dropped) for the length of the call, while both resources are borrowed.
        unsafe {
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture.resource),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: subresource },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(src.resource),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
            };
            self.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
    }
}

/// A 32-bit word that can be an inline constant.
pub trait InlineConstant: Copy {}
impl InlineConstant for i32 {}
impl InlineConstant for u32 {}

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

    /// The start of the buffer as a binding takes it.
    #[inline]
    pub fn location(&self) -> BufferLocation {
        // SAFETY: a live buffer.
        BufferLocation { address: unsafe { self.resource.GetGPUVirtualAddress() } }
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

// ---------------------------------------------------------------------------------------------------------
// What the frame pipeline owns: the device and its queue, the command allocators and the command list,
// the fences. Each type is the interface and nothing else, so a field of the pipeline's structs that holds
// one (or none) is one pointer.
// ---------------------------------------------------------------------------------------------------------

/// The first hardware adapter that gives a D3D12 device (feature level 11.0), for a decoder that is
/// created without a device.
pub fn create_hardware_device() -> Result<OwnedDevice, Error> {
    // SAFETY: DXGI and D3D12 calls with valid arguments; every out value is checked.
    unsafe {
        let factory =
            CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(failed("CreateDXGIFactory2"))?;
        let mut index = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;
            let software = adapter.GetDesc1().is_ok_and(|desc| {
                desc.Flags & (windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0
            });
            if software {
                continue;
            }
            let mut device: Option<ID3D12Device> = None;
            let created =
                D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device);
            if let (Ok(()), Some(device)) = (created, device) {
                return Ok(OwnedDevice { device });
            }
        }
    }
    Err(Error { step: "D3D12CreateDevice", code: E_FAIL.0 })
}

impl OwnedDevice {
    /// Hands the reference over as a raw `ID3D12Device *`.
    pub fn into_raw(self) -> *mut c_void {
        self.device.into_raw()
    }
}

impl Device<'_> {
    /// A direct queue: the queue of a decoder that is given none.
    pub fn create_queue(&self) -> Result<CommandQueue, Error> {
        let desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
            ..Default::default()
        };
        // SAFETY: a valid description.
        let queue = unsafe { self.device.CreateCommandQueue::<ID3D12CommandQueue>(&desc) };
        Ok(CommandQueue { queue: queue.map_err(failed("CreateCommandQueue"))? })
    }

    /// The memory of the commands of one frame in flight.
    pub fn create_command_allocator(&self) -> Result<CommandAllocator, Error> {
        // SAFETY: a plain call on a live device.
        let allocator =
            unsafe { self.device.CreateCommandAllocator::<ID3D12CommandAllocator>(D3D12_COMMAND_LIST_TYPE_DIRECT) };
        Ok(CommandAllocator { allocator: allocator.map_err(failed("CreateCommandAllocator"))? })
    }

    /// A fence at the value 0; `shared`: one that another device can open (`CreateSharedHandle`).
    pub fn create_fence(&self, shared: bool) -> Result<Fence, Error> {
        let flags = if shared { D3D12_FENCE_FLAG_SHARED } else { D3D12_FENCE_FLAG_NONE };
        // SAFETY: a plain call on a live device.
        let fence = unsafe { self.device.CreateFence::<ID3D12Fence>(0, flags) };
        Ok(Fence { fence: fence.map_err(failed("CreateFence"))? })
    }

    /// The command list of the decoder, open on `allocator`.
    pub fn create_command_list(&self, allocator: &CommandAllocator) -> Result<CommandList, Error> {
        // SAFETY: a plain call on a live device with a live allocator.
        let list = unsafe {
            self.device.CreateCommandList::<_, _, ID3D12GraphicsCommandList>(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator.allocator,
                None,
            )
        };
        Ok(CommandList { list: list.map_err(failed("CreateCommandList"))? })
    }
}

/// A queue the decoder holds a reference of.
#[repr(transparent)]
pub struct CommandQueue {
    queue: ID3D12CommandQueue,
}

impl CommandQueue {
    /// Hands the reference over as a raw `ID3D12CommandQueue *`.
    pub fn into_raw(self) -> *mut c_void {
        self.queue.into_raw()
    }
}

/// The memory of the commands of one frame in flight.
#[repr(transparent)]
pub struct CommandAllocator {
    allocator: ID3D12CommandAllocator,
}

impl CommandAllocator {
    /// The memory is free for the commands of the next frame of its slot. The frame before has completed
    /// on the GPU (its slot went back to the pool behind its fence), which is what makes the call valid;
    /// what it returns has nothing to say then.
    pub fn reset(&self) {
        // SAFETY: a plain call on a live allocator.
        let _ = unsafe { self.allocator.Reset() };
    }

    /// Hands the reference over as a raw `ID3D12CommandAllocator *`.
    pub fn into_raw(self) -> *mut c_void {
        self.allocator.into_raw()
    }
}

/// The command list the frames are recorded with, one after the other.
#[repr(transparent)]
pub struct CommandList {
    list: ID3D12GraphicsCommandList,
}

impl CommandList {
    /// Starts the recording of a frame on the allocator of its slot. The list is closed between two
    /// frames, so the call does not fail for a reason the decoder could act on.
    pub fn reset(&self, allocator: &CommandAllocator) {
        // SAFETY: a plain call on a live list with a live allocator.
        let _ = unsafe { self.list.Reset(&allocator.allocator, None) };
    }

    /// Ends the recording of a frame. An error of the recording shows as the removal of the device.
    pub fn close_quietly(&self) {
        // SAFETY: a plain call on a live list.
        let _ = unsafe { self.list.Close() };
    }

    /// Ends the recording.
    pub fn close(&self) -> Result<(), Error> {
        // SAFETY: a plain call on a live list.
        unsafe { self.list.Close() }.map_err(failed("Close"))
    }

    /// Hands the reference over as a raw `ID3D12GraphicsCommandList *`.
    pub fn into_raw(self) -> *mut c_void {
        self.list.into_raw()
    }
}

/// A fence: the queue signals a value when the commands in front of the signal are done.
#[repr(transparent)]
pub struct Fence {
    fence: ID3D12Fence,
}

impl Fence {
    /// `event` is set when the fence reaches `value`.
    pub fn set_event(&self, value: u64, event: &Event) {
        // SAFETY: a plain call on a live fence with a live event.
        let _ = unsafe { self.fence.SetEventOnCompletion(value, event.handle) };
    }

    /// Hands the reference over as a raw `ID3D12Fence *`.
    pub fn into_raw(self) -> *mut c_void {
        self.fence.into_raw()
    }

    /// The fence as the interface of the backend, for the frame an application gets
    /// (`Frame::d3d12_fence`).
    pub fn d3d12(&self) -> &ID3D12Fence {
        &self.fence
    }
}

/// An event a thread waits on for a fence: set when the fence reaches a value, reset by the wait.
#[repr(transparent)]
pub struct Event {
    handle: windows::Win32::Foundation::HANDLE,
}

impl Event {
    pub fn new() -> Result<Event, Error> {
        // SAFETY: a plain call; the handle is closed by `close`.
        let handle = unsafe { CreateEventW(None, false, false, None) };
        Ok(Event { handle: handle.map_err(failed("CreateEvent"))? })
    }

    /// The event of a struct that keeps its handle as a pointer.
    ///
    /// # Safety
    /// `raw` is the handle of a live event.
    pub unsafe fn borrowed<P>(raw: &*mut P) -> &Event {
        // SAFETY: an `Event` is the handle, which is the pointer (both `repr(transparent)`).
        unsafe { &*std::ptr::from_ref(raw).cast::<Event>() }
    }

    /// Waits until the event is set, and resets it.
    pub fn wait(&self) {
        // SAFETY: a plain call on a live event.
        unsafe { WaitForSingleObjectEx(self.handle, INFINITE, false) };
    }

    /// The handle, for a struct that keeps it as one.
    pub fn into_raw(self) -> *mut c_void {
        self.handle.0
    }

    /// Closes an event of [`Event::into_raw`].
    ///
    /// # Safety
    /// `raw` is the handle of an event nothing waits on any more.
    pub unsafe fn close(raw: *mut c_void) {
        // SAFETY: the caller's contract.
        let _ = unsafe { CloseHandle(windows::Win32::Foundation::HANDLE(raw)) };
    }
}

// ---------------------------------------------------------------------------------------------------------
// Timestamps and readback: the timing run (-gputime) and the stage dumps (-dumpstages).
// ---------------------------------------------------------------------------------------------------------

/// The timestamps a command list can take: a query heap.
#[repr(transparent)]
pub struct TimestampQueries {
    heap: ID3D12QueryHeap,
}

impl TimestampQueries {
    /// Hands the reference over as a raw `ID3D12QueryHeap *`.
    pub fn into_raw(self) -> *mut c_void {
        self.heap.into_raw()
    }
}

/// A buffer the GPU copies into and the CPU reads once the copy is done.
#[repr(transparent)]
pub struct Readback {
    resource: ID3D12Resource,
}

impl Readback {
    /// The buffer as the destination of a copy.
    pub fn buffer(&self) -> BufferRef<'_> {
        BufferRef { resource: &self.resource }
    }

    /// Reads the first `size` bytes. `None` when the buffer cannot be mapped.
    pub fn read<R>(&self, size: usize, read: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let range = D3D12_RANGE { Begin: 0, End: size };
        let mut mapped: *mut c_void = std::ptr::null_mut();
        // SAFETY: a readback buffer of at least `size` bytes (the caller sized it); the mapping is used
        // until Unmap, and nothing was written through it.
        unsafe {
            if self.resource.Map(0, Some(&range), Some(&mut mapped)).is_err() || mapped.is_null() {
                return None;
            }
            let result = read(std::slice::from_raw_parts(mapped.cast::<u8>(), size));
            self.resource.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            Some(result)
        }
    }

    /// Hands the reference over as a raw `ID3D12Resource *`.
    pub fn into_raw(self) -> *mut c_void {
        self.resource.into_raw()
    }
}

// SAFETY: each is `repr(transparent)` over its interface.
unsafe impl Wrapped for TimestampQueries {}
unsafe impl Wrapped for Readback {}

impl Device<'_> {
    /// Room for `count` timestamps of a command list.
    pub fn create_timestamp_queries(&self, count: u32) -> Result<TimestampQueries, Error> {
        let desc = D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: count, NodeMask: 0 };
        let mut heap: Option<ID3D12QueryHeap> = None;
        // SAFETY: a valid description; the out value is checked.
        unsafe { self.device.CreateQueryHeap(&desc, &mut heap) }.map_err(failed("CreateQueryHeap"))?;
        Ok(TimestampQueries { heap: heap.ok_or(Error { step: "CreateQueryHeap", code: E_FAIL.0 })? })
    }

    /// A readback buffer of `size` bytes, ready as the destination of a copy.
    pub fn create_readback(&self, size: u64) -> Result<Readback, Error> {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: valid heap properties and description; the out value is checked.
        unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut resource,
            )
        }
        .map_err(failed("CreateCommittedResource"))?;
        Ok(Readback { resource: resource.ok_or(Error { step: "CreateCommittedResource", code: E_FAIL.0 })? })
    }
}

impl Queue<'_> {
    /// Submits the commands of a frame.
    pub fn execute(&self, list: &CommandList) {
        // SAFETY: plain calls on a live queue with a closed list.
        unsafe {
            if let Ok(list) = list.list.cast::<ID3D12CommandList>() {
                self.queue.ExecuteCommandLists(&[Some(list)]);
            }
        }
    }

    /// `fence` reaches `value` when the commands submitted so far are done.
    pub fn signal(&self, fence: &Fence, value: u64) {
        // SAFETY: a plain call on a live queue with a live fence.
        let _ = unsafe { self.queue.Signal(&fence.fence, value) };
    }

    /// Ticks per second of the timestamps of command lists that run on this queue; `None` when the queue
    /// takes no timestamps (they need a direct or a compute queue).
    pub fn timestamp_frequency(&self) -> Option<u64> {
        // SAFETY: plain calls on a live queue.
        unsafe {
            let kind = self.queue.GetDesc().Type;
            if kind != D3D12_COMMAND_LIST_TYPE_DIRECT && kind != D3D12_COMMAND_LIST_TYPE_COMPUTE {
                return None;
            }
            self.queue.GetTimestampFrequency().ok().filter(|&frequency| frequency != 0)
        }
    }
}

impl Commands<'_> {
    /// Takes timestamp `index` at this point of the list.
    #[inline]
    pub fn timestamp(&self, queries: &TimestampQueries, index: u32) {
        // SAFETY: a plain call on a live list with a live heap.
        unsafe { self.list.EndQuery(&queries.heap, D3D12_QUERY_TYPE_TIMESTAMP, index) };
    }

    /// Writes the first `count` timestamps to the start of `readback` when the list has run.
    pub fn resolve_timestamps(&self, queries: &TimestampQueries, count: u32, readback: &Readback) {
        // SAFETY: a plain call on a live list with a live heap and buffer.
        unsafe {
            self.list.ResolveQueryData(&queries.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count, &readback.resource, 0);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The pipeline cache on disk (host/pipeline_cache.rs): a pipeline library, or the cached blob of every
// pipeline state where the device has no pipeline libraries.
// ---------------------------------------------------------------------------------------------------------

impl Device<'_> {
    /// `None` while the device works; the reason it was removed (`GetDeviceRemovedReason`) when it was.
    pub fn removed_reason(&self) -> Option<i32> {
        // SAFETY: a valid device.
        unsafe { self.device.GetDeviceRemovedReason() }.err().map(|error| error.code().0)
    }

    /// What the device can keep of a compiled pipeline (`D3D12_FEATURE_SHADER_CACHE`).
    pub fn shader_cache_support(&self) -> super::ShaderCacheSupport {
        let mut data = D3D12_FEATURE_DATA_SHADER_CACHE::default();
        // SAFETY: `data` is the out value of this feature, with its size.
        let known = unsafe {
            self.device.CheckFeatureSupport(
                D3D12_FEATURE_SHADER_CACHE,
                (&raw mut data).cast(),
                size_of::<D3D12_FEATURE_DATA_SHADER_CACHE>() as u32,
            )
        }
        .is_ok();
        let flags = if known { data.SupportFlags } else { D3D12_SHADER_CACHE_SUPPORT_NONE };
        super::ShaderCacheSupport {
            library: (flags & D3D12_SHADER_CACHE_SUPPORT_LIBRARY) == D3D12_SHADER_CACHE_SUPPORT_LIBRARY
                && self.device.cast::<ID3D12Device1>().is_ok(),
            single_pipeline: (flags & D3D12_SHADER_CACHE_SUPPORT_SINGLE_PSO) == D3D12_SHADER_CACHE_SUPPORT_SINGLE_PSO,
        }
    }

    /// A pipeline library: empty, or of the serialized library `blob`. The library keeps the blob, which
    /// D3D12 does not copy, until it is destroyed (also when pipelines it gave out hold it), and frees it
    /// then. An error when the device refuses the blob: `E_INVALIDARG` for one it cannot read,
    /// `D3D12_ERROR_DRIVER_VERSION_MISMATCH` and `D3D12_ERROR_ADAPTER_NOT_FOUND` for one of another
    /// driver or adapter.
    pub fn create_pipeline_library(&self, blob: Vec<u8>) -> Result<PipelineLibrary, Error> {
        let device = self.device.cast::<ID3D12Device1>().map_err(failed("ID3D12Device1"))?;
        let owner = BlobOwner::new(blob);
        let mut library: Option<ID3D12PipelineLibrary> = None;
        // SAFETY: the bytes live in `owner`, which the library holds as its private data from here on (or
        // which is released below when there is no library). An empty blob is passed as a null pointer
        // (an empty slice has a dangling one, which a runtime may read as a blob).
        unsafe {
            let bytes = owner.bytes();
            let pointer = if bytes.is_empty() { std::ptr::null() } else { bytes.as_ptr().cast::<c_void>() };
            (Interface::vtable(&device).CreatePipelineLibrary)(
                device.as_raw(),
                pointer,
                bytes.len(),
                &ID3D12PipelineLibrary::IID,
                (&raw mut library).cast(),
            )
            .ok()
        }
        .map_err(failed("CreatePipelineLibrary"))?;
        let library = library.ok_or(Error { step: "CreatePipelineLibrary", code: E_POINTER.0 })?;
        // SAFETY: a valid library and an IUnknown; the library takes a reference of the owner.
        unsafe { library.SetPrivateDataInterface(&BLOB_OWNER_GUID, &owner.unknown) }
            .map_err(failed("SetPrivateDataInterface"))?;
        Ok(PipelineLibrary { library })
    }

    /// A compute pipeline as [`Device::create_pipeline`], from the cached blob of a pipeline state of the
    /// same shader and layout on this device. An error when the device refuses the blob.
    pub fn create_pipeline_from_cache(
        &self,
        layout: &Layout,
        bytecode: &[u8],
        cached: &[u8],
    ) -> Result<Pipeline, Error> {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(layout.signature.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytecode.as_ptr().cast(), BytecodeLength: bytecode.len() },
            CachedPSO: D3D12_CACHED_PIPELINE_STATE {
                pCachedBlob: cached.as_ptr().cast(),
                CachedBlobSizeInBytes: cached.len(),
            },
            ..Default::default()
        };
        // SAFETY: the description points to the bytecode and the blob and holds the signature through the
        // call.
        let state = unsafe { self.device.CreateComputePipelineState::<ID3D12PipelineState>(&desc) };
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        Ok(Pipeline { state: state.map_err(failed("CreateComputePipelineState"))? })
    }
}

/// A pipeline library of the device: compiled pipeline states by name, which can be written to a file and
/// given to a later device on the same adapter and driver.
///
/// D3D12 makes it safe to use from several threads, except for loading the same name on two threads at
/// once (the remarks of `ID3D12Device1::CreatePipelineLibrary`, "Thread Safety"); the decoder loads every
/// name once.
pub struct PipelineLibrary {
    library: ID3D12PipelineLibrary,
}

impl PipelineLibrary {
    /// The pipeline of `name`, which has to have been stored with the same shader and layout. An error
    /// (`E_INVALIDARG`) when the library has no pipeline of that name or one of another description.
    pub fn load(&self, name: &str, layout: &Layout, bytecode: &[u8]) -> Result<Pipeline, Error> {
        let name = wide(name);
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(layout.signature.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytecode.as_ptr().cast(), BytecodeLength: bytecode.len() },
            ..Default::default()
        };
        // SAFETY: a C string of UTF-16 and a description that points to the bytecode and holds the
        // signature through the call.
        let state = unsafe {
            self.library.LoadComputePipeline::<_, ID3D12PipelineState>(windows::core::PCWSTR(name.as_ptr()), &desc)
        };
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        Ok(Pipeline { state: state.map_err(failed("LoadComputePipeline"))? })
    }

    /// Adds a pipeline under `name`.
    pub fn store(&self, name: &str, pipeline: &Pipeline) -> Result<(), Error> {
        let name = wide(name);
        // SAFETY: a C string of UTF-16 and a valid pipeline state.
        unsafe { self.library.StorePipeline(windows::core::PCWSTR(name.as_ptr()), &pipeline.state) }
            .map_err(failed("StorePipeline"))
    }

    /// The library as bytes for a file.
    pub fn serialize(&self) -> Result<Vec<u8>, Error> {
        // SAFETY: a valid library; the buffer has the size it asks for.
        unsafe {
            let mut bytes = vec![0u8; self.library.GetSerializedSize()];
            self.library.Serialize(&mut bytes).map_err(failed("Serialize"))?;
            Ok(bytes)
        }
    }
}

impl Pipeline {
    /// A reference of its own of the pipeline in a COM pointer field of the C++ host.
    ///
    /// # Safety
    /// `slot` is the COM pointer of a pipeline state, null or with a reference for the call.
    pub unsafe fn from_com<S>(slot: &S) -> Option<Pipeline> {
        // SAFETY: the caller's contract.
        unsafe { com_ref::<S, ID3D12PipelineState>(slot) }.map(|state| Pipeline { state: state.clone() })
    }

    /// What the driver can create the pipeline from again on this device, without compiling it
    /// (`GetCachedBlob`).
    pub fn cached_blob(&self) -> Result<Vec<u8>, Error> {
        // SAFETY: a valid pipeline state; the blob holds its bytes for as long as it lives.
        unsafe {
            let blob = self.state.GetCachedBlob().map_err(failed("GetCachedBlob"))?;
            Ok(std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()).to_vec())
        }
    }
}

/// A name as a C string of UTF-16.
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The private data of a pipeline library that holds its blob ({6c1b0e52-3f2a-4f5e-9a0d-7b2f4c8e1d93}).
const BLOB_OWNER_GUID: windows::core::GUID = windows::core::GUID::from_u128(0x6c1b0e52_3f2a_4f5e_9a0d_7b2f4c8e1d93);

/// The bytes of a serialized pipeline library as a COM object (only `IUnknown`), which the library holds
/// as its private data (`SetPrivateDataInterface`) and releases when it is destroyed: the way the
/// documentation of `CreatePipelineLibrary` recommends to keep the blob alive exactly as long as the
/// library, whatever holds the library.
#[repr(C)]
struct BlobObject {
    vtable: *const windows::core::IUnknown_Vtbl,
    references: std::sync::atomic::AtomicU32,
    bytes: Vec<u8>,
}

static BLOB_OBJECT_VTABLE: windows::core::IUnknown_Vtbl = windows::core::IUnknown_Vtbl {
    QueryInterface: blob_object_query_interface,
    AddRef: blob_object_add_ref,
    Release: blob_object_release,
};

unsafe extern "system" fn blob_object_query_interface(
    this: *mut c_void,
    iid: *const windows::core::GUID,
    interface: *mut *mut c_void,
) -> windows::core::HRESULT {
    if interface.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM's contract: `iid` is valid, `interface` is valid for writing, `this` is a BlobObject.
    unsafe {
        if !iid.is_null() && *iid == IUnknown::IID {
            blob_object_add_ref(this);
            *interface = this;
            windows::core::HRESULT(0)
        } else {
            *interface = std::ptr::null_mut();
            windows::Win32::Foundation::E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn blob_object_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live BlobObject (COM's contract).
    unsafe { (*this.cast::<BlobObject>()).references.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn blob_object_release(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live BlobObject from Box::into_raw (COM's contract); the last reference frees it.
    unsafe {
        let left = (*this.cast::<BlobObject>()).references.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this.cast::<BlobObject>()));
        }
        left
    }
}

/// The application's side of a [`BlobObject`]: one reference.
struct BlobOwner {
    unknown: IUnknown,
}

impl BlobOwner {
    fn new(bytes: Vec<u8>) -> BlobOwner {
        let object = Box::into_raw(Box::new(BlobObject {
            vtable: &raw const BLOB_OBJECT_VTABLE,
            references: std::sync::atomic::AtomicU32::new(1),
            bytes,
        }));
        // SAFETY: a COM object with one reference, which the IUnknown takes over.
        BlobOwner { unknown: unsafe { IUnknown::from_raw(object.cast()) } }
    }

    /// The bytes, valid for as long as a reference of the object is held.
    ///
    /// # Safety
    /// The caller keeps a reference (this owner, or the library's) for as long as it uses the bytes.
    unsafe fn bytes(&self) -> &'static [u8] {
        // SAFETY: the object is a BlobObject that lives while a reference is held (the caller's contract);
        // its bytes do not move.
        unsafe { &(*self.unknown.as_raw().cast::<BlobObject>()).bytes }
    }
}

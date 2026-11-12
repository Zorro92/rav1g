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
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
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

    /// Copies a plane of an image that lies in a buffer into a texture of the application.
    #[inline]
    pub fn copy_buffer_to_texture(&self, texture: TextureRef<'_>, src: BufferRef<'_>, source: &super::TextureSource) {
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: source.offset,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: match source.format {
                    super::TextureFormat::R8 => DXGI_FORMAT_R8_UNORM,
                    super::TextureFormat::R16 => DXGI_FORMAT_R16_UNORM,
                    super::TextureFormat::Rgb10A2 => DXGI_FORMAT_R10G10B10A2_UNORM,
                },
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
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
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

    /// A fence at the value 0.
    pub fn create_fence(&self) -> Result<Fence, Error> {
        // SAFETY: a plain call on a live device.
        let fence = unsafe { self.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) };
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
    /// Hands the reference over as a raw `ID3D12Fence *`.
    pub fn into_raw(self) -> *mut c_void {
        self.fence.into_raw()
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

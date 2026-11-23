// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The pool of output images the decoder writes its frames into: readback buffers, mapped for as long as
//! they exist. The decoder takes one for every frame that is output (on its decode thread) and hands it
//! back with the frame-ready callback (on its GPU completion thread), where the frame is hashed and the
//! buffer goes back into the pool; this is the player's -nopresent path with readback buffers only.
//!
//! With -output p010 or p010-copy (docs/output.md) an image is an NV12 or a P010 texture next to its
//! buffer. The decoder gets the texture alone; when the frame comes back, the pool copies the two planes
//! of the texture into the buffer on a command list of its own and waits for it, and the frame is hashed
//! from there. So what is hashed is what a reader of the texture finds in it.

use std::ffi::c_void;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_TYPELESS, DXGI_FORMAT_R8G8_TYPELESS,
    DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16G16_TYPELESS, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

use crate::device::hex32;

const RENT_WAIT_SECONDS: u64 = 600;

/// -output: what the output images of a task are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageMode {
    /// A buffer with the planes of the frame one behind the other.
    Planes,
    /// The semi-planar output in a texture that shaders may write: the output pass writes it directly
    /// where the device allows it.
    Texture,
    /// The semi-planar output in a texture that shaders may not write: the output pass copies into it.
    TextureCopy,
    /// The semi-planar output in a buffer.
    Buffer,
}

impl ImageMode {
    pub fn parse(text: &str) -> Option<ImageMode> {
        Some(match text {
            "planes" | "planar" => ImageMode::Planes,
            "p010" | "nv12" => ImageMode::Texture,
            "p010-copy" | "nv12-copy" => ImageMode::TextureCopy,
            "p010-buffer" | "nv12-buffer" => ImageMode::Buffer,
            _ => return None,
        })
    }

    pub fn is_semi_planar(self) -> bool {
        self != ImageMode::Planes
    }

    fn has_texture(self) -> bool {
        matches!(self, ImageMode::Texture | ImageMode::TextureCopy)
    }
}

/// What the decoder asks an image for.
#[derive(Clone, Copy)]
pub struct Request {
    /// Bytes of a buffer for the frame.
    pub size: usize,
    /// Of the frame.
    pub width: u32,
    pub height: u32,
    /// The frame is 10-bit and its semi-planar texture P010; NV12 otherwise.
    pub high_bit_depth: bool,
}

/// Where the two planes of a semi-planar frame of a size lie in a buffer: the bytes of a row, and the
/// place of the plane of U and V. The layout the decoder has for such a buffer (docs/output.md).
pub fn semi_planar_layout(width: u32, height: u32, high_bit_depth: bool) -> (usize, usize) {
    let bytes = if high_bit_depth { 2 } else { 1 };
    let stride = ((width as usize).next_multiple_of(2) * bytes).next_multiple_of(256);
    (stride, (stride * (height as usize).next_multiple_of(2)).next_multiple_of(512))
}

struct Buffer {
    resource: Option<ID3D12Resource>,
    mem: *mut c_void,
    size: usize,
    rented: bool,
    /// The texture of the image, with its size and whether it is P010.
    texture: Option<(ID3D12Resource, u32, u32, bool)>,
}

impl Buffer {
    fn drop_resource(&mut self) {
        if let Some(resource) = self.resource.take()
            && !self.mem.is_null()
        {
            // SAFETY: the resource was mapped at subresource 0 when mem was set.
            unsafe { resource.Unmap(0, None) };
        }
        self.mem = std::ptr::null_mut();
        self.size = 0;
    }
}

/// A buffer of the pool in the hands of the decoder.
pub struct Rented {
    /// What comes back with the frame: `OutputPool::give_back` takes it.
    pub token: usize,
    /// The readback buffer.
    pub resource: ID3D12Resource,
    /// The mapped memory of the buffer.
    pub mem: *mut u8,
    pub size: usize,
    /// The texture of the image: the decoder gets it in place of the buffer.
    pub texture: Option<ID3D12Resource>,
}

/// What the pool reads the textures back with.
struct Reader {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    event: HANDLE,
}

pub struct OutputPool {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    mode: ImageMode,
    buffers: Mutex<Vec<Buffer>>,
    returned: Condvar,
    reader: Mutex<Option<Reader>>,
}

// SAFETY: the buffers are only touched under the lock; D3D12 devices and resources are free-threaded.
unsafe impl Send for OutputPool {}
unsafe impl Sync for OutputPool {}

fn failed(what: &str, error: &windows::core::Error) -> String {
    format!("{what}: hr {}", hex32(error.code()))
}

impl OutputPool {
    pub fn new(device: &ID3D12Device, queue: &ID3D12CommandQueue, count: i32, mode: ImageMode) -> OutputPool {
        let buffers = (0..count.max(1))
            .map(|_| Buffer { resource: None, mem: std::ptr::null_mut(), size: 0, rented: false, texture: None })
            .collect();
        OutputPool {
            device: device.clone(),
            queue: queue.clone(),
            mode,
            buffers: Mutex::new(buffers),
            returned: Condvar::new(),
            reader: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Buffer>> {
        self.buffers.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// An NV12 or P010 texture of the frame size, both rounded up to even, in the common state.
    fn create_texture(&self, width: u32, height: u32, high_bit_depth: bool) -> Result<ID3D12Resource, String> {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: if high_bit_depth { DXGI_FORMAT_P010 } else { DXGI_FORMAT_NV12 },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: if self.mode == ImageMode::Texture {
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
            } else {
                D3D12_RESOURCE_FLAG_NONE
            },
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        let mut create = |desc: &D3D12_RESOURCE_DESC| {
            // SAFETY: valid heap properties and description; the out value is checked.
            unsafe {
                self.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut texture,
                )
            }
        };
        let mut result = create(&desc);
        if let (Err(error), ImageMode::Texture) = (&result, self.mode) {
            // A device without such textures for shaders to write: the decoder then copies into one
            // that they may not write.
            crate::say!(
                "TEXTURE the device has no {} texture that shaders may write (hr {}): the texture is created without",
                if high_bit_depth { "P010" } else { "NV12" },
                hex32(error.code())
            );
            desc.Flags = D3D12_RESOURCE_FLAG_NONE;
            result = create(&desc);
        }
        result.map_err(|error| {
            let format = if high_bit_depth { "P010" } else { "NV12" };
            let writable = if self.mode == ImageMode::Texture { ", written by shaders" } else { "" };
            failed(&format!("{format} texture of {width}x{height}{writable}"), &error)
        })?;
        texture.ok_or_else(|| "no texture".to_string())
    }

    /// Waits for a free image for a frame. The error says whether none came back in time or the device
    /// refused the buffer or the texture.
    pub fn rent(&self, request: &Request) -> Result<Rented, String> {
        let size = request.size;
        let guard = self.lock();
        let (mut buffers, timeout) = self
            .returned
            .wait_timeout_while(guard, Duration::from_secs(RENT_WAIT_SECONDS), |buffers| {
                buffers.iter().all(|buffer| buffer.rented)
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = buffers.iter().position(|buffer| !buffer.rented) else {
            let _ = timeout;
            return Err(format!(
                "no output image came back to the pool of {} within {} s",
                buffers.len(),
                RENT_WAIT_SECONDS
            ));
        };
        let buffer = &mut buffers[index];
        if buffer.size < size {
            buffer.drop_resource();
            let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut resource: Option<ID3D12Resource> = None;
            // SAFETY: valid heap properties and description; the out value is checked.
            let mut result = unsafe {
                self.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut resource,
                )
            };
            let mut mem: *mut c_void = std::ptr::null_mut();
            if result.is_ok() {
                result = match &resource {
                    // SAFETY: a readback buffer can be mapped; mem receives the address.
                    Some(resource) => unsafe { resource.Map(0, None, Some(&mut mem)) },
                    None => Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32))),
                };
            }
            if let Err(error) = result {
                return Err(format!("readback buffer of {} bytes: hr {}", size, hex32(error.code())));
            }
            buffer.resource = resource;
            buffer.mem = mem;
            buffer.size = size;
        }
        let Some(resource) = buffer.resource.clone() else {
            return Err(format!("no readback buffer of {size} bytes"));
        };
        let mut texture = None;
        if self.mode.has_texture() {
            let (width, height) = (request.width.next_multiple_of(2), request.height.next_multiple_of(2));
            let wanted = (width, height, request.high_bit_depth);
            if !buffer.texture.as_ref().is_some_and(|(_, w, h, hbd)| (*w, *h, *hbd) == wanted) {
                buffer.texture = None;
                buffer.texture =
                    Some((self.create_texture(width, height, request.high_bit_depth)?, width, height, wanted.2));
            }
            texture = buffer.texture.as_ref().map(|(texture, ..)| texture.clone());
        }
        buffer.rented = true;
        Ok(Rented { token: index + 1, resource, mem: buffer.mem.cast(), size: buffer.size, texture })
    }

    fn reader(&self) -> Result<Reader, String> {
        // SAFETY: plain creations on a live device; every result is checked.
        unsafe {
            // The lists run on the decoder's queue: of its type (direct, or compute with -computequeue).
            let kind = self.queue.GetDesc().Type;
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(kind)
                .map_err(|error| failed("command allocator of the readback", &error))?;
            let list: ID3D12GraphicsCommandList = self
                .device
                .CreateCommandList(0, kind, &allocator, None)
                .map_err(|error| failed("command list of the readback", &error))?;
            list.Close().map_err(|error| failed("command list of the readback", &error))?;
            let fence: ID3D12Fence = self
                .device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .map_err(|error| failed("fence of the readback", &error))?;
            let event =
                CreateEventW(None, false, false, None).map_err(|error| failed("event of the readback", &error))?;
            Ok(Reader { allocator, list, fence, value: 0, event })
        }
    }

    /// Copies the two planes of the texture of a rented image into its buffer, in the layout of
    /// [`semi_planar_layout`], and waits for the copy. Returns the mapping of the buffer.
    pub fn read_texture(&self, token: usize) -> Result<*const u8, String> {
        let (texture, width, height, high_bit_depth, buffer, mem) = {
            let buffers = self.lock();
            let entry = buffers.get(token.wrapping_sub(1)).ok_or("no such image")?;
            let (texture, width, height, hbd) = entry.texture.clone().ok_or("the image has no texture")?;
            (texture, width, height, hbd, entry.resource.clone().ok_or("the image has no buffer")?, entry.mem)
        };
        let (stride, chroma) = semi_planar_layout(width, height, high_bit_depth);
        let mut guard = self.reader.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(self.reader()?);
        }
        let reader = guard.as_mut().ok_or("no reader")?;
        let formats: [DXGI_FORMAT; 2] = if high_bit_depth {
            [DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16G16_TYPELESS]
        } else {
            [DXGI_FORMAT_R8_TYPELESS, DXGI_FORMAT_R8G8_TYPELESS]
        };
        let transition = |before, after| D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // SAFETY: a copy of the pointer in a field that is never dropped, used while the
                    // texture is alive.
                    pResource: unsafe { std::mem::transmute_copy(&texture) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        };
        // SAFETY: live objects; the copy locations hold their pointers without a reference for the
        // length of the calls; every result is checked.
        unsafe {
            reader.allocator.Reset().map_err(|error| failed("readback of a texture", &error))?;
            reader.list.Reset(&reader.allocator, None).map_err(|error| failed("readback of a texture", &error))?;
            reader.list.ResourceBarrier(&[transition(D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            for (plane, format) in formats.into_iter().enumerate() {
                let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: if plane == 0 { 0 } else { chroma as u64 },
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: if plane == 0 { width } else { width / 2 },
                        Height: if plane == 0 { height } else { height / 2 },
                        Depth: 1,
                        RowPitch: stride as u32,
                    },
                };
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&buffer),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
                };
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: plane as u32 },
                };
                reader.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            }
            reader.list.ResourceBarrier(&[transition(D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
            reader.list.Close().map_err(|error| failed("readback of a texture", &error))?;
            let list: ID3D12CommandList =
                reader.list.cast().map_err(|error| failed("readback of a texture", &error))?;
            self.queue.ExecuteCommandLists(&[Some(list)]);
            reader.value += 1;
            self.queue.Signal(&reader.fence, reader.value).map_err(|error| failed("readback of a texture", &error))?;
            reader
                .fence
                .SetEventOnCompletion(reader.value, reader.event)
                .map_err(|error| failed("readback of a texture", &error))?;
            WaitForSingleObject(reader.event, INFINITE);
        }
        Ok(mem.cast_const().cast())
    }

    /// The output fence of a decoder that was created shareable, opened through a shared handle as
    /// another device would open it.
    pub fn open_shared_fence(&self, fence: &ID3D12Fence) -> Result<ID3D12Fence, String> {
        // SAFETY: plain calls on live objects; the handle is closed.
        unsafe {
            let handle = self
                .device
                .CreateSharedHandle(fence, None, GENERIC_ALL.0, None)
                .map_err(|error| failed("CreateSharedHandle of the output fence", &error))?;
            let mut opened: Option<ID3D12Fence> = None;
            let result = self.device.OpenSharedHandle(handle, &mut opened);
            let _ = CloseHandle(handle);
            result.map_err(|error| failed("OpenSharedHandle of the output fence", &error))?;
            opened.ok_or_else(|| "OpenSharedHandle of the output fence gave none".to_string())
        }
    }

    /// The decoder is done with a buffer.
    pub fn give_back(&self, token: usize) {
        let index = token.wrapping_sub(1);
        let mut buffers = self.lock();
        if let Some(buffer) = buffers.get_mut(index) {
            buffer.rented = false;
            self.returned.notify_all();
        }
    }

    pub fn rented(&self) -> i32 {
        self.lock().iter().filter(|buffer| buffer.rented).count() as i32
    }
}

impl Drop for OutputPool {
    fn drop(&mut self) {
        for buffer in self.lock().iter_mut() {
            buffer.drop_resource();
        }
        if let Some(reader) = self.reader.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            // SAFETY: the event of the reader, closed once.
            let _ = unsafe { CloseHandle(reader.event) };
        }
    }
}

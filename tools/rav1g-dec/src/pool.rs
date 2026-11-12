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

use std::ffi::c_void;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use crate::device::hex32;

const RENT_WAIT_SECONDS: u64 = 600;

struct Buffer {
    resource: Option<ID3D12Resource>,
    mem: *mut c_void,
    size: usize,
    rented: bool,
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
    /// What the callbacks hand back: `OutputPool::give_back` takes it.
    pub token: *mut c_void,
    /// `ID3D12Resource*` of the readback buffer.
    pub resource: *mut c_void,
    /// The mapped memory of the buffer.
    pub mem: *mut c_void,
    pub size: usize,
}

pub struct OutputPool {
    device: ID3D12Device,
    buffers: Mutex<Vec<Buffer>>,
    returned: Condvar,
}

// SAFETY: the buffers are only touched under the lock; D3D12 devices and resources are free-threaded.
unsafe impl Send for OutputPool {}
unsafe impl Sync for OutputPool {}

impl OutputPool {
    pub fn new(device: &ID3D12Device, count: i32) -> OutputPool {
        let buffers = (0..count.max(1))
            .map(|_| Buffer { resource: None, mem: std::ptr::null_mut(), size: 0, rented: false })
            .collect();
        OutputPool { device: device.clone(), buffers: Mutex::new(buffers), returned: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Buffer>> {
        self.buffers.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits for a free buffer of at least `size` bytes. The error says whether none came back in time or
    /// the device refused the buffer.
    pub fn rent(&self, size: usize) -> Result<Rented, String> {
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
        buffer.rented = true;
        Ok(Rented {
            token: (index + 1) as *mut c_void,
            resource: buffer.resource.as_ref().map_or(std::ptr::null_mut(), |resource| resource.as_raw()),
            mem: buffer.mem,
            size: buffer.size,
        })
    }

    /// The decoder is done with a buffer.
    pub fn give_back(&self, token: *mut c_void) {
        let index = (token as usize).wrapping_sub(1);
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
    }
}

// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The device of the decoders: a D3D12 device, the queue the decoders submit to, and the compute pipeline
//! states of the decoder on that device.

use std::ffi::{CString, c_void};
use std::ptr::NonNull;
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::core::Interface;

use crate::error::{Error, ErrorCode};
use crate::{gpu, sys};

/// The compute pipeline states (`av1_create_pipeline_cache_handle`), destroyed with the last owner.
struct Pipelines(NonNull<c_void>);

// SAFETY: the library is used by the decoders from several threads; it has its own locks.
unsafe impl Send for Pipelines {}
unsafe impl Sync for Pipelines {}

impl Drop for Pipelines {
    fn drop(&mut self) {
        // SAFETY: created by av1_create_pipeline_cache_handle, destroyed once; every decoder that uses the
        // states holds the `Device` they belong to.
        unsafe { sys::av1_destroy_pipeline_cache_handle(self.0.as_ptr()) }
    }
}

struct Inner {
    // Dropped in this order: the states, then the queue, then the device.
    pipelines: Pipelines,
    queue: ID3D12CommandQueue,
    device: ID3D12Device,
}

/// A GPU device for decoders: the D3D12 device, the direct queue the decoders submit their command lists
/// to, and the decoder's compute pipeline states on that device (about 90 shaders; they are created once
/// and shared by every decoder of the device).
///
/// Cloning gives another handle of the same device. A decoder keeps one, so the device, the queue and the
/// pipeline states live as long as any decoder that uses them.
#[derive(Clone)]
pub struct Device {
    inner: Arc<Inner>,
}

impl Device {
    /// A device of the decoder's own: the first hardware adapter, with a direct queue.
    ///
    /// `pipeline_threads`: the pipeline states are created on that many threads (6 at most) while this
    /// call returns; [`Device::wait_pipelines`] waits for them, and so does the first decoder creation.
    /// With 0 they are created by this call.
    pub fn new(pipeline_threads: u32) -> Result<Device, Error> {
        let device = gpu::create_hardware_device().map_err(device_error)?;
        // SAFETY: a reference the call hands over.
        let device = unsafe { ID3D12Device::from_raw(device.into_raw()) };
        Device::with_d3d12(&device, None, pipeline_threads)
    }

    /// The application's D3D12 device. `queue` is the direct command queue the decoders submit to; without
    /// one the decoders get a direct queue of their own. The decoder's frames reach the application's
    /// resources through command lists on this queue, so an application that uses the frames on the GPU
    /// passes the queue it renders with, or waits for the frame's fence.
    pub fn with_d3d12(
        device: &ID3D12Device,
        queue: Option<&ID3D12CommandQueue>,
        pipeline_threads: u32,
    ) -> Result<Device, Error> {
        let queue = match queue {
            Some(queue) => queue.clone(),
            None => {
                let queue = gpu::Device::new(device).create_queue().map_err(device_error)?;
                // SAFETY: a reference the call hands over.
                unsafe { ID3D12CommandQueue::from_raw(queue.into_raw()) }
            }
        };
        // SAFETY: a valid device, which `Inner` keeps for as long as the states live.
        let pipelines = unsafe { sys::av1_create_pipeline_cache_handle(device.as_raw(), pipeline_threads as i32) };
        let Some(pipelines) = NonNull::new(pipelines) else {
            return Err(Error::new(ErrorCode::Error, "the compute pipeline states could not be created"));
        };
        Ok(Device { inner: Arc::new(Inner { pipelines: Pipelines(pipelines), queue, device: device.clone() }) })
    }

    /// As [`Device::with_d3d12`], from the pointers of a C interface.
    ///
    /// # Safety
    /// `device` is a valid `ID3D12Device *`; `queue` is null or a valid `ID3D12CommandQueue *` of that
    /// device.
    pub unsafe fn from_raw(device: *mut c_void, queue: *mut c_void, pipeline_threads: u32) -> Result<Device, Error> {
        // SAFETY: the caller's contract; the references are borrowed and cloned.
        unsafe {
            let Some(device) = ID3D12Device::from_raw_borrowed(&device) else {
                return Err(Error::new(ErrorCode::InvalidParam, "no D3D12 device"));
            };
            Device::with_d3d12(device, ID3D12CommandQueue::from_raw_borrowed(&queue), pipeline_threads)
        }
    }

    /// Waits until every pipeline state exists. An error when one could not be created; the decoder's log
    /// names it.
    pub fn wait_pipelines(&self) -> Result<(), Error> {
        // SAFETY: the handle is valid for as long as `self`.
        let errors = unsafe { sys::av1_wait_pipeline_cache_handle(self.inner.pipelines.0.as_ptr()) };
        if errors == 0 {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::Error, format!("a compute pipeline state could not be created ({errors:#010x})")))
        }
    }

    /// The D3D12 device.
    pub fn d3d12_device(&self) -> &ID3D12Device {
        &self.inner.device
    }

    /// The direct queue the decoders submit to.
    pub fn d3d12_queue(&self) -> &ID3D12CommandQueue {
        &self.inner.queue
    }

    /// One log line with the video memory of the device's adapter ("MEMORY dxgi `label`: ...").
    pub fn log_video_memory(&self, label: &str) {
        let label = CString::new(label.replace('\0', " ")).unwrap_or_default();
        // SAFETY: a valid device and a C string.
        unsafe { sys::av1_log_video_memory_now(self.inner.device.as_raw(), label.as_ptr()) };
    }

    pub(crate) fn pipelines_ptr(&self) -> *mut c_void {
        self.inner.pipelines.0.as_ptr()
    }
}

fn device_error(error: gpu::Error) -> Error {
    Error::new(ErrorCode::Error, format!("{} failed ({:#010x})", error.step, error.code))
}

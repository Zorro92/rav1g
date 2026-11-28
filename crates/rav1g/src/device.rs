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
use std::ops::BitOr;
use std::ptr::NonNull;
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::core::Interface;

use crate::error::{Error, ErrorCode};
use crate::gpu;

/// Which pipeline states a device compiles at its creation ([`DeviceSettings::prepare`]), as a set: the
/// bits of what a stream needs, behind [`Prepare::ONLY`]. A decoder compiles what its stream needs and
/// the device has not prepared when it is created, and waits for those states only; every other state
/// is compiled when a frame or a later sequence needs it (docs/integration.md, "Preparing the pipeline
/// states"). The default, [`Prepare::ALL`] (0), compiles every state up front, as before interface
/// version 4.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Prepare(pub u32);

impl Prepare {
    /// Every state (96), as before interface version 4.
    pub const ALL: Prepare = Prepare(0);
    /// The states of 8-bit streams: prediction, reconstruction, loop filter.
    pub const BIT_DEPTH_8: Prepare = Prepare(1);
    /// The states of streams of more than 8 bits.
    pub const BIT_DEPTH_10: Prepare = Prepare(2);
    /// The display pass ([`crate::Display`]) of SDR frames, of PQ frames, of HLG frames: one state each.
    pub const DISPLAY_SDR: Prepare = Prepare(4);
    pub const DISPLAY_PQ: Prepare = Prepare(8);
    pub const DISPLAY_HLG: Prepare = Prepare(16);
    /// Film grain synthesis.
    pub const FILM_GRAIN: Prepare = Prepare(32);
    /// The super-resolution upscale.
    pub const SUPERRES: Prepare = Prepare(64);
    /// Inter prediction from references of another frame size, of the bit depths named.
    pub const SCALED_REFERENCES: Prepare = Prepare(128);
    /// Only the states of the bits named, and those every stream needs (the transforms, the postfilters,
    /// the copies): without it a set is [`Prepare::ALL`].
    pub const ONLY: Prepare = Prepare(0x4000_0000);

    /// The bits a set may have.
    const KNOWN: u32 = Prepare::BIT_DEPTH_8.0
        | Prepare::BIT_DEPTH_10.0
        | Prepare::DISPLAY_SDR.0
        | Prepare::DISPLAY_PQ.0
        | Prepare::DISPLAY_HLG.0
        | Prepare::FILM_GRAIN.0
        | Prepare::SUPERRES.0
        | Prepare::SCALED_REFERENCES.0
        | Prepare::ONLY.0;

    fn has(self, part: Prepare) -> bool {
        self.0 & part.0 != 0
    }

    /// The classes of the host's states the set names.
    pub(crate) fn classes(self) -> u32 {
        use crate::host::class;
        if !self.has(Prepare::ONLY) {
            return class::ALL;
        }
        let parts = [
            (Prepare::BIT_DEPTH_8, class::DEPTH_8),
            (Prepare::BIT_DEPTH_10, class::DEPTH_10),
            (Prepare::DISPLAY_SDR, class::DISPLAY_SDR),
            (Prepare::DISPLAY_PQ, class::DISPLAY_PQ),
            (Prepare::DISPLAY_HLG, class::DISPLAY_HLG),
            (Prepare::FILM_GRAIN, class::FILM_GRAIN),
            (Prepare::SUPERRES, class::SUPERRES),
        ];
        let mut classes = parts.iter().filter(|(part, _)| self.has(*part)).fold(class::CORE, |all, (_, c)| all | c);
        if self.has(Prepare::SCALED_REFERENCES) {
            if self.has(Prepare::BIT_DEPTH_8) {
                classes |= class::SCALED_8;
            }
            if self.has(Prepare::BIT_DEPTH_10) {
                classes |= class::SCALED_10;
            }
        }
        classes
    }
}

impl BitOr for Prepare {
    type Output = Prepare;
    fn bitor(self, other: Prepare) -> Prepare {
        Prepare(self.0 | other.0)
    }
}

/// What a device is created with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DeviceSettings {
    /// The threads that compile the pipeline states (6 at most; the console's 4 cores give no gain
    /// beyond 4): the prepared states while the creation returns ([`Device::wait_pipelines`] waits for
    /// them), and a decoder creation's on demand next to the creating thread. With 0 the creation compiles
    /// the prepared states itself, and a decoder creation its own.
    pub pipeline_threads: u32,
    /// Which states are compiled at the creation.
    pub prepare: Prepare,
}

/// The compute pipeline states (`av1_create_pipeline_cache_handle`), destroyed with the last owner.
struct Pipelines(NonNull<c_void>);

// SAFETY: the library is used by the decoders from several threads; it has its own locks.
unsafe impl Send for Pipelines {}
unsafe impl Sync for Pipelines {}

impl Drop for Pipelines {
    fn drop(&mut self) {
        // SAFETY: created by av1_create_pipeline_cache_handle, destroyed once; every decoder that uses the
        // states holds the `Device` they belong to.
        unsafe { crate::host::av1_destroy_pipeline_cache_handle(self.0.as_ptr()) }
    }
}

struct Inner {
    // Dropped in this order: the states, then the queue, then the device.
    pipelines: Pipelines,
    queue: ID3D12CommandQueue,
    device: ID3D12Device,
}

/// A GPU device for decoders: the D3D12 device, the direct queue the decoders submit their command lists
/// to, and the decoder's compute pipeline states on that device (96 shaders; each is created once and
/// shared by every decoder of the device: at the creation those of [`DeviceSettings::prepare`], the
/// others when a decoder needs them).
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
    /// With 0 they are created by this call. Every state is prepared ([`Prepare::ALL`]).
    pub fn new(pipeline_threads: u32) -> Result<Device, Error> {
        Device::new_with(&DeviceSettings { pipeline_threads, prepare: Prepare::ALL })
    }

    /// [`Device::new`] with the states of `settings` prepared.
    pub fn new_with(settings: &DeviceSettings) -> Result<Device, Error> {
        let device = gpu::create_hardware_device().map_err(device_error)?;
        // SAFETY: a reference the call hands over.
        let device = unsafe { ID3D12Device::from_raw(device.into_raw()) };
        Device::with_d3d12_settings(&device, None, settings)
    }

    /// The application's D3D12 device. `queue` is the direct command queue the decoders submit to; without
    /// one the decoders get a direct queue of their own. The decoder's frames reach the application's
    /// resources through command lists on this queue, so an application that uses the frames on the GPU
    /// passes the queue it renders with, or waits for the frame's fence. Every state is prepared
    /// ([`Prepare::ALL`]).
    ///
    /// Returns [`ErrorCode::Incapable`] if the device cannot support the Shader Model 6.0 pipelines.
    pub fn with_d3d12(
        device: &ID3D12Device,
        queue: Option<&ID3D12CommandQueue>,
        pipeline_threads: u32,
    ) -> Result<Device, Error> {
        Device::with_d3d12_settings(device, queue, &DeviceSettings { pipeline_threads, prepare: Prepare::ALL })
    }

    /// [`Device::with_d3d12`] with the states of `settings` prepared: an [`ErrorCode::InvalidParam`]
    /// for a [`Prepare`] with a bit it does not know.
    pub fn with_d3d12_settings(
        device: &ID3D12Device,
        queue: Option<&ID3D12CommandQueue>,
        settings: &DeviceSettings,
    ) -> Result<Device, Error> {
        if settings.prepare.0 & !Prepare::KNOWN != 0 {
            return Err(Error::new(
                ErrorCode::InvalidParam,
                format!("prepare {:#x}: bits that are not RAV1G_PREPARE_*", settings.prepare.0),
            ));
        }
        let gpu_device = gpu::Device::new(device);
        let sm6 = gpu_device.supports_shader_model_6_0().map_err(|error| {
            Error::new(
                ErrorCode::Incapable,
                format!("D3D12 Shader Model 6.0 is required: {} failed ({:#010x})", error.step, error.code),
            )
        })?;
        if !sm6 {
            return Err(Error::new(ErrorCode::Incapable, "the D3D12 device does not support Shader Model 6.0"));
        }
        let queue = match queue {
            Some(queue) => queue.clone(),
            None => {
                let queue = gpu_device.create_queue().map_err(device_error)?;
                // SAFETY: a reference the call hands over.
                unsafe { ID3D12CommandQueue::from_raw(queue.into_raw()) }
            }
        };
        let threads = settings.pipeline_threads.min(64) as i32;
        // SAFETY: a valid device, which `Inner` keeps for as long as the states live.
        let pipelines =
            unsafe { crate::host::create_pipeline_library(device.as_raw(), threads, settings.prepare.classes()) };
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
        // SAFETY: the caller's contract.
        unsafe { Device::from_raw_settings(device, queue, &DeviceSettings { pipeline_threads, prepare: Prepare::ALL }) }
    }

    /// As [`Device::with_d3d12_settings`], from the pointers of a C interface.
    ///
    /// # Safety
    /// As for [`Device::from_raw`].
    pub unsafe fn from_raw_settings(
        device: *mut c_void,
        queue: *mut c_void,
        settings: &DeviceSettings,
    ) -> Result<Device, Error> {
        // SAFETY: the caller's contract; the references are borrowed and cloned.
        unsafe {
            let Some(device) = ID3D12Device::from_raw_borrowed(&device) else {
                return Err(Error::new(ErrorCode::InvalidParam, "no D3D12 device"));
            };
            Device::with_d3d12_settings(device, ID3D12CommandQueue::from_raw_borrowed(&queue), settings)
        }
    }

    /// Waits until every pipeline state the device prepared ([`DeviceSettings::prepare`]) exists; the
    /// calling thread compiles those no thread has begun. An error when one could not be created; the
    /// decoder's log names it. The other states are compiled when a decoder needs them.
    pub fn wait_pipelines(&self) -> Result<(), Error> {
        // SAFETY: the handle is valid for as long as `self`.
        let errors = unsafe { crate::host::av1_wait_pipeline_cache_handle(self.inner.pipelines.0.as_ptr()) };
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
        unsafe { crate::host::av1_log_video_memory_now(self.inner.device.as_raw(), label.as_ptr()) };
    }

    pub(crate) fn pipelines_ptr(&self) -> *mut c_void {
        self.inner.pipelines.0.as_ptr()
    }
}

fn device_error(error: gpu::Error) -> Error {
    Error::new(ErrorCode::Error, format!("{} failed ({:#010x})", error.step, error.code))
}

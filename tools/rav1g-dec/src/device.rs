// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The D3D12 device the decoder runs on: the WARP software adapter, or with -hw the first hardware adapter.

use std::ffi::{CStr, c_void};

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{HRESULT, Interface};

use crate::args::Globals;
use crate::say;

pub struct Device {
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
}

impl Device {
    /// `ID3D12Device*` for the decoder.
    pub fn device_ptr(&self) -> *mut c_void {
        self.device.as_raw()
    }
    /// `ID3D12CommandQueue*` for the decoder.
    pub fn queue_ptr(&self) -> *mut c_void {
        self.queue.as_raw()
    }
}

pub fn hex32(hr: HRESULT) -> String {
    format!("0x{:08X}", hr.0 as u32)
}

/// The adapter's name with everything outside printable ASCII as '?'.
fn narrow(wide: &[u16]) -> String {
    wide.iter().take_while(|&&c| c != 0).map(|&c| if (32..127).contains(&c) { c as u8 as char } else { '?' }).collect()
}

/// -debuglayer: what the debug layer has stored since the last call, at most 60 messages.
pub fn print_debug_messages(device: &ID3D12Device) {
    let Ok(queue) = device.cast::<ID3D12InfoQueue>() else {
        return;
    };
    // SAFETY: plain calls on a valid info queue; the message buffer is as large and as aligned as asked.
    unsafe {
        let count = queue.GetNumStoredMessages();
        let dropped = queue.GetNumMessagesDiscardedByMessageCountLimit();
        say!("DEBUGLAYER {} messages stored, {} discarded by the limit", count, dropped);
        for i in 0..count.min(60) {
            let mut size = 0usize;
            let _ = queue.GetMessage(i, None, &mut size);
            if size == 0 {
                continue;
            }
            let mut storage = vec![0u64; size.div_ceil(8)];
            let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
            if queue.GetMessage(i, Some(message), &mut size).is_err() {
                continue;
            }
            let description = (*message).pDescription;
            let text = if description.is_null() {
                String::new()
            } else {
                CStr::from_ptr(description.cast()).to_string_lossy().into_owned()
            };
            say!("DEBUGLAYER [severity {}, id {}] {}", (*message).Severity.0, (*message).ID.0, text);
        }
        queue.ClearStoredMessages();
    }
}

/// Creates the device and its command queue. `None` after an `ERROR ...` line.
pub fn create(globals: &Globals) -> Option<Device> {
    // SAFETY: D3D12 and DXGI calls with valid arguments; every out value is checked.
    unsafe {
        if globals.debug_layer {
            let mut debug: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut debug) {
                Ok(()) if debug.is_some() => {
                    let debug = debug.unwrap();
                    debug.EnableDebugLayer();
                    if globals.gpu_validation
                        && let Ok(debug1) = debug.cast::<ID3D12Debug1>()
                    {
                        debug1.SetEnableGPUBasedValidation(true);
                    }
                    say!("DEBUGLAYER on{}", if globals.gpu_validation { ", with GPU-based validation" } else { "" });
                }
                result => {
                    let hr = result.err().map(|error| error.code()).unwrap_or(HRESULT(0x80004005u32 as i32));
                    say!(
                        "DEBUGLAYER not available (hr {}): the Graphics Tools feature of Windows is not installed",
                        hex32(hr)
                    );
                }
            }
        }
        let factory: IDXGIFactory4 = match CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) {
            Ok(factory) => factory,
            Err(error) => {
                say!("ERROR CreateDXGIFactory2: hr {}", hex32(error.code()));
                return None;
            }
        };
        let mut device: Option<ID3D12Device> = None;
        let adapter: IDXGIAdapter1;
        if globals.hardware {
            let mut found: Option<IDXGIAdapter1> = None;
            let mut hr = HRESULT(0x80004005u32 as i32); // E_FAIL
            let mut index = 0u32;
            while let Ok(candidate) = factory.EnumAdapters1(index) {
                index += 1;
                let Ok(desc) = candidate.GetDesc1() else {
                    continue;
                };
                if desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                    continue;
                }
                match D3D12CreateDevice(&candidate, D3D_FEATURE_LEVEL_11_0, &mut device) {
                    Ok(()) if device.is_some() => {
                        found = Some(candidate);
                        break;
                    }
                    Ok(()) => {}
                    Err(error) => hr = error.code(),
                }
            }
            let Some(found) = found else {
                say!("ERROR -hw: no hardware adapter with a D3D12 device (hr {})", hex32(hr));
                return None;
            };
            adapter = found;
        } else {
            adapter = match factory.EnumWarpAdapter::<IDXGIAdapter1>() {
                Ok(adapter) => adapter,
                Err(error) => {
                    say!("ERROR EnumWarpAdapter: hr {}", hex32(error.code()));
                    return None;
                }
            };
            if let Err(error) = D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device) {
                say!("ERROR D3D12CreateDevice on the WARP adapter: hr {}", hex32(error.code()));
                return None;
            }
        }
        let Some(device) = device else {
            say!("ERROR D3D12CreateDevice returned no device");
            return None;
        };
        let desc = adapter.GetDesc1().unwrap_or_default();
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        let options_result = device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS,
            (&raw mut options).cast(),
            size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS>() as u32,
        );
        say!(
            "ADAPTER {} ({}), vendor 0x{:04X} device 0x{:04X}; shader double precision {}",
            narrow(&desc.Description),
            if desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 { "software" } else { "hardware" },
            desc.VendorId,
            desc.DeviceId,
            if options_result.is_err() {
                "not known"
            } else if options.DoublePrecisionFloatShaderOps.as_bool() {
                "reported"
            } else {
                "not reported"
            }
        );
        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
            ..Default::default()
        };
        let queue: ID3D12CommandQueue = match device.CreateCommandQueue(&queue_desc) {
            Ok(queue) => queue,
            Err(error) => {
                say!("ERROR CreateCommandQueue: hr {}", hex32(error.code()));
                return None;
            }
        };
        Some(Device { device, queue })
    }
}

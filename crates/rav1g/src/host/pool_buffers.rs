// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! A pool can outgrow its initial buffer while tiles still have pointers into it and earlier frames
//! may be reading it on the GPU. New pieces go to a larger buffer at the same logical offsets. At
//! the end of parse, the old pieces of that frame are copied across once, so its command list still
//! binds a single buffer per pool. The initial buffer is released after its last user completes.

use std::sync::{PoisonError, atomic::Ordering};

use windows::Win32::Graphics::Direct3D12::ID3D12Device;

use super::create::to_mib;
use super::log::log;
use super::pool::{CHUNK_ITEMS, Kind};
use super::stage_buffers::MEMORY_GUARD_MARGIN_BUFFER;
use super::types::{Av1Core, GpuBufferObject, av1_frame_thread_data};
use crate::gpu::d3d12::com_ref;
use crate::sys::{MB_MODE_INFO, tx_block_info_gpu};

pub(crate) const fn chunk_bytes(kind: Kind) -> usize {
    CHUNK_ITEMS[kind as usize]
        * match kind {
            Kind::ModeInfo => size_of::<MB_MODE_INFO>(),
            Kind::Coefs => size_of::<i32>(),
            Kind::Txb => size_of::<tx_block_info_gpu>(),
        }
}

/// The logical capacity of the largest frame that fits the decoder.
pub(crate) fn worst(dec: &Av1Core) -> usize {
    ((dec.max_width >> 6) * (dec.max_height >> 6)) as usize
}

pub(crate) fn growth_bytes(dec: &Av1Core) -> usize {
    Kind::ALL.into_iter().map(|kind| (worst(dec) + dec.pool_total[kind as usize]) * chunk_bytes(kind)).sum()
}

/// The buffer a piece came from. It stays mapped until the last frame holding a piece or a
/// recording using the buffer releases it.
///
/// # Safety
/// `dec` is live. The caller holds the piece or is operating on the backing under the pool lock.
pub(crate) unsafe fn backing(dec: *const Av1Core, kind: Kind, grown: bool) -> *mut GpuBufferObject {
    // SAFETY: the caller's contract.
    unsafe {
        if grown {
            (*dec).grown_pools[kind as usize].load(Ordering::Acquire)
        } else {
            match kind {
                Kind::ModeInfo => (*dec).mode_info_pool,
                Kind::Coefs => (*dec).idct_coefs,
                Kind::Txb => (*dec).idct_blocks_pool,
            }
        }
    }
}

/// Allocates a replacement, while the old mapping stays live. Called only on actual exhaustion,
/// under the pool lock.
///
/// # Safety
/// `dec` and `td` are live, and this pool has never grown.
#[cold]
pub(crate) unsafe fn grow(dec: *mut Av1Core, td: *const av1_frame_thread_data, kind: Kind, chunks: usize) -> bool {
    // SAFETY: the caller's contract. The allocator is shared with the recording and completion threads.
    unsafe {
        let _lock = (*dec).memory_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let old = &*backing(dec, kind, false);
        let name = std::ffi::CStr::from_ptr(old.name);
        let frame = (*td).frame_number;
        let bytes = chunks * chunk_bytes(kind);
        if let Some(available) = (*dec).memory_available_cb {
            let available = available((*dec).memory_available_priv);
            if available >= 0 && bytes as u64 + MEMORY_GUARD_MARGIN_BUFFER > available as u64 {
                log(
                    true,
                    &format!(
                        "MEMORY REFUSED pool {} grown for frame {frame}: {bytes} bytes ({:.1} MiB) for the new \
                         backing and the margin of {:.1} MiB exceed the {:.1} MiB the application has left while \
                         the old backing is still in use",
                        name.to_string_lossy(),
                        to_mib(bytes as u64),
                        to_mib(MEMORY_GUARD_MARGIN_BUFFER),
                        to_mib(available as u64)
                    ),
                );
                return false;
            }
        }
        let memory = &mut *(*dec).memory;
        let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device);
        let new = memory.create_buffer(device, bytes, old.memtype, name, -1, false);
        if new.is_null() {
            return false;
        }
        if kind == Kind::Coefs {
            (*new).host_ptr.cast::<u8>().write_bytes(0, (*new).size);
        }
        (*dec).grown_pools[kind as usize].store(new, Ordering::Release);
        log(
            false,
            &format!(
                "MEMORY pool {} grown for frame {frame}: {} to {chunks} chunks, {} to {bytes} bytes; \
                 {} device buffers, {} bytes with both backings live",
                name.to_string_lossy(),
                (*dec).pool_total[kind as usize],
                old.size,
                memory.device_count(),
                memory.device_bytes()
            ),
        );
        true
    }
}

/// Selects the GPU backing of the frame. Called after its last tile, before any recording.
///
/// # Safety
/// `td` is a frame of `dec` whose parse threads have stopped writing its pieces.
pub(crate) unsafe fn parsed(dec: *mut Av1Core, td: *mut av1_frame_thread_data, ok: bool) {
    // SAFETY: the frame owns its chunks until its fence (or failure). A copy touches only its chunks,
    // which no other frame can be writing or reading, even if the other frame is on the GPU.
    unsafe {
        let Some(pools) = (*dec).pools.as_deref() else {
            return;
        };
        let grown = pools.finish_parse((*td).slot_index as usize, ok, |kind, first, chunks| {
            let bytes = chunk_bytes(kind);
            let old = (*backing(dec, kind, false)).host_ptr.cast::<u8>();
            let new = (*backing(dec, kind, true)).host_ptr.cast::<u8>();
            old.add(first * bytes).copy_to_nonoverlapping(new.add(first * bytes), chunks * bytes);
        });
        if ok {
            (*td).parse_buffers = Kind::ALL.map(|kind| backing(dec, kind, grown & (1 << kind as usize) != 0));
        }
    }
}

/// Returns a frame's pieces and retires any initial buffers that no frame can still use.
///
/// # Safety
/// `td`'s parse and GPU work are finished, or it failed and was never submitted.
pub(crate) unsafe fn release(dec: *mut Av1Core, td: &av1_frame_thread_data, submitted: bool) {
    // SAFETY: the caller's contract. Retiring is protected by the same pool lock as allocation and
    // selection of a backing, and can only happen when no parse thread or command list uses it.
    unsafe {
        let Some(pools) = (*dec).pools.as_deref() else {
            return;
        };
        pools.release_with(
            td.slot_index as usize,
            |kind, first, chunks, grown| {
                // Normally the transform shader clears the coefficients it reads.
                if !submitted && kind == Kind::Coefs {
                    let base = (*backing(dec, kind, grown)).host_ptr.cast::<u8>();
                    let bytes = chunk_bytes(kind);
                    base.add(first * bytes).write_bytes(0, chunks * bytes);
                }
            },
            |kind| {
                let _lock = (*dec).memory_lock.lock().unwrap_or_else(PoisonError::into_inner);
                let memory = &mut *(*dec).memory;
                let old = backing(dec, kind, false);
                let name = std::ffi::CStr::from_ptr((*old).name);
                memory.retire_buffer(old);
                log(
                    false,
                    &format!(
                        "MEMORY pool {} initial backing released: {} device buffers, {} bytes",
                        name.to_string_lossy(),
                        memory.device_count(),
                        memory.device_bytes()
                    ),
                );
            },
        );
    }
}

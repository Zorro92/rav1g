// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The decoder's allocator: device buffers with their descriptors in a fixed pool, and the host pool, the
//! one piece of host memory the entropy decoder's structures are cut from. From `libav1/dx/av1_memory.cpp`:
//! the sizes, the alignment, the order of the descriptors and the text of the log lines are those of the
//! C++ host.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI32, Ordering};

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::core::Interface;

use super::log::{log, log_create_error};
use super::types::{DevMemPoolSize, GpuBufferObject, MemoryTypeCount};
use crate::gpu::{self, BufferRef, Device, MemoryKind, MemorySegment, VideoMemory};
use crate::sys::parser::{AV1_POISON_BUFFERS, AV1_POISON_MODE_INFO, AV1_POISON_OFF};

const HOST_MEM_ALIGN: usize = 32;
const DEVICE_MEM_ALIGN: usize = 256;
/// The byte of -poisonmi and -poisonbuf.
const POISON: u8 = 0xA5;

const _: () = assert!(MemoryTypeCount as usize == MemoryKind::COUNT);

fn to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// The name of a memory type in the log lines.
pub(crate) fn memory_type_name(mem: c_int) -> &'static str {
    const NAMES: [&str; MemoryKind::COUNT] =
        ["DeviceOnly", "DeviceUpload", "HostRW", "ReadBack", "DeviceOnlyConst", "UploadUAV"];
    usize::try_from(mem).ok().and_then(|mem| NAMES.get(mem)).copied().unwrap_or("unknown")
}

// ---------------------------------------------------------------------------------------------------------
// Video memory of the adapter (IDXGIAdapter3::QueryVideoMemoryInfo), for the MEMORY log lines.
// ---------------------------------------------------------------------------------------------------------

fn write_segment(line: &mut String, name: &str, segment: &MemorySegment) {
    if segment.error < 0 {
        let _ = write!(line, "{name} QueryVideoMemoryInfo failed (hr 0x{:08X})", segment.error as u32);
    } else {
        let _ = write!(
            line,
            "{name} budget {:.1} MiB, usage {:.1} MiB, reserved {:.1} MiB of {:.1} MiB available",
            to_mib(segment.budget),
            to_mib(segment.usage),
            to_mib(segment.reservation),
            to_mib(segment.available_for_reservation)
        );
    }
}

/// One "MEMORY dxgi <label>: ..." line; says so when the query failed.
pub(crate) fn log_video_memory(is_error: bool, label: &str, memory: &VideoMemory) {
    if memory.adapter_error < 0 {
        log(
            is_error,
            &format!(
                "MEMORY dxgi {label}: not available, no IDXGIAdapter3 for the device (hr 0x{:08X})",
                memory.adapter_error as u32
            ),
        );
        return;
    }
    let mut line = format!("MEMORY dxgi {label}: ");
    write_segment(&mut line, "local", &memory.local);
    line.push_str("; ");
    write_segment(&mut line, "non-local", &memory.non_local);
    log(is_error, &line);
}

fn label_text(label: *const c_char) -> String {
    if label.is_null() {
        String::new()
    } else {
        // SAFETY: a label is a C string (the callers' contract).
        unsafe { CStr::from_ptr(label) }.to_string_lossy().into_owned()
    }
}

/// `av1_log_video_memory_now` (`dx/av1_perf.h`): the line for the device, now.
///
/// # Safety
/// `d3d12device` is null or an `ID3D12Device *`; `label` is null or a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_log_video_memory_now(d3d12device: *mut c_void, label: *const c_char) {
    // SAFETY: the caller's contract.
    let memory = Device::video_memory(unsafe { ID3D12Device::from_raw_borrowed(&d3d12device) });
    log_video_memory(false, &label_text(label), &memory);
}

// ---------------------------------------------------------------------------------------------------------
// Debugging aid of the test programs (-poisonmi, -poisonbuf; dx/av1_perf.h): the buffers the CPU writes for
// the shaders start filled with a byte that is not 0 instead of with what the memory held. 0 (the default):
// nothing is filled. The fill is right behind the creation of a buffer, so everything the decoder clears or
// initialises afterwards (the coefficients, which have to be 0 between two frames, the grids cleared per
// frame) is as in a run without it.
// ---------------------------------------------------------------------------------------------------------

static POISON_BUFFERS: AtomicI32 = AtomicI32::new(AV1_POISON_OFF);

/// `av1_set_poison_buffers` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_poison_buffers(level: c_int) {
    POISON_BUFFERS.store(level, Ordering::Relaxed);
}

/// The constants of the stages are different: their memory is used again in every frame, so a fill at
/// creation is gone after the first frame, and what a record then starts with is the record an earlier
/// frame had at the same place, mostly a plausible one. With -poisonbuf every record is filled again when
/// it is handed out, before its writer has written anything (`av1_poison_constants` of the C++).
///
/// # Safety
/// `ptr` is valid for writing `size` bytes.
pub(crate) unsafe fn poison_constants(ptr: *mut c_void, size: usize) {
    if POISON_BUFFERS.load(Ordering::Relaxed) == AV1_POISON_BUFFERS {
        // SAFETY: the caller's contract.
        unsafe { ptr.cast::<u8>().write_bytes(POISON, size) };
    }
}

/// The buffer starts filled with the poison byte.
fn poisoned(kind: MemoryKind, name: &CStr) -> bool {
    let level = POISON_BUFFERS.load(Ordering::Relaxed);
    if level == AV1_POISON_OFF {
        false
    } else if level == AV1_POISON_MODE_INFO {
        name == c"mode_info_pool"
    } else {
        // Written by the CPU, read by the GPU. A readback buffer goes the other way.
        matches!(kind, MemoryKind::HostRw | MemoryKind::DeviceUpload | MemoryKind::UploadUav)
    }
}

// ---------------------------------------------------------------------------------------------------------
// The allocator.
// ---------------------------------------------------------------------------------------------------------

/// The next multiple of `align` (a power of two) from `value`.
const fn align_up(value: usize, align: usize) -> usize {
    (value.wrapping_add(align - 1)) & !(align - 1)
}

/// What a request of the host pool takes at `offset`: the gap in front of it and its size with the gap,
/// rounded up to the alignment of the pool. `align` below that alignment counts as that alignment.
fn host_request(offset: usize, size: usize, align: c_int) -> (usize, usize) {
    let align = usize::try_from(align).unwrap_or(0).max(HOST_MEM_ALIGN);
    let gap = align_up(offset, align) - offset;
    (gap, align_up(size.wrapping_add(gap), HOST_MEM_ALIGN))
}

/// The allocator of a decoder (`av1_memory_allocator` of the C++ host).
pub(crate) struct Allocator {
    /// The descriptors of the device buffers, in the order of their creation. The decoder keeps pointers
    /// to them, so the allocator does not move while it has buffers.
    pub dev_buffer_pool: [GpuBufferObject; DevMemPoolSize as usize],
    pub dev_obj_count: c_int,
    pub dev_bytes: [usize; MemoryKind::COUNT],
    pub dev_count: [c_int; MemoryKind::COUNT],
    pub host_base_addr: *mut u8,
    pub host_max_size: usize,
    pub host_offset: usize,
}

impl Allocator {
    /// An allocator without buffers on the host pool `host_mem`, which starts at the first address in it
    /// that is a multiple of 256.
    ///
    /// # Safety
    /// `host_mem` is valid for `host_size` bytes for as long as the allocator is used.
    pub(crate) unsafe fn new(host_mem: *mut u8, host_size: usize) -> Allocator {
        let offset = (align_up(host_mem as usize, 256) - host_mem as usize).min(host_size);
        Allocator {
            // SAFETY: a descriptor is pointers and numbers; all zero is the empty one.
            dev_buffer_pool: unsafe { std::mem::zeroed() },
            dev_obj_count: 0,
            dev_bytes: [0; MemoryKind::COUNT],
            dev_count: [0; MemoryKind::COUNT],
            host_base_addr: host_mem.wrapping_add(offset),
            host_max_size: host_size - offset,
            host_offset: 0,
        }
    }

    /// [`Allocator::new`] in an allocation of its own, which the buffer descriptors stay in. `None` when
    /// the system has no memory for it.
    ///
    /// # Safety
    /// As for [`Allocator::new`].
    pub(crate) unsafe fn new_boxed(host_mem: *mut u8, host_size: usize) -> Option<Box<Allocator>> {
        // SAFETY: the layout has a size; the allocation is written whole before the box is made.
        unsafe {
            let allocator = std::alloc::alloc(std::alloc::Layout::new::<Allocator>()).cast::<Allocator>();
            if allocator.is_null() {
                return None;
            }
            allocator.write(Allocator::new(host_mem, host_size));
            Some(Box::from_raw(allocator))
        }
    }

    /// The bytes of the device buffers.
    pub(crate) fn device_bytes(&self) -> usize {
        self.dev_bytes.iter().sum()
    }

    /// The descriptors of the device buffers that exist.
    pub(crate) fn buffers(&self) -> &[GpuBufferObject] {
        &self.dev_buffer_pool[..usize::try_from(self.dev_obj_count).unwrap_or(0)]
    }

    /// A device buffer of `size` bytes, rounded up to 256, with the next descriptor of the pool. Returns
    /// null when the allocation fails, after a "MEMORY FAILED" line that names it.
    ///
    /// # Safety
    /// `device` is the device of the decoder, or none.
    pub(crate) unsafe fn create_buffer(
        &mut self,
        device: Option<&ID3D12Device>,
        size: usize,
        mem: c_int,
        name: &'static CStr,
        slot: c_int,
    ) -> *mut GpuBufferObject {
        let name_text = name.to_string_lossy();
        let owner = if slot >= 0 { format!(" of slot {slot}") } else { String::new() };
        let aligned_size = align_up(size, DEVICE_MEM_ALIGN);
        let total = self.device_bytes();
        let type_name = memory_type_name(mem);
        let before = |count: c_int| {
            format!("allocated before it: {total} bytes ({:.1} MiB) in {count} buffers", to_mib(total as u64))
        };

        // The pool of buffer descriptors is fixed. Refuse instead of writing behind it.
        if self.dev_obj_count >= DevMemPoolSize as c_int {
            log_create_error(&format!(
                "MEMORY FAILED buffer {name_text}{owner}: {aligned_size} bytes ({:.1} MiB), type {type_name}: the \
                 decoder's pool of {} buffer descriptors is full; {}",
                to_mib(aligned_size as u64),
                DevMemPoolSize as c_int,
                before(self.dev_obj_count)
            ));
            return std::ptr::null_mut();
        }

        let count = self.dev_obj_count;
        let obj = &mut self.dev_buffer_pool[count as usize];
        obj.dev = std::ptr::null_mut();
        obj.host_ptr = std::ptr::null_mut();
        obj.memtype = mem;
        obj.size = aligned_size;
        obj.name = name.as_ptr();
        obj.slot = slot;

        let kind = MemoryKind::from_raw(mem);
        let created = match (kind, device) {
            (Some(kind), Some(device)) => Device::new(device).create_buffer(aligned_size as u64, kind),
            _ => Err(gpu::Error { step: "CreateCommittedResource", code: E_POINTER.0 }),
        };
        let buffer = match created {
            Ok(buffer) => buffer,
            Err(error) => {
                // Nothing of a failed buffer stays behind: its descriptor is not counted and release()
                // never sees it.
                log_create_error(&format!(
                    "MEMORY FAILED buffer {name_text}{owner}: {aligned_size} bytes ({:.1} MiB), type {type_name}, \
                     heap {}, {} hr 0x{:08X}; {}",
                    to_mib(aligned_size as u64),
                    kind.map_or("unknown", Device::heap_name),
                    error.step,
                    error.code as u32,
                    before(count)
                ));
                return std::ptr::null_mut();
            }
        };
        let kind = kind.expect("a buffer was created for a kind");
        if let Some(mapped) = buffer.mapped() {
            obj.host_ptr = mapped.as_ptr().cast();
            if poisoned(kind, name) {
                // SAFETY: the mapping of the new buffer, `aligned_size` bytes.
                unsafe { mapped.as_ptr().write_bytes(POISON, aligned_size) };
            }
        }
        obj.dev = buffer.into_raw().cast();

        self.dev_obj_count += 1;
        self.dev_bytes[kind as usize] += aligned_size;
        self.dev_count[kind as usize] += 1;
        obj
    }

    /// `size` bytes of the host pool at an offset that is a multiple of `align` (of 32 at least). Returns
    /// null when the pool is too small, after a "MEMORY FAILED" line.
    pub(crate) fn host_allocate(&mut self, size: usize, align: c_int) -> *mut c_void {
        let (gap, size) = host_request(self.host_offset, size, align);
        if self.host_offset.wrapping_add(size) > self.host_max_size {
            log_create_error(&format!(
                "MEMORY FAILED host pool: {size} bytes asked, {} of {} bytes in use (the pool is sized by \
                 av1_codec_query_memory_requirements; its configuration must be the one the decoder is created with)",
                self.host_offset, self.host_max_size
            ));
            return std::ptr::null_mut();
        }
        let ptr = self.host_base_addr.wrapping_add(self.host_offset + gap);
        self.host_offset += size;
        ptr.cast()
    }

    /// Releases the buffers of one memory type and returns their bytes and their number. For a release
    /// that is logged type by type; [`Allocator::release`] must follow it before the allocator is used
    /// again.
    ///
    /// # Safety
    /// No buffer of the type is in use.
    pub(crate) unsafe fn release_type(&mut self, mem: c_int) -> (usize, c_int) {
        let mut bytes = 0;
        let mut released = 0;
        let count = usize::try_from(self.dev_obj_count).unwrap_or(0);
        for obj in &mut self.dev_buffer_pool[..count] {
            if obj.dev.is_null() || obj.memtype != mem {
                continue;
            }
            if !obj.host_ptr.is_null() {
                // SAFETY: the descriptor holds the reference of create_buffer.
                if let Some(buffer) = unsafe { BufferRef::from_raw(&obj.dev) } {
                    buffer.unmap();
                }
                obj.host_ptr = std::ptr::null_mut();
            }
            // SAFETY: the reference of create_buffer; the descriptor forgets the pointer.
            unsafe { BufferRef::release(obj.dev.cast()) };
            obj.dev = std::ptr::null_mut();
            bytes += obj.size;
            released += 1;
        }
        if let Some(kind) = MemoryKind::from_raw(mem) {
            self.dev_bytes[kind as usize] = 0;
            self.dev_count[kind as usize] = 0;
        }
        (bytes, released)
    }

    /// Releases every buffer. The descriptors are free again afterwards.
    ///
    /// # Safety
    /// No buffer is in use.
    pub(crate) unsafe fn release(&mut self) {
        for mem in 0..MemoryKind::COUNT as c_int {
            // SAFETY: the caller's contract.
            unsafe { self.release_type(mem) };
        }
        self.dev_obj_count = 0;
    }
}

/// The plan of a decoder's memory (`av1_memory_allocator_dummy` of the C++): counts what the requests of
/// the creation would take of the host pool and of device memory, without allocating.
#[derive(Default)]
pub(crate) struct Plan {
    pub host: usize,
    pub device: usize,
}

impl Plan {
    /// Counts a device buffer of `size` bytes.
    pub(crate) fn create_buffer(&mut self, size: usize) {
        let gap = align_up(self.device, DEVICE_MEM_ALIGN) - self.device;
        self.device += align_up(size.wrapping_add(gap), DEVICE_MEM_ALIGN);
    }

    /// Counts a request of `size` bytes of the host pool.
    pub(crate) fn host_allocate(&mut self, size: usize, align: c_int) {
        self.host += host_request(self.host, size, align).1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic of the C++ (`av1_memory_allocator::host_allocate`), as it was written.
    fn host_request_cpp(host_offset: usize, mut size: usize, align: c_int) -> (usize, usize) {
        let align = (align.max(32) - 1) as usize;
        let addr_offset = ((host_offset + align) & !align) - host_offset;
        size += addr_offset;
        size = (size + 31) & !31;
        (addr_offset, size)
    }

    #[test]
    fn host_requests_as_in_cpp() {
        for offset in [0usize, 32, 96, 1024, 4064, 65504] {
            for size in [0usize, 1, 31, 32, 33, 100, 4096, 1 << 20] {
                for align in [0, 1, 16, 32, 64, 256, 4096] {
                    assert_eq!(host_request(offset, size, align), host_request_cpp(offset, size, align));
                }
            }
        }
    }

    #[test]
    fn planning_counts_like_the_allocator() {
        let mut plan = Plan::default();
        let mut host = 0usize;
        let mut device = 0usize;
        for (size, align) in [(1usize, 0), (4096, 64), (100, 32), (65536, 4096), (7, 0)] {
            plan.host_allocate(size, align);
            plan.create_buffer(size);
            let (gap, taken) = host_request(host, size, align);
            assert_eq!((host + gap) % (align.max(32) as usize), 0);
            host += taken;
            device += align_up(size, 256);
        }
        assert_eq!(plan.host, host);
        assert_eq!(plan.device, device);
    }

    #[test]
    fn the_host_pool_starts_at_a_multiple_of_256_and_refuses_what_does_not_fit() {
        let mut memory = vec![0u8; 4096];
        // SAFETY: the vector lives to the end of the test.
        let mut allocator = unsafe { Allocator::new(memory.as_mut_ptr().wrapping_add(1), 4095) };
        assert_eq!(allocator.host_base_addr as usize % 256, 0);
        assert!(allocator.host_max_size <= 4095 && allocator.host_max_size >= 4095 - 255);
        let first = allocator.host_allocate(100, 0);
        let second = allocator.host_allocate(1, 64);
        assert_eq!(first, allocator.host_base_addr.cast());
        assert_eq!(second as usize - first as usize, 128);
        assert_eq!(allocator.host_offset, 128 + 32);
        // More than is left: null (and a MEMORY FAILED line), nothing taken.
        assert!(allocator.host_allocate(4096, 0).is_null());
        assert_eq!(allocator.host_offset, 128 + 32);
        assert_eq!((allocator.device_bytes(), allocator.buffers().len()), (0, 0));
        // SAFETY: no buffers.
        unsafe { allocator.release() };
    }

    #[test]
    fn type_names() {
        let name = memory_type_name;
        assert_eq!(name(0), "DeviceOnly");
        assert_eq!(name(2), "HostRW");
        assert_eq!(name(5), "UploadUAV");
        assert_eq!(name(-1), "unknown");
        assert_eq!(name(6), "unknown");
    }

    #[test]
    fn poison_levels() {
        av1_set_poison_buffers(AV1_POISON_OFF);
        assert!(!poisoned(MemoryKind::HostRw, c"mode_info_pool"));
        av1_set_poison_buffers(AV1_POISON_MODE_INFO);
        assert!(poisoned(MemoryKind::HostRw, c"mode_info_pool"));
        assert!(!poisoned(MemoryKind::HostRw, c"cb_alloc"));
        av1_set_poison_buffers(AV1_POISON_BUFFERS);
        assert!(poisoned(MemoryKind::DeviceUpload, c"cb_alloc"));
        assert!(poisoned(MemoryKind::UploadUav, c"idct_coefs"));
        assert!(!poisoned(MemoryKind::ReadBack, c"readback"));
        assert!(!poisoned(MemoryKind::DeviceOnly, c"frame_buffer_pool"));
        av1_set_poison_buffers(AV1_POISON_OFF);
    }
}

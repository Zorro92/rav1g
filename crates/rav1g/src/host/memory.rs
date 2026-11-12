// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The decoder's allocator: device buffers with their descriptors in a fixed pool, and the host pool the
//! application gives the decoder. From `libav1/dx/av1_memory.cpp`.
//!
//! The allocator is the C++ struct `av1_memory_allocator`, which the frame pipeline creates in the host
//! pool and calls through its member functions; the glue (`crates/rav1g-sys/glue/av1_memory.cpp`) has
//! those as calls of the `rav1g_memory_*` functions here. The sizes, the alignment, the order of the
//! descriptors and the text of the log lines are those of the C++.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI32, Ordering};

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::core::Interface;

use super::log::{log, log_create_error};
use crate::gpu::d3d12::com_ref;
use crate::gpu::{self, BufferRef, Device, MemoryKind, MemorySegment, VideoMemory};
use crate::sys::host::{
    AV1_POISON_BUFFERS, AV1_POISON_MODE_INFO, AV1_POISON_OFF, DevMemPoolSize, GpuBufferObject, MemoryTypeCount,
    av1_memory_allocator, av1_memory_allocator_dummy, av1_video_memory,
};

const HOST_MEM_ALIGN: usize = 32;
const DEVICE_MEM_ALIGN: usize = 256;
/// The byte of -poisonmi and -poisonbuf.
const POISON: u8 = 0xA5;

const _: () = assert!(MemoryTypeCount as usize == MemoryKind::COUNT);

fn to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// `av1_memory_type_name` of the C++.
#[unsafe(no_mangle)]
pub extern "C" fn rav1g_memory_type_name(mem: c_int) -> *const c_char {
    const NAMES: [&CStr; MemoryKind::COUNT] =
        [c"DeviceOnly", c"DeviceUpload", c"HostRW", c"ReadBack", c"DeviceOnlyConst", c"UploadUAV"];
    usize::try_from(mem).ok().and_then(|mem| NAMES.get(mem)).map_or(c"unknown", |name| name).as_ptr()
}

// ---------------------------------------------------------------------------------------------------------
// Video memory of the adapter (IDXGIAdapter3::QueryVideoMemoryInfo), for the MEMORY log lines.
// ---------------------------------------------------------------------------------------------------------

/// `av1_query_video_memory` of the C++: budget and usage of the adapter the device is on.
///
/// # Safety
/// `device` is null or an `ID3D12Device *`; `info` is valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_query_video_memory(device: *mut c_void, info: *mut av1_video_memory) {
    // SAFETY: the caller's contract.
    let memory = Device::video_memory(unsafe { ID3D12Device::from_raw_borrowed(&device) });
    // SAFETY: `info` is valid for writing; all of it is plain numbers, zero where a query failed.
    unsafe {
        info.write_bytes(0, 1);
        let info = &mut *info;
        info.hr_adapter = memory.adapter_error;
        for (segment, hr, raw) in [
            (&memory.local, &mut info.hr_local, &mut info.local),
            (&memory.non_local, &mut info.hr_non_local, &mut info.non_local),
        ] {
            *hr = segment.error;
            raw.Budget = segment.budget;
            raw.CurrentUsage = segment.usage;
            raw.AvailableForReservation = segment.available_for_reservation;
            raw.CurrentReservation = segment.reservation;
        }
    }
}

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
fn log_video_memory(is_error: bool, label: &str, memory: &VideoMemory) {
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

/// `av1_log_video_memory` of the C++.
///
/// # Safety
/// `label` is null or a C string; `info` was filled by [`rav1g_query_video_memory`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_log_video_memory(is_error: c_int, label: *const c_char, info: *const av1_video_memory) {
    // SAFETY: the caller's contract.
    let info = unsafe { &*info };
    let segment = |hr: i32, raw: &crate::sys::host::DXGI_QUERY_VIDEO_MEMORY_INFO| MemorySegment {
        error: hr,
        budget: raw.Budget,
        usage: raw.CurrentUsage,
        available_for_reservation: raw.AvailableForReservation,
        reservation: raw.CurrentReservation,
    };
    let memory = VideoMemory {
        adapter_error: info.hr_adapter,
        local: segment(info.hr_local, &info.local),
        non_local: segment(info.hr_non_local, &info.non_local),
    };
    log_video_memory(is_error != 0, &label_text(label), &memory);
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
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn rav1g_poison_constants(ptr: *mut c_void, size: usize) {
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

/// `av1_memory_allocator::setup`: the allocator starts empty on the host pool `host_mem`.
///
/// # Safety
/// `this` is the allocator; `host_mem` is valid for `host_size` bytes for as long as the allocator is used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_setup(this: *mut av1_memory_allocator, host_mem: *mut u8, host_size: usize) {
    // SAFETY: the caller's contract.
    let this = unsafe { &mut *this };
    let offset = align_up(host_mem as usize, 256) - host_mem as usize;
    this.host_base_addr = host_mem.wrapping_add(offset);
    this.host_max_size = host_size.wrapping_sub(offset);
    this.host_offset = 0;
    this.shader_obj_count = 0;
    this.host_obj_count = 0;
    this.dev_obj_count = 0;
    // SAFETY: a descriptor is pointers and numbers; all zero is the empty one.
    this.dev_buffer_pool = unsafe { std::mem::zeroed() };
    this.dev_bytes = [0; MemoryKind::COUNT];
    this.dev_count = [0; MemoryKind::COUNT];
}

fn device_bytes(this: &av1_memory_allocator) -> usize {
    this.dev_bytes.iter().sum()
}

/// `av1_memory_allocator::device_bytes()`: the bytes of all device buffers.
///
/// # Safety
/// `this` is the allocator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_device_bytes(this: *const av1_memory_allocator) -> usize {
    // SAFETY: the caller's contract.
    device_bytes(unsafe { &*this })
}

/// `av1_memory_allocator::create_buffer`: a device buffer of `size` bytes, rounded up to 256, with the
/// next descriptor of the pool. Returns null when the allocation fails, after a "MEMORY FAILED" line that
/// names it.
///
/// # Safety
/// `this` is the allocator, set up, with the device context of the decoder; `name` is null or a C string
/// that lives as long as the buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_create_buffer(
    this: *mut av1_memory_allocator,
    size: usize,
    mem: c_int,
    name: *const c_char,
    slot: c_int,
) -> *mut GpuBufferObject {
    // SAFETY: the caller's contract.
    let this = unsafe { &mut *this };
    let name = if name.is_null() { c"unnamed".as_ptr() } else { name };
    // SAFETY: a C string (the caller's contract, or the literal).
    let name_c = unsafe { CStr::from_ptr(name) };
    let name_text = name_c.to_string_lossy();
    let owner = if slot >= 0 { format!(" of slot {slot}") } else { String::new() };
    let aligned_size = align_up(size, DEVICE_MEM_ALIGN);
    let total = device_bytes(this);
    // SAFETY: a static C string.
    let type_name = unsafe { CStr::from_ptr(rav1g_memory_type_name(mem)) }.to_string_lossy();
    let before = |count: c_int| {
        format!("allocated before it: {total} bytes ({:.1} MiB) in {count} buffers", to_mib(total as u64))
    };

    // The pool of buffer descriptors is fixed. Refuse instead of writing behind it.
    if this.dev_obj_count >= DevMemPoolSize as c_int {
        log_create_error(&format!(
            "MEMORY FAILED buffer {name_text}{owner}: {aligned_size} bytes ({:.1} MiB), type {type_name}: the \
             decoder's pool of {} buffer descriptors is full; {}",
            to_mib(aligned_size as u64),
            DevMemPoolSize as c_int,
            before(this.dev_obj_count)
        ));
        return std::ptr::null_mut();
    }

    let count = this.dev_obj_count;
    let obj = &mut this.dev_buffer_pool[count as usize];
    obj.dev = std::ptr::null_mut();
    obj.host_ptr = std::ptr::null_mut();
    obj.memtype = mem;
    obj.size = aligned_size;
    obj.name = name;
    obj.slot = slot;

    let kind = MemoryKind::from_raw(mem);
    // SAFETY: the context is the decoder's, with its device or null.
    let device = unsafe { this.context.as_ref().and_then(|context| com_ref::<_, ID3D12Device>(&context.device)) };
    let created = match (kind, device) {
        (Some(kind), Some(device)) => Device::new(device).create_buffer(aligned_size as u64, kind),
        _ => Err(gpu::Error { step: "CreateCommittedResource", code: E_POINTER.0 }),
    };
    let buffer = match created {
        Ok(buffer) => buffer,
        Err(error) => {
            // Nothing of a failed buffer stays behind: its descriptor is not counted and release() never
            // sees it.
            log_create_error(&format!(
                "MEMORY FAILED buffer {name_text}{owner}: {aligned_size} bytes ({:.1} MiB), type {type_name}, heap \
                 {}, {} hr 0x{:08X}; {}",
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
        if poisoned(kind, name_c) {
            // SAFETY: the mapping of the new buffer, `aligned_size` bytes.
            unsafe { mapped.as_ptr().write_bytes(POISON, aligned_size) };
        }
    }
    obj.dev = buffer.into_raw().cast();

    this.dev_obj_count += 1;
    this.dev_bytes[kind as usize] += aligned_size;
    this.dev_count[kind as usize] += 1;
    obj
}

/// `av1_memory_allocator::host_allocate`: `size` bytes of the host pool at an offset that is a multiple
/// of `align` (of 32 at least). Returns null when the pool is too small, after a "MEMORY FAILED" line.
///
/// # Safety
/// `this` is the allocator, set up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_host_allocate(
    this: *mut av1_memory_allocator,
    size: usize,
    align: c_int,
) -> *mut c_void {
    // SAFETY: the caller's contract.
    let this = unsafe { &mut *this };
    let (gap, size) = host_request(this.host_offset, size, align);
    if this.host_offset.wrapping_add(size) > this.host_max_size {
        log_create_error(&format!(
            "MEMORY FAILED host pool: {size} bytes asked, {} of {} bytes in use (the pool is sized by \
             av1_codec_query_memory_requirements; its configuration must be the one the decoder is created with)",
            this.host_offset, this.host_max_size
        ));
        return std::ptr::null_mut();
    }
    let ptr = this.host_base_addr.wrapping_add(this.host_offset + gap);
    this.host_offset += size;
    ptr.cast()
}

fn release_type(this: &mut av1_memory_allocator, mem: c_int) -> (usize, c_int) {
    let mut bytes = 0;
    let mut released = 0;
    let count = usize::try_from(this.dev_obj_count).unwrap_or(0);
    for obj in &mut this.dev_buffer_pool[..count] {
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
        this.dev_bytes[kind as usize] = 0;
        this.dev_count[kind as usize] = 0;
    }
    (bytes, released)
}

/// `av1_memory_allocator::release_type`: releases the buffers of one memory type and returns their bytes
/// (and their number in `count`). For a release that is logged type by type; `release` must follow it
/// before the allocator is used again.
///
/// # Safety
/// `this` is the allocator; `count` is null or valid for writing. No buffer of the type is in use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_release_type(
    this: *mut av1_memory_allocator,
    mem: c_int,
    count: *mut c_int,
) -> usize {
    // SAFETY: the caller's contract.
    let (bytes, released) = release_type(unsafe { &mut *this }, mem);
    if !count.is_null() {
        // SAFETY: the caller's contract.
        unsafe { *count = released };
    }
    bytes
}

/// `av1_memory_allocator::release`: releases every buffer. The descriptors are free again afterwards.
///
/// # Safety
/// `this` is the allocator. No buffer is in use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_release(this: *mut av1_memory_allocator) {
    // SAFETY: the caller's contract.
    let this = unsafe { &mut *this };
    for mem in 0..MemoryKind::COUNT as c_int {
        release_type(this, mem);
    }
    this.dev_obj_count = 0;
}

/// `av1_memory_allocator_dummy::create_buffer`: counts what the buffer would take. The planning pass of
/// the frame pipeline sizes the decoder with it.
///
/// # Safety
/// `this` is the planning allocator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_dummy_create_buffer(this: *mut av1_memory_allocator_dummy, size: usize) {
    // SAFETY: the caller's contract.
    let this = unsafe { &mut *this };
    let gap = align_up(this.device_ptr, DEVICE_MEM_ALIGN) - this.device_ptr;
    this.device_ptr += align_up(size.wrapping_add(gap), DEVICE_MEM_ALIGN);
}

/// `av1_memory_allocator_dummy::host_allocate`: counts what the request would take of the host pool.
///
/// # Safety
/// `this` is the planning allocator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_memory_dummy_host_allocate(
    this: *mut av1_memory_allocator_dummy,
    size: usize,
    align: c_int,
) {
    // SAFETY: the caller's contract.
    let this = unsafe { &mut *this };
    this.host_ptr += host_request(this.host_ptr, size, align).1;
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
        // SAFETY: the planning allocator is numbers and a table pointer the functions do not use.
        let mut plan: av1_memory_allocator_dummy = unsafe { std::mem::zeroed() };
        let mut host = 0usize;
        let mut device = 0usize;
        for (size, align) in [(1usize, 0), (4096, 64), (100, 32), (65536, 4096), (7, 0)] {
            // SAFETY: `plan` is a planning allocator.
            unsafe {
                rav1g_memory_dummy_host_allocate(&mut plan, size, align);
                rav1g_memory_dummy_create_buffer(&mut plan, size);
            }
            let (gap, taken) = host_request(host, size, align);
            assert_eq!((host + gap) % (align.max(32) as usize), 0);
            host += taken;
            device += align_up(size, 256);
        }
        assert_eq!(plan.host_ptr, host);
        assert_eq!(plan.device_ptr, device);
    }

    #[test]
    fn type_names() {
        // SAFETY: static C strings.
        let name = |mem| unsafe { CStr::from_ptr(rav1g_memory_type_name(mem)) }.to_str().unwrap();
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

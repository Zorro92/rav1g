// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Test program flag -dumpstages (`av1_set_stage_dump` of `dx/av1_perf.h`): copies of the buffers behind
//! the stages of a frame's command list. A copy is recorded into a readback buffer of its own right
//! behind the stage, and handed to the callback by the completion thread when the frame's fence is done.
//! Inert until a callback and a list of frames are set.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::sync::RwLock;

use windows::Win32::Graphics::Direct3D12::ID3D12Device;

use super::core::commands;
use super::log::log;
use crate::gpu::d3d12::{com_ref, com_release, com_store, wrapped_ref};
use crate::gpu::{BufferRef, BufferState, Device, Readback};
use crate::sys;
use crate::sys::host::{
    AV1_COMMON, Av1Core, Av1StageDumpMax, HwFrameBuffer, HwOutputImage, ID3D12Resource, MB_MODE_INFO,
    av1_frame_thread_data, av1_stage_dump_entry, av1_stage_dump_region, av1_tile_data,
};

/// Frames a dump can be asked for (`StageDumpFramesMax`).
const FRAMES_MAX: usize = 256;

/// What `av1_set_stage_dump` set: the callback, its context and the frames to dump.
struct Wanted {
    callback: unsafe extern "C" fn(*mut c_void, *const sys::av1_stage_dump),
    /// The context of the callback, a pointer of the application.
    context: usize,
    frames: Vec<c_int>,
}

static WANTED: RwLock<Option<Wanted>> = RwLock::new(None);

/// `av1_set_stage_dump` (`dx/av1_perf.h`): the callback that gets the copies, and the numbers of the
/// frames to dump (at most 256). Without a callback or without frames nothing is dumped.
///
/// # Safety
/// `frames` is null or valid for `count` numbers; `cb` may be called with `private` from the completion
/// thread of any decoder until it is replaced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_set_stage_dump(
    cb: sys::av1_stage_dump_cb_fn_t,
    private: *mut c_void,
    frames: *const c_int,
    count: c_int,
) {
    let mut wanted = WANTED.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    *wanted = None;
    let (Some(callback), false, Ok(count)) = (cb, frames.is_null(), usize::try_from(count)) else {
        return;
    };
    if count == 0 {
        return;
    }
    // SAFETY: the caller's contract.
    let frames = unsafe { std::slice::from_raw_parts(frames, count.min(FRAMES_MAX)) }.to_vec();
    *wanted = Some(Wanted { callback, context: private as usize, frames });
}

fn frame_wanted(frame_number: c_int) -> bool {
    let wanted = WANTED.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    wanted.as_ref().is_some_and(|wanted| wanted.frames.contains(&frame_number))
}

/// Releases what a dump holds: its readback buffer and the text of its layout.
///
/// # Safety
/// `e` is a dump of a slot.
unsafe fn release_entry(e: &mut av1_stage_dump_entry) {
    // SAFETY: the readback field is null or holds the buffer; the layout is null or a string of
    // `CString::into_raw` ([`stage_dump_data`]).
    unsafe {
        com_release(&raw mut e.readback);
        if !e.layout.is_null() {
            drop(CString::from_raw(e.layout));
        }
    }
    e.layout = std::ptr::null_mut();
}

/// Start of a command list: is its frame dumped. Copies of a frame that was never submitted are dropped
/// here.
///
/// # Safety
/// `td` is the slot (or output context) whose frame the decoder starts to record.
pub(crate) unsafe fn stage_dump_begin(td: &mut av1_frame_thread_data, is_sef: c_int) {
    for i in 0..usize::try_from(td.dump_count).unwrap_or(0) {
        // SAFETY: a dump of the slot.
        unsafe { release_entry(&mut td.dumps[i]) };
    }
    td.dump_count = 0;
    td.dump_is_sef = is_sef;
    td.dump_info[0] = 0;
    td.dump_active = c_int::from(frame_wanted(td.frame_number));
}

/// A new dump of the frame of the current command list: a readback buffer of `size` bytes for its copies.
///
/// # Safety
/// `dec` is the decoder while it records a frame; the result is used before the next dump is made.
unsafe fn new_entry<'a>(dec: *mut Av1Core, stage: &'static CStr, size: usize) -> Option<&'a mut av1_stage_dump_entry> {
    // SAFETY: the caller's contract.
    unsafe {
        let td = (*dec).curr_frame_data.as_mut()?;
        if td.dump_active == 0 || size == 0 {
            return None;
        }
        let stage_text = stage.to_string_lossy();
        if td.dump_count >= Av1StageDumpMax {
            log(true, &format!("DUMPSTAGES frame {}: no room for the dump of stage {stage_text}", td.frame_number));
            return None;
        }
        let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device)?;
        let readback = match Device::new(device).create_readback(size as u64) {
            Ok(readback) => readback,
            Err(error) => {
                log(
                    true,
                    &format!(
                        "DUMPSTAGES frame {}: no readback buffer of {size} bytes for stage {stage_text} (hr 0x{:08X})",
                        td.frame_number, error.code as u32
                    ),
                );
                return None;
            }
        };
        let e = &mut td.dumps[td.dump_count as usize];
        com_store(&raw mut e.readback, readback.into_raw());
        e.planes = std::mem::zeroed();
        e.stage = stage.as_ptr();
        e.layout = std::ptr::null_mut();
        e.kind = 0;
        e.bytes_per_sample = if td.is_hbd != 0 { 2 } else { 1 };
        e.bit_depth = td.bitdepth;
        e.size = size;
        td.dump_count += 1;
        Some(e)
    }
}

/// Records the copy of `size` bytes of `src` from `offset` to `dst_offset` of the dump's readback
/// buffer. `uav`: `src` is a buffer the shaders write, so it is a copy source for the length of the copy.
/// Otherwise it is a buffer the GPU only reads (upload or host memory), which is a copy source as it is.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `e` is a dump of that frame and `src` a buffer of the
/// decoder.
unsafe fn copy_region(
    dec: *mut Av1Core,
    e: &av1_stage_dump_entry,
    dst_offset: usize,
    src: *mut ID3D12Resource,
    offset: usize,
    size: usize,
    uav: bool,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let (Some(readback), Some(src)) = (wrapped_ref::<_, Readback>(&e.readback), BufferRef::from_raw(&src)) else {
            return;
        };
        let commands = commands(dec);
        if uav {
            commands.transition(src, BufferState::ReadWrite, BufferState::CopySource);
        }
        commands.copy_buffer(readback.buffer(), dst_offset as u64, src, offset as u64, size as u64);
        if uav {
            commands.transition(src, BufferState::CopySource, BufferState::ReadWrite);
        }
    }
}

/// Records the copy of `size` bytes of `src` from `offset` into a readback buffer of its own. `src` is a
/// buffer the shaders write.
///
/// # Safety
/// As for [`copy_region`].
unsafe fn copy<'a>(
    dec: *mut Av1Core,
    stage: &'static CStr,
    src: *mut ID3D12Resource,
    offset: usize,
    size: usize,
) -> Option<&'a mut av1_stage_dump_entry> {
    // SAFETY: the caller's contract.
    unsafe {
        let e = new_entry(dec, stage, size)?;
        copy_region(dec, e, 0, src, offset, size, true);
        Some(e)
    }
}

/// `av1_stage_dump_data` of the C++: the buffers a stage reads besides the frame (kind 2). The regions
/// are copied one behind the other into one dump; `layout` is the text that describes them.
///
/// # Safety
/// `dec` is the decoder while it records a frame; the regions are of buffers of it.
pub(crate) unsafe fn stage_dump_data(
    dec: *mut Av1Core,
    stage: &'static CStr,
    layout: &str,
    regions: &[av1_stage_dump_region],
) {
    let size = regions.iter().map(|region| region.size).sum();
    // SAFETY: the caller's contract.
    unsafe {
        let Some(e) = new_entry(dec, stage, size) else {
            return;
        };
        e.kind = 2;
        e.bytes_per_sample = 0;
        // The text up to its first 0, as a C string keeps it.
        let text = layout.split('\0').next().unwrap_or("");
        e.layout = CString::new(text).map_or(std::ptr::null_mut(), CString::into_raw);
        let mut pos = 0;
        for region in regions {
            if region.size == 0 {
                continue;
            }
            copy_region(dec, e, pos, region.src, region.offset, region.size, region.uav != 0);
            pos += region.size;
        }
    }
}

/// `av1_stage_dump_mode_info` of the C++: the mode info the stages of the frame read: the grid (one
/// pointer per 4x4 unit, rows of mi_stride) and the records it points to. Of the pool only the records of
/// the frame's tiles are copied, tile by tile: the layout lists, per run of records, the pool index of
/// its first record and the number of records.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's.
pub(crate) unsafe fn stage_dump_mode_info(dec: *mut Av1Core, cm: *const AV1_COMMON) {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(td) = (*dec).curr_frame_data.as_ref() else {
            return;
        };
        if td.dump_active == 0 {
            return;
        }
        let cm = &*cm;
        let grid_object = &*td.mode_info_grid;
        let pool = &*(*dec).mode_info_pool;
        let record = size_of::<MB_MODE_INFO>();
        let grid_bytes =
            (size_of::<*mut MB_MODE_INFO>() * cm.mi_stride as usize * cm.mi_rows as usize).min(grid_object.size);
        let text = format!(
            "buffer=mi_grid entry={} mi_stride={} mi_cols={} mi_rows={} pool_base={} record={record}\n",
            size_of::<*mut MB_MODE_INFO>(),
            cm.mi_stride,
            cm.mi_cols,
            cm.mi_rows,
            pool.host_ptr as usize as u32
        );
        let grid = av1_stage_dump_region { src: grid_object.dev, offset: 0, size: grid_bytes, uav: 0 };
        stage_dump_data(dec, c"mi_grid", &text, &[grid]);

        let tile_count = usize::try_from(td.tile_count).unwrap_or(0);
        let pool_records = pool.size / record;
        let mut regions = Vec::with_capacity(tile_count * 2);
        let mut layout = format!("buffer=mi_pool record={record}\n");
        // The tiles of the frame's slot, then what they continued with in a second slot's share.
        let second: *const av1_tile_data = td.sec_thread_data.as_ref().map_or(std::ptr::null(), |sec| sec.tile_data);
        for tiles in [td.tile_data.cast_const(), second] {
            if tiles.is_null() {
                continue;
            }
            for tile in std::slice::from_raw_parts(tiles, tile_count) {
                let (first, records) = (tile.mi_offset, tile.mi_count);
                if records <= 0 || first < 0 || first as usize + records as usize > pool_records {
                    continue;
                }
                regions.push(av1_stage_dump_region {
                    src: pool.dev,
                    offset: record * first as usize,
                    size: record * records as usize,
                    uav: 0,
                });
                let _ = writeln!(layout, "run first={first} count={records}");
            }
        }
        if !regions.is_empty() {
            stage_dump_data(dec, c"mi_pool", &layout, &regions);
        }
    }
}

/// `av1_stage_dump_fb` of the C++: a frame buffer in its current layout, whole, borders and all, from its
/// start in the pool. `dims`: the buffer whose visible size the copy has, when `fb` is a back buffer
/// that only has a layout; null: `fb`.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `fb` and `dims` are frame buffers of it.
pub(crate) unsafe fn stage_dump_fb(
    dec: *mut Av1Core,
    stage: &'static CStr,
    fb: *const HwFrameBuffer,
    dims: *const HwFrameBuffer,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let fb = &*fb;
        let dims = dims.as_ref().unwrap_or(fb);
        let Some(e) = copy(dec, stage, (*(*dec).frame_buffer_pool).dev, fb.base_offset, fb.size) else {
            return;
        };
        for p in 0..3 {
            e.planes[p].offset = (fb.planes[p].offset as usize).wrapping_sub(fb.base_offset) as u32;
            e.planes[p].stride = fb.planes[p].stride as u32;
            e.planes[p].width = (if p != 0 { dims.uv_crop_width } else { dims.y_crop_width }) as u32;
            e.planes[p].height = (if p != 0 { dims.uv_crop_height } else { dims.y_crop_height }) as u32;
        }
    }
}

/// `av1_stage_dump_residuals` of the C++: the residual buffer in the layout of the frame.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
pub(crate) unsafe fn stage_dump_residuals(dec: *mut Av1Core, stage: &'static CStr) {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(td) = (*dec).curr_frame_data.as_ref() else {
            return;
        };
        if td.dump_active == 0 {
            return;
        }
        let fb = &*td.frame_buffer;
        let residuals = &*(*dec).idct_residuals;
        // The residual planes of the frame: rows of res_stride bytes from res_offset, 128-aligned in both
        // directions.
        let size = fb.planes[2].res_offset as usize
            + fb.planes[2].res_stride as usize * ((((fb.height >> 1) + 127) & !127) as usize);
        let Some(e) = copy(dec, stage, residuals.dev, 0, size.min(residuals.size)) else {
            return;
        };
        e.kind = 1;
        e.bytes_per_sample = 2;
        for p in 0..3 {
            e.planes[p].offset = fb.planes[p].res_offset as u32;
            e.planes[p].stride = fb.planes[p].res_stride as u32;
            e.planes[p].width = (if p != 0 { fb.uv_crop_width } else { fb.y_crop_width }) as u32;
            e.planes[p].height = (if p != 0 { fb.uv_crop_height } else { fb.y_crop_height }) as u32;
        }
    }
}

/// `av1_stage_dump_output` of the C++: the output image, which is put together at the start of
/// `back_buffer1` in the layout of the image.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `img` is the image of the frame.
pub(crate) unsafe fn stage_dump_output(
    dec: *mut Av1Core,
    stage: &'static CStr,
    img: *const HwOutputImage,
    size: usize,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let img = &*img;
        let Some(e) = copy(dec, stage, (*(*dec).frame_buffer_pool).dev, (*dec).back_buffer1.base_offset, size) else {
            return;
        };
        for p in 0..(if img.monochrome != 0 { 1 } else { 3 }) {
            e.planes[p].offset = img.planes[p].offset as u32;
            e.planes[p].stride = img.planes[p].stride as u32;
            e.planes[p].width = (if p != 0 { img.uv_crop_width } else { img.y_crop_width }) as u32;
            e.planes[p].height = (if p != 0 { img.uv_crop_height } else { img.y_crop_height }) as u32;
        }
    }
}

/// Writes the line about the frame of a slot's dumps, cut to what the slot keeps of it.
pub(crate) fn set_dump_info(td: &mut av1_frame_thread_data, text: &str) {
    let room = td.dump_info.len() - 1;
    let bytes = &text.as_bytes()[..text.len().min(room)];
    for (dst, &src) in td.dump_info.iter_mut().zip(bytes) {
        *dst = src as c_char;
    }
    td.dump_info[bytes.len()] = 0;
}

/// One line about the frame for its dumps: what a reader needs to know which stages ran and on what.
///
/// # Safety
/// `dec` is the decoder while it records the frame of `cm`.
pub(crate) unsafe fn stage_dump_frame_info(dec: *mut Av1Core, cm: *const AV1_COMMON) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        if td.dump_active == 0 {
            return;
        }
        let cm = &*cm;
        const FRAME_TYPES: [&str; 4] = ["key", "inter", "intra_only", "switch"];
        let frame_type = cm.current_frame.frame_type as usize;
        let intra_only = frame_type == 0 || frame_type == 2; // frame_is_intra_only(): KEY_FRAME, INTRA_ONLY_FRAME
        // Short names: the line has to fit the 255 characters a dump file keeps of it, references included.
        let mut line = format!(
            "type={} show={} size={}x{} upscaled_width={} denom={} bits={} q={} lf={},{},{},{} cdef={} superres={} \
             lr={} grain={} tiles={}x{} scaled={} refs=",
            FRAME_TYPES.get(frame_type).copied().unwrap_or("?"),
            cm.show_frame,
            cm.width,
            cm.height,
            cm.superres_upscaled_width,
            cm.superres_scale_denominator,
            td.bitdepth,
            cm.base_qindex,
            cm.lf.filter_level[0],
            cm.lf.filter_level[1],
            cm.lf.filter_level_u,
            cm.lf.filter_level_v,
            td.do_cdef,
            td.do_superres,
            td.do_loop_rest,
            td.do_filmgrain,
            cm.tile_cols,
            cm.tile_rows,
            td.scale_enable
        );
        for i in 0..7 {
            if line.len() >= td.dump_info.len() {
                break;
            }
            let separator = if i != 0 { "," } else { "" };
            let reference = if intra_only { None } else { td.refs[i].as_ref() };
            match reference {
                // frame number : size
                Some(r) => {
                    let _ = write!(line, "{separator}{}:{}x{}", r.frame_number, r.y_crop_width, r.y_crop_height);
                }
                None => {
                    let _ = write!(line, "{separator}-");
                }
            }
        }
        set_dump_info(td, &line);
    }
}

/// Completion thread, after the frame's fence: hands the copies over and releases them.
///
/// # Safety
/// `td` is the slot of a frame that has completed on the GPU and is not back in its pool.
pub(crate) unsafe fn stage_dump_deliver(td: &mut av1_frame_thread_data) {
    let (callback, context) = {
        let wanted = WANTED.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        wanted.as_ref().map_or((None, 0), |wanted| (Some(wanted.callback), wanted.context))
    };
    for i in 0..usize::try_from(td.dump_count).unwrap_or(0) {
        let info = td.dump_info.as_ptr();
        let (frame_number, is_sef) = (td.frame_number, td.dump_is_sef);
        let e = &mut td.dumps[i];
        if let Some(callback) = callback {
            // SAFETY: the readback buffer of the dump, whose copies have run; the callback gets pointers
            // that are valid for the call.
            let delivered = unsafe {
                wrapped_ref::<_, Readback>(&e.readback).and_then(|readback| {
                    readback.read(e.size, |data| {
                        let mut dump: sys::av1_stage_dump = std::mem::zeroed();
                        dump.frame_number = frame_number;
                        dump.index = i as c_int;
                        dump.stage = e.stage;
                        dump.kind = e.kind;
                        dump.bytes_per_sample = e.bytes_per_sample;
                        dump.bit_depth = e.bit_depth;
                        dump.is_sef = is_sef;
                        dump.info = info;
                        for (dst, src) in dump.planes.iter_mut().zip(&e.planes) {
                            *dst = sys::av1_stage_dump_plane {
                                offset: src.offset,
                                stride: src.stride,
                                width: src.width,
                                height: src.height,
                            };
                        }
                        dump.data = data.as_ptr();
                        dump.size = e.size;
                        dump.layout = e.layout;
                        callback(context as *mut c_void, &dump);
                    })
                })
            };
            if delivered.is_none() {
                // SAFETY: the name of a stage is a static C string.
                let stage = unsafe { CStr::from_ptr(e.stage) }.to_string_lossy();
                log(
                    true,
                    &format!("DUMPSTAGES frame {frame_number}: the dump of stage {stage} could not be read back"),
                );
            }
        }
        // SAFETY: a dump of the slot.
        unsafe { release_entry(e) };
    }
    td.dump_count = 0;
    td.dump_active = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn nothing(_: *mut c_void, _: *const sys::av1_stage_dump) {}

    #[test]
    fn the_frames_to_dump() {
        let frames = [3, 7, 11];
        // SAFETY: a list of three numbers and a callback that does nothing.
        unsafe { av1_set_stage_dump(Some(nothing), std::ptr::null_mut(), frames.as_ptr(), 3) };
        assert!(frame_wanted(7) && frame_wanted(11) && !frame_wanted(4));
        // Without a callback, without frames or with no count nothing is wanted.
        // SAFETY: as above.
        unsafe { av1_set_stage_dump(None, std::ptr::null_mut(), frames.as_ptr(), 3) };
        assert!(!frame_wanted(7));
        // SAFETY: as above.
        unsafe { av1_set_stage_dump(Some(nothing), std::ptr::null_mut(), frames.as_ptr(), 0) };
        assert!(!frame_wanted(7));
        // SAFETY: as above; no list.
        unsafe { av1_set_stage_dump(Some(nothing), std::ptr::null_mut(), std::ptr::null(), 3) };
        assert!(!frame_wanted(7));
    }
}

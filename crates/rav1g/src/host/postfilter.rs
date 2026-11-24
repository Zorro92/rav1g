// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The stages between the loop filter and the output: CDEF, superres and loop restoration, and the
//! order they run in with the frame buffers they read and write. From `av1_cdef_looprestoration` and
//! the functions it calls in `libav1/dx/loopfilters.cpp`.

use std::ffi::{c_int, c_void};

use super::core::{
    alloc_constants, buffer, commands, perf_gpu_mark, pipeline, stage_dump_active, stage_dump_data, stage_dump_fb,
};
use super::types::{Av1Core, av1_stage_dump_region};
use crate::gpu::d3d12::{com_ref, wrapped_ref};
use crate::gpu::{BufferState, Device, PlaneViews};
use crate::sys::{
    AV1_COMMON, AV1_PERF_GPU_CDEF, AV1_PERF_GPU_LOOP_REST, AV1_PERF_GPU_SUPERRES, AV1LrStruct, HwFrameBuffer,
    MI_SIZE_64X64, RESTORE_NONE, RESTORE_WIENER,
};
use windows::Win32::Graphics::Direct3D12::ID3D12Device;

/// Direct semi-planar (NV12/P010) texture target fused into the last postfilter stage (CDEF, superres,
/// or loop restoration), avoiding a separate full-frame `copy_plane_sp_tex_v2` pass.
#[derive(Clone, Copy)]
pub(crate) struct FusedTarget {
    pub pair: u32,
    pub write_fb_y: bool,
    pub write_fb_uv: bool,
    pub out_luma: bool,
    pub out_chroma: bool,
    pub is_monochrome: bool,
}

fn plane_words(buffer: &HwFrameBuffer) -> [[c_int; 4]; 3] {
    buffer.planes.map(|plane| [plane.stride, plane.offset, plane.res_stride, plane.res_offset])
}

/// Binds the plane UAV descriptor table for a fused semi-planar output pass.
///
/// # Safety
/// `dec` is the decoder with its device and `plane_views`; `pair` is a valid pair written for this frame.
unsafe fn bind_fused_planes(dec: *mut Av1Core, root: u32, pair: u32) {
    // SAFETY: the caller's contract.
    unsafe {
        let commands = commands(dec);
        let views = wrapped_ref::<_, PlaneViews>(&(*dec).plane_views).expect("direct semi-planar has plane views");
        let device =
            Device::new(com_ref::<_, ID3D12Device>(&(*dec).compute.device).expect("decoder has a D3D12 device"));
        commands.bind_write_planes(root, device, views, pair);
    }
}

// ---------------------------------------------------------------------------------------------------------
// Superres.
// ---------------------------------------------------------------------------------------------------------

/// The constants of `upscaling.hlsl`.
#[repr(C)]
struct UpscaleData {
    src_planes: [[c_int; 4]; 3],
    dst_planes: [[c_int; 4]; 3],
}

/// `av1_superres` of the C++: upscales `src` into `dst`, which has the layout of the upscaled frame.
/// `iter` names the stage dump: 2 is the copy loop restoration reads.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `src` and `dst` are frame buffers of it.
unsafe fn superres(
    dec: *mut Av1Core,
    src: *const HwFrameBuffer,
    dst: *const HwFrameBuffer,
    iter: c_int,
    fused: Option<FusedTarget>,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let (src, dst_buffer) = (&*src, &*dst);

        let constants = alloc_constants::<UpscaleData>(&mut td.command_buffer);
        let data = constants.data;
        (*data).src_planes = plane_words(src);
        (*data).dst_planes = plane_words(dst_buffer);
        (*data).dst_planes[0][2] = dst_buffer.y_crop_width;
        (*data).dst_planes[1][2] = dst_buffer.uv_crop_width;
        (*data).dst_planes[2][2] = dst_buffer.uv_crop_width;
        (*data).src_planes[0][2] = src.y_crop_width;
        (*data).src_planes[1][2] = src.uv_crop_width;
        (*data).src_planes[2][2] = src.uv_crop_width;
        (*data).src_planes[0][3] = src.width;
        (*data).src_planes[1][3] = (src.width + 1) >> 1;
        (*data).src_planes[2][3] = (src.width + 1) >> 1;

        if let Some(target) = fused {
            let shader = pipeline(&raw const (*(*dec).shader_lib).shader_upscale_sp_tex);
            commands.set_pipeline(shader);
            commands.set_layout(shader.layout());
            commands.bind_write(0, frame_buffers.location());
            commands.bind_constants(1, constants.location);
            bind_fused_planes(dec, 3, target.pair);

            if target.write_fb_y || target.out_luma {
                let w = dst_buffer.y_crop_width;
                let h = dst_buffer.y_crop_height;
                let wi_count = h * ((w + 3) >> 2);
                let flags = c_int::from(target.write_fb_y) | (c_int::from(target.out_luma) << 1);
                // plane, wi_count, hbd, height, out_flags
                commands.set_inline_constants(2, &[0, wi_count, dst_buffer.hbd, h, flags], 0);
                commands.dispatch(((wi_count + 63) >> 6) as u32, 1, 1);
            }
            if target.write_fb_uv || target.out_chroma {
                let w = dst_buffer.uv_crop_width;
                let h = dst_buffer.uv_crop_height;
                let wi_count = h * ((w + 3) >> 2);
                let flags = c_int::from(target.write_fb_uv)
                    | (c_int::from(target.out_chroma) << 1)
                    | (c_int::from(target.is_monochrome) << 2);
                commands.set_inline_constants(2, &[1, wi_count, dst_buffer.hbd, h, flags], 0);
                commands.dispatch(((wi_count + 63) >> 6) as u32, 1, 1);
            }
            if target.write_fb_y || target.write_fb_uv || stage_dump_active(dec) {
                commands.write_barrier(frame_buffers);
            }
        } else {
            let shader = pipeline(&raw const (*(*dec).shader_lib).shader_upscale);
            commands.set_pipeline(shader);
            commands.set_layout(shader.layout());
            commands.bind_write(0, frame_buffers.location());
            commands.bind_constants(1, constants.location);
            for plane in 0..3 {
                let w = if plane != 0 { dst_buffer.uv_crop_width } else { dst_buffer.y_crop_width };
                let h = if plane != 0 { dst_buffer.uv_crop_height } else { dst_buffer.y_crop_height };
                let wi_count = h * ((w + 3) >> 2);
                // plane, wi_count, hbd
                commands.set_inline_constants(2, &[plane, wi_count, dst_buffer.hbd], 0);
                commands.dispatch(((wi_count + 63) >> 6) as u32, 1, 1);
            }
            commands.write_barrier(frame_buffers);
        }

        perf_gpu_mark(dec, AV1_PERF_GPU_SUPERRES);
        stage_dump_fb(dec, if iter == 2 { c"superres_lr" } else { c"superres" }, dst, std::ptr::null());
    }
}

// ---------------------------------------------------------------------------------------------------------
// CDEF.
// ---------------------------------------------------------------------------------------------------------

/// The constants shared by the CDEF filter shaders.
#[repr(C)]
struct CdefData {
    plane: [c_int; 4],
    uv_stride: c_int,
    dst_offset: [c_int; 3],

    uv_offset: [c_int; 2],
    index_stride: c_int,
    skips_stride: c_int,

    pri_damping: c_int,
    sec_damping: c_int,
    pli: c_int,
    hbd: c_int,

    bit_depth: c_int,
    out_crop_w: c_int,
    out_crop_h: c_int,
    out_flags: c_int,

    cdef_directions: [[[c_int; 4]; 2]; 16],
    cdef_strength: [[c_int; 4]; 16],
    cdef_uv_strength: [[c_int; 4]; 16],
}

// The two bytes of TX_INFO_WORD that cdef_lists.hlsl reads.
const _: () = {
    assert!(std::mem::offset_of!(crate::sys::MB_MODE_INFO, skip) == 60 + 1);
    assert!(std::mem::offset_of!(crate::sys::MB_MODE_INFO, cdef_strength) == 60 + 2);
    assert!(size_of::<crate::sys::MiGridEntry>() == 4);
};

/// Whether the frame needs the full skip table (`cdef_lists.hlsl`): only a stage dump of `cdef_info`
/// reads it; CDEF reads the masks.
///
/// # Safety
/// `dec` is the decoder while it records a frame.
unsafe fn cdef_needs_skips(dec: *mut Av1Core) -> bool {
    // SAFETY: the caller's contract.
    unsafe { stage_dump_active(dec) }
}

/// The lists of CDEF by the GPU (`Av1Core::gpu_cdef_lists`, `AV1_PARSER_OPTION_GPU_CDEF_LISTS`):
/// `cdef_lists_v2.hlsl` writes what the loops of [`cdef_filter_run`] write, the same words at the same
/// places and no other: the index of each of the `nhfb` by `nvfb` 64x64 blocks, and the mask of the 8x8
/// blocks CDEF filters per 64x32 group. For a stage dump of `cdef_info`, `cdef_lists.hlsl` also writes
/// the skip flag of each of the `skip_cols` by `skip_rows` 8x8 blocks, in rows of `nhfb * 16`. It reads the
/// mode info as the loop filter's list shaders do, from the grid of the slot and the pool, which hold the frame's
/// records until the frame's fence; the thread that records then reads none of it.
///
/// The two lists are buffers the CPU could write (a custom heap, in the state the shaders read them in)
/// that a command list may write as well: in the state for it around this pass only, so CDEF and the
/// stage dump find them as they find the lists the CPU wrote.
///
/// The lists move to the state for writing and the shaders are dispatched; [`record_cdef_lists_end`]
/// is the barrier behind them. With the merged front of the frame ([`cdef_lists_front`]) they are in the
/// step of gen_pred_blocks.
///
/// # Safety
/// `dec` is the decoder while it records a frame whose slot has the lists as buffers the GPU may write;
/// `cm` is the frame's. `sizes`: `nhfb`, `nvfb`, `skip_cols`, `skip_rows` of the frame.
unsafe fn record_cdef_lists_begin(dec: *mut Av1Core, cm: &AV1_COMMON, sizes: [c_int; 4]) {
    let [nhfb, nvfb, skip_cols, skip_rows] = sizes;
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let commands = commands(dec);
        let lib = (*dec).shader_lib;
        let indexes = buffer(td.cdef_indexes);
        let inline = [cm.mi_stride, crate::sys::MI_GRID_BASE as c_int, nhfb, nhfb * 16, skip_cols, skip_rows];
        let mi_pool = buffer(td.parse_buffers[super::pool::Kind::ModeInfo as usize]).location();
        let mi_grid = buffer(td.mode_info_grid).location();

        // The full skip table only when -dumpstages dumps cdef_info.
        let need_skips = cdef_needs_skips(dec);
        // The transitions first, one barrier, then the dispatches: nothing between two dispatches of the
        // step.
        commands.transition(indexes, BufferState::Read, BufferState::ReadWrite);
        commands.transition(buffer(td.cdef_masks), BufferState::Read, BufferState::ReadWrite);
        if need_skips {
            commands.transition(buffer(td.cdef_skips), BufferState::Read, BufferState::ReadWrite);
        }
        let masks = buffer(td.cdef_masks);
        let shader = pipeline(&raw const (*lib).shader_cdef_lists_v2);
        commands.set_layout(shader.layout());
        commands.set_pipeline(shader);
        commands.bind_read(0, mi_pool);
        commands.bind_read(1, mi_grid);
        commands.bind_write(2, indexes.location());
        commands.bind_write(3, masks.location());
        commands.set_inline_constants(4, &inline, 0);
        commands.dispatch(nhfb as u32, nvfb as u32, 1);
        if need_skips {
            // Both shaders write the indexes (the same words).
            commands.write_barrier(indexes);
            let skips = buffer(td.cdef_skips);
            let shader = pipeline(&raw const (*lib).shader_cdef_lists);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_read(0, mi_pool);
            commands.bind_read(1, mi_grid);
            commands.bind_write(2, indexes.location());
            commands.bind_write(3, skips.location());
            commands.set_inline_constants(4, &inline, 0);
            commands.dispatch(nhfb as u32, nvfb as u32, 1);
        }
    }
}

/// The barrier behind [`record_cdef_lists_begin`]: the lists back to the state CDEF reads them in.
///
/// # Safety
/// As for [`record_cdef_lists_begin`], behind it.
unsafe fn record_cdef_lists_end(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &*(*dec).curr_frame_data;
        let commands = commands(dec);
        commands.transition(buffer(td.cdef_masks), BufferState::ReadWrite, BufferState::Read);
        if cdef_needs_skips(dec) {
            commands.transition(buffer(td.cdef_skips), BufferState::ReadWrite, BufferState::Read);
        }
        commands.transition(buffer(td.cdef_indexes), BufferState::ReadWrite, BufferState::Read);
    }
}

/// The sizes [`record_cdef_lists_begin`] takes: `nhfb`, `nvfb`, `skip_cols`, `skip_rows` of the frame.
fn cdef_list_sizes(cm: &AV1_COMMON) -> [c_int; 4] {
    let mi_size_64 = MI_SIZE_64X64 as c_int;
    let nvfb = (cm.mi_rows + mi_size_64 - 1) / mi_size_64;
    let nhfb = (cm.mi_cols + mi_size_64 - 1) / mi_size_64;
    let skip_rows = (nvfb * 16).min(cm.mi_rows / 2);
    let skip_cols = (nhfb * 16).min(cm.mi_cols / 2);
    [nhfb, nvfb, skip_cols, skip_rows]
}

/// The merged front of the frame (`frame::record_front`): the CDEF lists by the GPU depend on the mode
/// info alone, so they are written in the step of gen_pred_blocks instead of a step of their own in
/// front of CDEF (AV1 7.15: `cdef_idx` and `skip` of the blocks). Their transitions come first, so the
/// caller records this in front of the step's other dispatches. Returns whether it recorded them; the
/// caller then records [`cdef_lists_front_end`] with that step's barrier.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's; behind the grid build.
pub(crate) unsafe fn cdef_lists_front(dec: *mut Av1Core, cm: &AV1_COMMON) -> bool {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        td.cdef_lists_front = 0;
        if td.do_cdef == 0 || (*dec).gpu_cdef_lists == 0 {
            return false;
        }
        record_cdef_lists_begin(dec, cm, cdef_list_sizes(cm));
        td.cdef_lists_front = 1;
        true
    }
}

/// The barrier behind [`cdef_lists_front`].
///
/// # Safety
/// As for [`cdef_lists_front`], which returned true, with the stage dump unchanged.
pub(crate) unsafe fn cdef_lists_front_end(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    unsafe { record_cdef_lists_end(dec) };
}

/// `av1_cdef_filter_run` of the C++: CDEF from `src` into `dst`, when the frame has it.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's; `src` and `dst` are frame buffers.
unsafe fn cdef_filter_run(
    dec: *mut Av1Core,
    cm: &AV1_COMMON,
    src: *const HwFrameBuffer,
    dst: *const HwFrameBuffer,
    fused: Option<FusedTarget>,
) {
    // SAFETY: the caller's contract; the lists are mapped buffers of the slot, sized for the largest
    // frame of the decoder, and the mode info grid covers the frame.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        if td.do_cdef == 0 {
            return;
        }
        let cdef_info = &cm.cdef_info;
        let mi_size_64 = MI_SIZE_64X64 as c_int;
        let [nhfb, nvfb, skip_cols, skip_rows] = cdef_list_sizes(cm);
        let gpu_lists = (*dec).gpu_cdef_lists != 0;
        if !gpu_lists {
            let cdef_indexes = (*td.cdef_indexes).host_ptr.cast::<c_int>();
            let cdef_skips = (*td.cdef_skips).host_ptr.cast::<c_int>();
            let cdef_masks = (*td.cdef_masks).host_ptr.cast::<u32>();
            let mode_info = |row: c_int, col: c_int| {
                &*crate::sys::mi_grid_record(cm.mi, *cm.mi_grid_visible.offset((row * cm.mi_stride + col) as isize))
            };
            for fbr in 0..nvfb {
                for fbc in 0..nhfb {
                    *cdef_indexes.offset((fbr * nhfb + fbc) as isize) =
                        c_int::from(mode_info(mi_size_64 * fbr, mi_size_64 * fbc).cdef_strength);
                }
            }
            std::ptr::write_bytes(cdef_masks, 0, (2 * nvfb * nhfb) as usize);
            let write_skips = stage_dump_active(dec);
            for r in 0..skip_rows {
                for c in 0..skip_cols {
                    let skipped = mode_info(r * 2, c * 2).skip != 0
                        && mode_info(r * 2, c * 2 + 1).skip != 0
                        && mode_info(r * 2 + 1, c * 2).skip != 0
                        && mode_info(r * 2 + 1, c * 2 + 1).skip != 0;
                    if write_skips {
                        *cdef_skips.offset((r * nhfb * 16 + c) as isize) = c_int::from(skipped);
                    }
                    if !skipped {
                        let gy = r >> 2;
                        let gx = c >> 3;
                        let bit = ((r & 3) << 3) | (c & 7);
                        *cdef_masks.offset((gy * nhfb + gx) as isize) |= 1u32 << bit;
                    }
                }
            }
        }

        let constants = alloc_constants::<CdefData>(&mut td.command_buffer);
        let data = constants.data;

        // Generated from gen_filter_tables.c.
        const CDEF_DIRECTIONS: [[c_int; 4]; 8] = [
            [-1, 1, -2, 2],
            [0, 1, -1, 2],
            [0, 1, 0, 2],
            [0, 1, 1, 2],
            [1, 1, 2, 2],
            [1, 0, 2, 1],
            [1, 0, 2, 0],
            [1, 0, 2, -1],
        ];
        for (i, direction) in CDEF_DIRECTIONS.iter().enumerate() {
            for at in [i, 8 + i] {
                (*data).cdef_directions[at][0][1] = direction[0];
                (*data).cdef_directions[at][0][0] = direction[1];
                (*data).cdef_directions[at][1][1] = direction[2];
                (*data).cdef_directions[at][1][0] = direction[3];
            }
        }
        for i in 0..16 {
            (*data).cdef_strength[i][0] = cdef_info.cdef_strengths[i];
            (*data).cdef_uv_strength[i][0] = cdef_info.cdef_uv_strengths[i];
        }

        (*data).index_stride = nhfb;
        (*data).skips_stride = nhfb * 16;
        (*data).hbd = td.is_hbd;
        (*data).bit_depth = td.bitdepth;

        (*data).pri_damping = cdef_info.cdef_pri_damping;
        (*data).sec_damping = cdef_info.cdef_sec_damping;
        let (source, target) = (&*src, &*dst);
        let w = source.width;
        let h = source.height;
        (*data).plane[0] = source.planes[0].stride;
        (*data).plane[1] = source.planes[0].offset;
        (*data).plane[2] = w;
        (*data).plane[3] = h;
        (*data).uv_stride = source.planes[1].stride;
        (*data).uv_offset[0] = source.planes[1].offset;
        (*data).uv_offset[1] = source.planes[2].offset;

        (*data).dst_offset[0] = target.planes[0].offset;
        (*data).dst_offset[1] = target.planes[1].offset;
        (*data).dst_offset[2] = target.planes[2].offset;

        (*data).out_crop_w = source.y_crop_width;
        (*data).out_crop_h = source.y_crop_height;
        (*data).out_flags = match fused {
            Some(f) => {
                c_int::from(f.write_fb_y)
                    | (c_int::from(f.write_fb_uv) << 1)
                    | (c_int::from(f.out_luma) << 2)
                    | (c_int::from(f.out_chroma) << 3)
                    | (c_int::from(f.is_monochrome) << 4)
            }
            None => 0,
        };

        // The lists by the GPU are in the merged front of the frame (`cdef_lists_front`).
        debug_assert!(!gpu_lists || td.cdef_lists_front != 0);

        if stage_dump_active(dec) {
            let indexes = &*td.cdef_indexes;
            let skips = &*td.cdef_skips;
            let regions = [
                av1_stage_dump_region {
                    src: indexes.dev,
                    offset: 0,
                    size: (size_of::<c_int>() * nvfb as usize * nhfb as usize).min(indexes.size),
                    uav: 0,
                },
                av1_stage_dump_region {
                    src: skips.dev,
                    offset: 0,
                    size: (size_of::<c_int>() * nvfb as usize * 16 * nhfb as usize * 16).min(skips.size),
                    uav: 0,
                },
            ];
            let layout = format!(
                "buffer=cdef_info index_size={} index_stride={} index_rows={} skips_stride={} skip_cols={} \
                 skip_rows={}\n",
                regions[0].size as c_int,
                nhfb,
                nvfb,
                nhfb * 16,
                skip_cols,
                skip_rows
            );
            stage_dump_data(dec, c"cdef_info", &layout, 256, &regions);
        }

        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let lib = (*dec).shader_lib;
        // cdef_filter_v3: a group of 64 threads per 64x32 luma samples.
        let (x_shift, y_shift) = (6, 5);
        let shader = pipeline(if fused.is_some() {
            &raw const (*lib).shader_cdef_filter_v3_sp_tex
        } else {
            &raw const (*lib).shader_cdef_filter_v3
        });
        commands.set_pipeline(shader);
        commands.set_layout(shader.layout());
        commands.bind_read(0, buffer(td.cdef_indexes).location());
        commands.bind_read(1, buffer(td.cdef_masks).location());
        commands.bind_write(2, frame_buffers.location());
        commands.bind_constants(3, constants.location);
        if let Some(f) = fused {
            bind_fused_planes(dec, 4, f.pair);
        }
        commands.dispatch(
            ((w + (1 << x_shift) - 1) >> x_shift) as u32,
            ((h + (1 << y_shift) - 1) >> y_shift) as u32,
            1,
        );
        let wrote_fb = fused.is_none_or(|f| f.write_fb_y || f.write_fb_uv) || stage_dump_active(dec);
        if wrote_fb {
            commands.write_barrier(frame_buffers);
        }
        perf_gpu_mark(dec, AV1_PERF_GPU_CDEF);
        stage_dump_fb(dec, c"cdef", dst, src);
    }
}

// ---------------------------------------------------------------------------------------------------------
// Loop restoration.
// ---------------------------------------------------------------------------------------------------------

/// A plane in the constants of `loop_restoration.hlsl`.
#[repr(C)]
struct PlaneInfo1 {
    stride: c_int,
    offset: c_int,
    width: c_int,
    height: c_int,
}

/// The grid of restoration units of a plane.
#[repr(C)]
struct UnitsInfo {
    rows: c_int,
    cols: c_int,
    size: c_int,
    stride: c_int,
}

#[repr(C)]
struct PlaneRestorationData {
    plane: PlaneInfo1,
    units: UnitsInfo,

    pp_offset: c_int,
    dst_offset: c_int,
    lr_buffer_offset: c_int,
    subsampling: c_int,
    hbd: c_int,
    bit_depth: c_int,
    unit_shift: c_int,
    pad: c_int,
}

#[repr(C)]
struct RestorationData {
    sgr_params: [[c_int; 4]; 16],
    sgr_a2_packed: [[u32; 4]; 16],
}

/// `av1_looprestoration` of the C++: loop restoration into the frame's buffer, when the frame has it.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's and `lr_ctxt` the decoder's
/// restoration context.
unsafe fn looprestoration(
    dec: *mut Av1Core,
    cm: *mut AV1_COMMON,
    lr_ctxt: *mut AV1LrStruct,
    fused: Option<FusedTarget>,
) {
    // SAFETY: the caller's contract; the two unit lists are mapped buffers of the slot, sized for the
    // units of the largest frame of the decoder.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        if td.do_loop_rest == 0 {
            return;
        }
        let restored = [0, 1, 2].map(|p| u32::from((*cm).rst_info[p].frame_restoration_type) != RESTORE_NONE as u32);
        if restored.contains(&true) {
            super::libaom::loop_restoration_filter_frame_init(&mut *lr_ctxt, &raw mut (*(*cm).cur_frame).buf, &mut *cm);
        }

        let mut lr_ptr: c_int = 0;
        let mut pl_width = [0; 3];
        let mut pl_height = [0; 3];
        let mut needs_general = [false; 3];
        let back_buffer1 = &raw const (*dec).back_buffer1;
        let dst_buffer = &*td.dst_frame_buffer;
        let src_buffer = &*if td.do_cdef == td.do_superres { td.frame_buffer.cast_const() } else { back_buffer1 };
        let pp_buffer = &*if td.do_superres != 0 { back_buffer1 } else { td.frame_buffer.cast_const() };

        let sgr_constants = alloc_constants::<RestorationData>(&mut td.command_buffer);
        *sgr_constants.data =
            RestorationData { sgr_params: super::libaom::SGR_PARAMS, sgr_a2_packed: super::libaom::SGR_A2_PACKED };
        let plane_constants = alloc_constants::<[PlaneRestorationData; 3]>(&mut td.command_buffer);
        let pl_data = plane_constants.data.cast::<PlaneRestorationData>();

        for p in 0..3 {
            let subsampling = c_int::from(p != 0);
            pl_width[p] = dst_buffer.width >> subsampling;
            pl_height[p] = dst_buffer.height >> subsampling;

            let plane = pl_data.add(p);
            (*plane).plane.width = if p != 0 { dst_buffer.uv_crop_width } else { dst_buffer.y_crop_width };
            (*plane).plane.height = if p != 0 { dst_buffer.uv_crop_height } else { dst_buffer.y_crop_height };
            (*plane).plane.stride = dst_buffer.planes[p].stride;
            (*plane).plane.offset = src_buffer.planes[p].offset;
            (*plane).pp_offset = pp_buffer.planes[p].offset;
            (*plane).dst_offset = dst_buffer.planes[p].offset;
            (*plane).subsampling = subsampling;
            (*plane).hbd = td.is_hbd;
            (*plane).bit_depth = td.bitdepth;
            (*plane).pad = 0;

            if !restored[p] {
                // Entry zero is in range even for a copy-only plane. Keep its dummy grid initialized
                // because the type table is a root SRV and the upload memory is reused across frames.
                (*plane).units.rows = 1;
                (*plane).units.cols = 1;
                (*plane).units.size = 1;
                (*plane).unit_shift = 0;
                (*plane).units.stride = 1;
                (*plane).lr_buffer_offset = 0;
                continue;
            }

            let rsi = &*(*lr_ctxt).ctxt[p].rsi;
            (*plane).units.rows = rsi.vert_units_per_tile;
            (*plane).units.cols = rsi.horz_units_per_tile;
            (*plane).units.size = rsi.restoration_unit_size;
            // The parser provides the unit size as log2, and the host stores it as a power of two.
            (*plane).unit_shift = rsi.restoration_unit_size.trailing_zeros() as c_int;
            (*plane).units.stride = rsi.horz_units_per_tile;
            (*plane).lr_buffer_offset = lr_ptr;

            let lr_type = (*td.loop_rest_types).host_ptr.cast::<c_int>();
            let lr_wiener = (*td.loop_rest_wiener).host_ptr.cast::<c_int>();
            for u in 0..rsi.units_per_tile {
                let unit = &*rsi.unit_info.offset(u as isize);
                let kind = c_int::from(unit.restoration_type);
                let dst_type = lr_type.offset((lr_ptr * 4) as isize);
                let dst_wiener = lr_wiener.offset((lr_ptr * 16) as isize);
                lr_ptr += 1;
                *dst_type = kind;
                *dst_type.add(1) = unit.sgrproj_info.xqd[0];
                *dst_type.add(2) = unit.sgrproj_info.xqd[1];
                *dst_type.add(3) = unit.sgrproj_info.ep;
                if kind == RESTORE_WIENER {
                    for i in 0..8 {
                        *dst_wiener.add(i) = c_int::from(unit.wiener_info.hfilter[i]);
                    }
                    for i in 0..8 {
                        *dst_wiener.add(i + 8) = c_int::from(unit.wiener_info.vfilter[i]);
                    }
                } else {
                    needs_general[p] |= kind != RESTORE_NONE;
                }
            }
        }

        // -dumpstages: the two lists loop restoration reads besides the frames. Per unit, the planes one
        // behind the other: four words (type, the two self guided weights, the self guided set), and behind
        // all of them sixteen words per unit (the horizontal and the vertical Wiener filter), which are
        // written for a Wiener unit only.
        if lr_ptr != 0 && stage_dump_active(dec) {
            let mut layout = format!("buffer=lr_info units={lr_ptr} type_size=16 wiener_size=64\n");
            for p in (0..3).filter(|&p| restored[p]) {
                let plane = &*pl_data.add(p);
                layout.push_str(&format!(
                    "units plane={} first={} cols={} rows={} unit_size={}\n",
                    p, plane.lr_buffer_offset, plane.units.cols, plane.units.rows, plane.units.size
                ));
            }
            let types = &*td.loop_rest_types;
            let wiener = &*td.loop_rest_wiener;
            let regions = [
                av1_stage_dump_region { src: types.dev, offset: 0, size: lr_ptr as usize * 16, uav: 0 },
                av1_stage_dump_region { src: wiener.dev, offset: 0, size: lr_ptr as usize * 64, uav: 0 },
            ];
            if regions[0].size <= types.size && regions[1].size <= wiener.size {
                stage_dump_data(dec, c"lr_info", &layout, 256, &regions);
            }
        }

        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let lib = (*dec).shader_lib;
        if let Some(target) = fused {
            let luma_shader = if needs_general[0] {
                &raw const (*lib).shader_loop_rest_sp_tex
            } else {
                &raw const (*lib).shader_loop_rest_wiener_sp_tex
            };
            let chroma_shader = if needs_general[1] || needs_general[2] {
                &raw const (*lib).shader_loop_rest_sp_tex
            } else {
                &raw const (*lib).shader_loop_rest_wiener_sp_tex
            };
            let mut bound = luma_shader;
            let shader = pipeline(bound);
            commands.set_pipeline(shader);
            commands.set_layout(shader.layout());
            commands.bind_read(0, frame_buffers.location());
            commands.bind_read(1, buffer(td.loop_rest_types).location());
            commands.bind_read(2, buffer(td.loop_rest_wiener).location());
            commands.bind_write(3, frame_buffers.location());
            commands.bind_constants(4, sgr_constants.location);
            commands.bind_constants(5, plane_constants.location);
            bind_fused_planes(dec, 7, target.pair);

            if target.write_fb_y || target.out_luma {
                let flags = c_int::from(target.write_fb_y) | (c_int::from(target.out_luma) << 1);
                commands.set_inline_constants(6, &[c_int::from(restored[0]), 0, flags], 0);
                commands.dispatch(((pl_width[0] + 15) >> 4) as u32, ((pl_height[0] + 3) >> 2) as u32, 1);
            }
            if target.write_fb_uv || target.out_chroma {
                if chroma_shader != bound {
                    bound = chroma_shader;
                    commands.set_pipeline(pipeline(bound));
                }
                let do_rest = c_int::from(restored[1]) | (c_int::from(restored[2]) << 1);
                let flags = c_int::from(target.write_fb_uv)
                    | (c_int::from(target.out_chroma) << 2)
                    | (c_int::from(target.is_monochrome) << 3)
                    | 16;
                commands.set_inline_constants(6, &[do_rest, 1, flags], 0);
                commands.dispatch(((pl_width[1] + 15) >> 4) as u32, ((pl_height[1] + 3) >> 2) as u32, 1);
            }
            if target.write_fb_y || target.write_fb_uv || stage_dump_active(dec) {
                commands.write_barrier(frame_buffers);
            }
        } else {
            // Keep one dispatch per plane and use the same root bindings for both pipelines.
            let shaders = needs_general.map(|needs_general| {
                if needs_general {
                    &raw const (*lib).shader_loop_rest
                } else {
                    &raw const (*lib).shader_loop_rest_wiener
                }
            });
            let mut bound = shaders[0];
            let shader = pipeline(bound);
            commands.set_pipeline(shader);
            commands.set_layout(shader.layout());
            commands.bind_read(0, frame_buffers.location());
            commands.bind_read(1, buffer(td.loop_rest_types).location());
            commands.bind_read(2, buffer(td.loop_rest_wiener).location());
            commands.bind_write(3, frame_buffers.location());
            commands.bind_constants(4, sgr_constants.location);
            commands.bind_constants(5, plane_constants.location);

            for (p, shader) in shaders.into_iter().enumerate() {
                if shader != bound {
                    bound = shader;
                    commands.set_pipeline(pipeline(bound));
                }
                // do_restoration, plane
                commands.set_inline_constants(6, &[c_int::from(restored[p]), p as c_int], 0);
                commands.dispatch(((pl_width[p] + 15) >> 4) as u32, ((pl_height[p] + 3) >> 2) as u32, 1);
            }

            commands.write_barrier(frame_buffers);
        }
        perf_gpu_mark(dec, AV1_PERF_GPU_LOOP_REST);
        stage_dump_fb(dec, c"loop_rest", td.dst_frame_buffer, std::ptr::null());
    }
}

/// Gives a frame buffer the layout of the upscaled frame.
///
/// # Safety
/// `buffer` is a frame buffer of the decoder, and nothing else refers to it during the call.
unsafe fn set_superres_layout(buffer: *mut HwFrameBuffer, layout: &SuperresLayout) {
    // SAFETY: the caller's contract.
    unsafe {
        let buffer = &mut *buffer;
        assert!(buffer.size >= (layout.y_size + 2 * layout.uv_size) as usize, "a frame buffer fits the upscaled frame");
        buffer.y_crop_width = layout.upscaled_width;
        buffer.uv_crop_width = (layout.upscaled_width + 1) >> 1;
        buffer.y_crop_height = layout.y_crop_height;
        buffer.uv_crop_height = layout.uv_crop_height;
        buffer.planes[0].stride = layout.y_stride;
        buffer.planes[1].stride = layout.uv_stride;
        buffer.planes[2].stride = layout.uv_stride;
        buffer.planes[0].offset = (buffer.base_offset + (layout.h_border * layout.bpp) as usize) as c_int;
        buffer.planes[1].offset = buffer.planes[0].offset + layout.y_size;
        buffer.planes[2].offset = buffer.planes[1].offset + layout.uv_size;
        buffer.hbd = layout.hbd;
    }
}

/// The planes of a frame of the upscaled width in a frame buffer.
struct SuperresLayout {
    upscaled_width: c_int,
    /// Height of the visible frame, luma and chroma, and whether a sample has two bytes: the frame's.
    y_crop_height: c_int,
    uv_crop_height: c_int,
    hbd: c_int,
    h_border: c_int,
    bpp: c_int,
    y_stride: c_int,
    uv_stride: c_int,
    y_size: c_int,
    uv_size: c_int,
}

/// `av1_cdef_looprestoration` of the C++: records CDEF, superres and loop restoration for the frame of
/// the current command list, each when the frame has it.
///
/// With superres the frame is upscaled between CDEF and loop restoration. Loop restoration reads the
/// frame before CDEF too, so that one is upscaled as well, into the back buffer.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's and `lr_ctxt` the decoder's
/// restoration context (`AV1LrStruct`).
pub(crate) unsafe fn cdef_looprestoration(
    dec: *mut Av1Core,
    cm: *mut AV1_COMMON,
    lr_ctxt: *mut c_void,
    fused: Option<FusedTarget>,
) {
    // SAFETY: the caller's contract; the frame buffers are those of the slot and the decoder's back
    // buffer, which only this thread touches while it records.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let back_buffer1 = &raw mut (*dec).back_buffer1;
        let cdef_dst = if td.do_loop_rest == td.do_superres { td.dst_frame_buffer } else { back_buffer1 };
        let dst_fb = td.dst_frame_buffer;

        let cdef_fused = if td.do_superres == 0 && td.do_loop_rest == 0 { fused } else { None };
        let superres_fused = if td.do_superres != 0 && td.do_loop_rest == 0 { fused } else { None };
        let lr_fused = if td.do_loop_rest != 0 { fused } else { None };

        cdef_filter_run(dec, &*cm, td.frame_buffer, cdef_dst, cdef_fused);

        if td.do_superres != 0 {
            let h_border = 16;
            let bpp = 1 + (*dst_fb).hbd;
            let upscaled_width = (*cm).superres_upscaled_width;
            let y_height = ((*dst_fb).height + 127) & !127;
            let superres_w = (upscaled_width + 127) & !127;
            let y_stride = (superres_w + 2 * h_border) * bpp;
            let uv_stride = ((superres_w >> 1) + 2 * h_border) * bpp;
            let layout = SuperresLayout {
                upscaled_width,
                y_crop_height: (*dst_fb).y_crop_height,
                uv_crop_height: (*dst_fb).uv_crop_height,
                hbd: (*dst_fb).hbd,
                h_border,
                bpp,
                y_stride,
                uv_stride,
                y_size: y_height * y_stride,
                uv_size: (y_height >> 1) * uv_stride,
            };
            if td.do_loop_rest != 0 {
                // The frame before CDEF (the slot's frame buffer), upscaled into the back buffer.
                set_superres_layout(back_buffer1, &layout);
                superres(dec, td.frame_buffer, back_buffer1, 2, None);
            }

            if td.do_cdef != 0 || td.do_loop_rest == 0 {
                // The frame behind CDEF, or the frame as it is without CDEF, upscaled into the slot's frame
                // buffer when loop restoration follows, else into the frame's own buffer.
                let src = if td.do_cdef != 0 { cdef_dst } else { td.frame_buffer };
                let dst = if td.do_loop_rest != 0 { td.frame_buffer } else { dst_fb };
                set_superres_layout(dst, &layout);
                superres(dec, src, dst, 3, superres_fused);
            }

            // The frame's buffer has the upscaled layout from here on, if it did not get it above.
            let dst_fb = &mut *dst_fb;
            dst_fb.y_crop_width = upscaled_width;
            dst_fb.uv_crop_width = (upscaled_width + 1) >> 1;
            dst_fb.width = (upscaled_width + 7) & !7;
            dst_fb.planes[0].stride = y_stride;
            dst_fb.planes[1].stride = uv_stride;
            dst_fb.planes[2].stride = uv_stride;
            dst_fb.planes[0].offset = (dst_fb.base_offset + (h_border * bpp) as usize) as c_int;
            dst_fb.planes[1].offset = dst_fb.planes[0].offset + layout.y_size;
            dst_fb.planes[2].offset = dst_fb.planes[1].offset + layout.uv_size;
            let buf = &mut (*(*cm).cur_frame).buf;
            buf.crop_widths[0] = dst_fb.y_crop_width;
            buf.crop_widths[1] = dst_fb.uv_crop_width;
            buf.widths[0] = dst_fb.width;
            buf.widths[1] = dst_fb.width >> 1;
        }

        looprestoration(dec, cm, lr_ctxt.cast(), lr_fused);
    }
}

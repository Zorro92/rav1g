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
use std::sync::atomic::{AtomicI32, Ordering};

use super::core::{
    alloc_constants, buffer, commands, perf_gpu_mark, pipeline, stage_dump_active, stage_dump_data, stage_dump_fb,
};
use super::types::{Av1Core, av1_stage_dump_region};
use crate::sys::parser::{
    AV1_COMMON, AV1_PERF_GPU_CDEF, AV1_PERF_GPU_LOOP_REST, AV1_PERF_GPU_SUPERRES, AV1LrStruct, HwFrameBuffer,
    MI_SIZE_64X64, RESTORE_NONE, RESTORE_WIENER, YV12_BUFFER_CONFIG, sgr_params,
};

unsafe extern "C" {
    /// `av1/common/restoration.c`: the restoration units of the planes `plane_start..plane_end`.
    fn av1_loop_restoration_filter_frame_init(
        lr_ctxt: *mut AV1LrStruct,
        frame: *mut YV12_BUFFER_CONFIG,
        cm: *mut AV1_COMMON,
        optimized_lr: c_int,
        num_planes: c_int,
    );
}

fn plane_words(buffer: &HwFrameBuffer) -> [[c_int; 4]; 3] {
    buffer.planes.map(|plane| [plane.stride, plane.offset, plane.res_stride, plane.res_offset])
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
unsafe fn superres(dec: *mut Av1Core, src: *const HwFrameBuffer, dst: *const HwFrameBuffer, iter: c_int) {
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
        perf_gpu_mark(dec, AV1_PERF_GPU_SUPERRES);
        stage_dump_fb(dec, if iter == 2 { c"superres_lr" } else { c"superres" }, dst, std::ptr::null());
    }
}

// ---------------------------------------------------------------------------------------------------------
// CDEF.
// ---------------------------------------------------------------------------------------------------------

/// The constants of `cdef_filter.hlsl` and `cdef_filter_v2.hlsl`.
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
    _dummie: [c_int; 3],

    cdef_directions: [[[c_int; 4]; 2]; 16],
    cdef_strength: [[c_int; 4]; 16],
    cdef_uv_strength: [[c_int; 4]; 16],
}

// Which CDEF shader runs. cdef_filter_v2.hlsl writes the same bytes as cdef_filter.hlsl with a different
// work split (one group per 64x64 luma block) and is 7 to 9 times faster on 4K 10-bit streams; it is the
// default. av1_set_cdef_v2(0) selects the original shader (test player flag -cdefv1).
static CDEF_V2: AtomicI32 = AtomicI32::new(1);

/// `av1_set_cdef_v2` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_cdef_v2(enable: c_int) {
    CDEF_V2.store(enable, Ordering::Relaxed);
}

/// `av1_cdef_filter_run` of the C++: CDEF from `src` into `dst`, when the frame has it.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's; `src` and `dst` are frame buffers.
unsafe fn cdef_filter_run(dec: *mut Av1Core, cm: &AV1_COMMON, src: *const HwFrameBuffer, dst: *const HwFrameBuffer) {
    // SAFETY: the caller's contract; the two lists are mapped buffers of the slot, sized for the largest
    // frame of the decoder, and the mode info grid covers the frame.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        if td.do_cdef == 0 {
            return;
        }
        let cdef_info = &cm.cdef_info;
        let mi_size_64 = MI_SIZE_64X64 as c_int;

        let cdef_indexes = (*td.cdef_indexes).host_ptr.cast::<c_int>();
        let cdef_skips = (*td.cdef_skips).host_ptr.cast::<c_int>();
        let nvfb = (cm.mi_rows + mi_size_64 - 1) / mi_size_64;
        let nhfb = (cm.mi_cols + mi_size_64 - 1) / mi_size_64;
        let mode_info = |row: c_int, col: c_int| &**cm.mi_grid_visible.offset((row * cm.mi_stride + col) as isize);

        // The strength index of every 64x64 block, and for every 8x8 block whether all of it is skipped.
        for fbr in 0..nvfb {
            for fbc in 0..nhfb {
                *cdef_indexes.offset((fbr * nhfb + fbc) as isize) =
                    c_int::from(mode_info(mi_size_64 * fbr, mi_size_64 * fbc).cdef_strength);
            }
        }
        let skip_rows = (nvfb * 16).min(cm.mi_rows / 2);
        let skip_cols = (nhfb * 16).min(cm.mi_cols / 2);
        for r in 0..skip_rows {
            for c in 0..skip_cols {
                let skipped = mode_info(r * 2, c * 2).skip != 0
                    && mode_info(r * 2, c * 2 + 1).skip != 0
                    && mode_info(r * 2 + 1, c * 2).skip != 0
                    && mode_info(r * 2 + 1, c * 2 + 1).skip != 0;
                *cdef_skips.offset((r * nhfb * 16 + c) as isize) = c_int::from(skipped);
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

        // -dumpstages: the two lists CDEF reads besides the frame. The strength index of every 64x64 block,
        // rows of nhfb; behind them the skip flags of the 8x8 blocks, rows of skips_stride, of which the loops
        // above write skip_cols by skip_rows for this frame.
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
        let cdef_v2 = CDEF_V2.load(Ordering::Relaxed) != 0;
        let lib = (*dec).shader_lib;
        let shader = pipeline(if cdef_v2 {
            &raw const (*lib).shader_cdef_filter_v2
        } else {
            &raw const (*lib).shader_cdef_filter
        });
        commands.set_pipeline(shader);
        commands.set_layout(shader.layout());
        commands.bind_read(0, buffer(td.cdef_indexes).location());
        commands.bind_read(1, buffer(td.cdef_skips).location());
        commands.bind_write(2, frame_buffers.location());
        commands.bind_constants(3, constants.location);
        if cdef_v2 {
            // The same bindings, 64x64 luma samples per group.
            commands.dispatch(((w + 63) >> 6) as u32, ((h + 63) >> 6) as u32, 1);
        } else {
            commands.dispatch(((w + 31) >> 5) as u32, ((h + 31) >> 5) as u32, 1);
        }
        commands.write_barrier(frame_buffers);
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
    pad: [c_int; 2],
}

#[repr(C)]
struct RestorationData {
    sgr_params: [[c_int; 4]; 16],
}

/// `av1_looprestoration` of the C++: loop restoration into the frame's buffer, when the frame has it.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's and `lr_ctxt` the decoder's
/// restoration context.
unsafe fn looprestoration(dec: *mut Av1Core, cm: *mut AV1_COMMON, lr_ctxt: *mut AV1LrStruct) {
    // SAFETY: the caller's contract; the two unit lists are mapped buffers of the slot, sized for the
    // units of the largest frame of the decoder.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        if td.do_loop_rest == 0 {
            return;
        }
        let restored = [0, 1, 2].map(|p| u32::from((*cm).rst_info[p].frame_restoration_type) != RESTORE_NONE as u32);
        if restored.contains(&true) {
            av1_loop_restoration_filter_frame_init(lr_ctxt, &raw mut (*(*cm).cur_frame).buf, cm, 0, 3);
        }

        let mut lr_ptr: c_int = 0;
        let mut pl_width = [0; 3];
        let mut pl_height = [0; 3];
        let back_buffer1 = &raw const (*dec).back_buffer1;
        let dst_buffer = &*td.dst_frame_buffer;
        let src_buffer = &*if td.do_cdef == td.do_superres { td.frame_buffer.cast_const() } else { back_buffer1 };
        let pp_buffer = &*if td.do_superres != 0 { back_buffer1 } else { td.frame_buffer.cast_const() };

        let sgr_constants = alloc_constants::<RestorationData>(&mut td.command_buffer);
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

            if !restored[p] {
                // A plane without restoration has no units, but its record still gets a valid unit grid (one
                // unit, the first entry of the type table). The memory of the record is the frame's constant
                // upload memory, which is only rewound between frames, so fields left unwritten here would
                // keep what an earlier frame put at this place. A shader that computed the unit's address
                // from them and loaded the type table without a branch (as FXC compiled
                // loop_restoration.hlsl, the load flattened in front of a select) read at an address made of
                // stale data: the table is bound without bounds checking, and the read can be outside of
                // mapped memory, which removes the device. loop_restoration.hlsl now loads under a branch for
                // a restored plane only; these defaults keep any other reader in range.
                (*plane).units.rows = 1;
                (*plane).units.cols = 1;
                (*plane).units.size = 1;
                (*plane).units.stride = 1;
                (*plane).lr_buffer_offset = 0;
                continue;
            }

            let rsi = &*(*lr_ctxt).ctxt[p].rsi;
            (*plane).units.rows = rsi.vert_units_per_tile;
            (*plane).units.cols = rsi.horz_units_per_tile;
            (*plane).units.size = rsi.restoration_unit_size;
            (*plane).units.stride = rsi.horz_units_per_tile;
            (*plane).lr_buffer_offset = lr_ptr;

            let lr_type = (*td.loop_rest_types).host_ptr.cast::<c_int>();
            let lr_wiener = (*td.loop_rest_wiener).host_ptr.cast::<c_int>();
            for u in 0..rsi.units_per_tile {
                let unit = &*rsi.unit_info.offset(u as isize);
                let dst_type = lr_type.offset((lr_ptr * 4) as isize);
                let dst_wiener = lr_wiener.offset((lr_ptr * 16) as isize);
                lr_ptr += 1;
                *dst_type = c_int::from(unit.restoration_type);
                *dst_type.add(1) = unit.sgrproj_info.xqd[0];
                *dst_type.add(2) = unit.sgrproj_info.xqd[1];
                *dst_type.add(3) = unit.sgrproj_info.ep;
                if u32::from(unit.restoration_type) == RESTORE_WIENER as u32 {
                    for i in 0..8 {
                        *dst_wiener.add(i) = c_int::from(unit.wiener_info.hfilter[i]);
                    }
                    for i in 0..8 {
                        *dst_wiener.add(i + 8) = c_int::from(unit.wiener_info.vfilter[i]);
                    }
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

        let parameters = (&raw const sgr_params).read();
        for (set, parameters) in parameters.iter().enumerate() {
            (*sgr_constants.data).sgr_params[set] =
                [parameters.r[0], parameters.r[1], parameters.s[0], parameters.s[1]];
        }

        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let shader = pipeline(&raw const (*(*dec).shader_lib).shader_loop_rest);
        commands.set_pipeline(shader);
        commands.set_layout(shader.layout());
        commands.bind_read(0, frame_buffers.location());
        commands.bind_read(1, buffer(td.loop_rest_types).location());
        commands.bind_read(2, buffer(td.loop_rest_wiener).location());
        commands.bind_write(3, frame_buffers.location());
        commands.bind_constants(4, sgr_constants.location);
        commands.bind_constants(5, plane_constants.location);

        for p in 0..3 {
            // do_restoration, plane
            commands.set_inline_constants(6, &[c_int::from(restored[p]), p as c_int], 0);
            commands.dispatch(((pl_width[p] + 15) >> 4) as u32, ((pl_height[p] + 3) >> 2) as u32, 1);
        }

        commands.write_barrier(frame_buffers);
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_cdef_looprestoration(dec: *mut Av1Core, cm: *mut AV1_COMMON, lr_ctxt: *mut c_void) {
    // SAFETY: the caller's contract; the frame buffers are those of the slot and the decoder's back
    // buffer, which only this thread touches while it records.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let back_buffer1 = &raw mut (*dec).back_buffer1;
        let cdef_dst = if td.do_loop_rest == td.do_superres { td.dst_frame_buffer } else { back_buffer1 };
        let dst_fb = td.dst_frame_buffer;

        cdef_filter_run(dec, &*cm, td.frame_buffer, cdef_dst);

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
                superres(dec, td.frame_buffer, back_buffer1, 2);
            }

            if td.do_cdef != 0 || td.do_loop_rest == 0 {
                // The frame behind CDEF, or the frame as it is without CDEF, upscaled into the slot's frame
                // buffer when loop restoration follows, else into the frame's own buffer.
                let src = if td.do_cdef != 0 { cdef_dst } else { td.frame_buffer };
                let dst = if td.do_loop_rest != 0 { td.frame_buffer } else { dst_fb };
                set_superres_layout(dst, &layout);
                superres(dec, src, dst, 3);
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
            buf.__bindgen_anon_3.crop_widths[0] = dst_fb.y_crop_width;
            buf.__bindgen_anon_3.crop_widths[1] = dst_fb.uv_crop_width;
            buf.__bindgen_anon_1.widths[0] = dst_fb.width;
            buf.__bindgen_anon_1.widths[1] = dst_fb.width >> 1;
        }

        looprestoration(dec, cm, lr_ctxt.cast());
    }
}

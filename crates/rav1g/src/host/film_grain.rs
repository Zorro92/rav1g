// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Film grain (AV1 7.18.3): the grain templates of the frame (7.18.3.3) are made on the CPU while the
//! frame's command list is recorded, and added to the frame on its way into the output image (7.18.3.5),
//! on the GPU. From `av1_filmgrain_run` of `libav1/dx/loopfilters.cpp`.
//!
//! **The templates on the CPU** (`grain_template.rs`, rav1d's `generate_grain_y` and
//! `generate_grain_uv`). They depend on the grain parameters of the header and on nothing decoded. On
//! the GPU (`film_grain_gen_luma.hlsl` and `film_grain_gen_chroma.hlsl`, the path of
//! `av1_set_fg_gen_legacy`) they were two dependent dispatches of one thread group each, a group sync
//! for every step of a wavefront through the template: the GPU's worst case, in front of the apply pass
//! of every grain frame, where each dependent step costs about 18 us fixed (AGENTS.md, "Dependent
//! dispatches"). On the CPU they are about 0.15 million multiply-adds a frame, on the thread that records
//! the frame: the submission thread with more than one frame in parse, so not a parse thread (the CPU
//! limit on high-bitrate streams), and the decode thread with one. The templates go to the GPU in the
//! frame's grain buffer, the upload buffer that has its random offsets (37,320 bytes in front of them: 73
//! rows of 82 ints for luma, 2 x 38 rows of 44 for chroma), written once through the mapping from a copy
//! on the stack, and the apply shaders read them there: no copy pass, no extra dispatch, no barrier. The
//! layout is that of the GPU path's `filmgrain_noise`, so the apply shaders are unchanged.

use std::ffi::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

use super::core::{alloc_constants, buffer, commands, perf_add, perf_gpu_mark, perf_ticks, pipeline};
use super::grain_template::{CHROMA_OFFSET_INTS, GRAIN_HEIGHT, GRAIN_WIDTH, TEMPLATE_BYTES, Templates};
use super::types::Av1Core;
use crate::gpu::d3d12::{com_ref, wrapped_ref};
use crate::gpu::{Device, PlaneViews};
use crate::sys::{
    AOM_CICP_MC_IDENTITY, AV1_COMMON, AV1_PERF_CPU_GRAIN_TEMPLATE, AV1_PERF_GPU_FILM_GRAIN,
    AV1_PERF_GPU_FILM_GRAIN_GEN, HwFrameBuffer,
};
use windows::Win32::Graphics::Direct3D12::ID3D12Device;

pub(super) static SKIP_FILM_GRAIN: AtomicBool = AtomicBool::new(false);
static FG_LEGACY: AtomicBool = AtomicBool::new(false);
static FG_GEN_LEGACY: AtomicBool = AtomicBool::new(false);

/// The bytes of a frame's grain buffer (`filmgrain_rand_offset` of a slot or output context) that come
/// before its random offsets: the templates of the CPU path.
pub(super) const GRAIN_BUFFER_TEMPLATE_BYTES: usize = TEMPLATE_BYTES;

/// `av1_set_fg_legacy` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_fg_legacy(enable: c_int) {
    FG_LEGACY.store(enable != 0, Ordering::Relaxed);
}

/// `av1_set_fg_gen_legacy` (`dx/av1_perf.h`): the grain templates made on the GPU, by the two shaders
/// of the C++ decoder, instead of on the CPU.
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_fg_gen_legacy(enable: c_int) {
    FG_GEN_LEGACY.store(enable != 0, Ordering::Relaxed);
}

/// `av1_set_skip_film_grain` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_skip_film_grain(enable: c_int) {
    SKIP_FILM_GRAIN.store(enable != 0, Ordering::Relaxed);
}

/// The film grain parameters of a frame as the shaders read them.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GrainParams {
    apply_grain: c_int,
    update_parameters: c_int,
    num_y_points: c_int,  // value: 0..14
    num_cb_points: c_int, // value: 0..10

    num_cr_points: c_int,  // value: 0..10
    scaling_shift: c_int,  // values : 8..11
    ar_coeff_lag: c_int,   // values:  0..3
    ar_coeff_shift: c_int, // values : 6..9

    cb_mult: c_int,      // 8 bits
    cb_luma_mult: c_int, // 8 bits
    cb_offset: c_int,    // 9 bits
    cr_mult: c_int,      // 8 bits

    cr_luma_mult: c_int, // 8 bits
    cr_offset: c_int,    // 9 bits
    overlap_flag: c_int,
    clip_to_restricted_range: c_int,

    bit_depth: u32, // video bit depth
    chroma_scaling_from_luma: c_int,
    grain_scale_shift: c_int,
    random_seed: u32,

    // x: y, cb, cr
    scaling_points: [[[c_int; 4]; 2]; 14],
    ar_coeffs: [[[c_int; 4]; 25]; 3],
}

/// The constants of `film_grain_gen_luma.hlsl` and `film_grain_gen_chroma.hlsl`.
#[repr(C)]
struct FilmGrainGenData {
    params: GrainParams,

    luma_block_size_y: c_int,
    luma_block_size_x: c_int,
    luma_grain_stride: c_int,
    chroma_block_size_y: c_int,

    chroma_block_size_x: c_int,
    chroma_grain_stride: c_int,
    left_pad: c_int,
    top_pad: c_int,

    right_pad: c_int,
    bottom_pad: c_int,
    dst_offset_u: c_int,
    dst_offset_v: c_int,

    pred_pos: [[[c_int; 4]; 3]; 25],
}

/// The constants of `film_grain_filter.hlsl`.
#[repr(C)]
struct FilmGrainData {
    params: GrainParams,

    src_planes: [[c_int; 4]; 3],
    dst_planes: [[c_int; 4]; 3],

    enable_chroma: c_int,
    random_offset_stride: c_int,
    width: c_int,
    height: c_int,

    mc_identity: c_int,
    luma_grain_stride: c_int,
    chroma_grain_stride: c_int,
    left_pad: c_int,

    right_pad: c_int,
    top_pad: c_int,
    bottom_pad: c_int,
    ar_padding: c_int,

    grain_offset_u: c_int,
    grain_offset_v: c_int,
    is_10x3: c_int,
    pad: c_int,

    scaling_lut: [[c_int; 4]; 256],
}

/// The piecewise linear scaling function through `points` (value, scaling), at the values 0..255.
fn scaling_function(points: &[[c_int; 2]], mut set: impl FnMut(usize, c_int)) {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        for i in 0..256 {
            set(i, 0);
        }
        return;
    };
    for i in 0..first[0] {
        set(i as usize, first[1]);
    }
    for pair in points.windows(2) {
        let delta_y = pair[1][1] - pair[0][1];
        let delta_x = pair[1][0] - pair[0][0];
        let delta = i64::from(delta_y * ((65536 + (delta_x >> 1)) / delta_x));
        for x in 0..delta_x {
            set((pair[0][0] + x) as usize, pair[0][1] + ((i64::from(x) * delta + 32768) >> 16) as c_int);
        }
    }
    for i in last[0]..256 {
        set(i as usize, last[1]);
    }
}

/// The original shader interpolates between 8-bit knots with signed rounding. Pack all four
/// 10-bit results into one word so the direct-texture shader needs just one lookup.
fn pack_10bit_scaling(scale: c_int, next: c_int) -> c_int {
    let delta = next - scale;
    let quarter = scale + ((delta + 2) >> 2);
    let half = scale + ((2 * delta + 2) >> 2);
    let three_quarters = scale + ((3 * delta + 2) >> 2);
    scale | (quarter << 8) | (half << 16) | (three_quarters << 24)
}

/// Write component `plane` of the apply shader's scaling table.
/// # Safety
/// `lut` is valid for writing.
unsafe fn write_scaling_lut(points: &[[c_int; 2]], lut: *mut [[c_int; 4]; 256], plane: usize, packed: bool) {
    // SAFETY: the caller's contract. Only component `plane` is written.
    let set = |index: usize, value: c_int| unsafe { (*lut)[index][plane] = value };
    if packed {
        // Emit a word when the next knot is available, without reading the mapped constants.
        let mut previous = 0;
        scaling_function(points, |index, value| {
            if index != 0 {
                set(index - 1, pack_10bit_scaling(previous, value));
            }
            previous = value;
        });
        // All four indices at the last knot have the same scale.
        set(255, pack_10bit_scaling(previous, previous));
    } else {
        scaling_function(points, set);
    }
}

/// The random register of the grain blocks of a row of 32 luma lines.
struct Rng {
    random_register: u16,
}

impl Rng {
    fn new(luma_line: c_int, seed: u16) -> Rng {
        let luma_num = luma_line >> 5;
        let mut random_register = seed;
        random_register ^= (((luma_num * 37 + 178) & 255) << 8) as u16;
        random_register ^= ((luma_num * 173 + 105) & 255) as u16;
        Rng { random_register }
    }

    fn get_random_number(&mut self, bits: u32) -> c_int {
        let register = self.random_register;
        let bit = (register ^ (register >> 1) ^ (register >> 3) ^ (register >> 12)) & 1;
        self.random_register = (register >> 1) | (bit << 15);
        c_int::from((self.random_register >> (16 - bits)) & ((1 << bits) - 1))
    }
}

/// Where the frame with its grain goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrainTarget {
    /// The three planes of the output image, in the back buffer.
    Planes,
    /// The two planes of the semi-planar output (`output.rs`), in the back buffer.
    SemiPlanar,
    /// The two planes of the application's texture: the pair of plane views.
    SemiPlanarTexture(u32),
}

/// The GPU path of the templates (`av1_set_fg_gen_legacy`): `film_grain_gen_luma.hlsl`, then
/// `film_grain_gen_chroma.hlsl` (whose filter reads the luma template), into `filmgrain_noise`, one thread
/// group each (chroma: one a plane). Their time is the stage `film_grain_gen` of the timing run.
///
/// # Safety
/// As for [`filmgrain_run`].
unsafe fn record_gpu_templates(
    dec: *mut Av1Core,
    grain: &GrainParams,
    luma_block_size_x: c_int,
    luma_block_size_y: c_int,
    chroma_block_size_x: c_int,
    chroma_block_size_y: c_int,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let gen_constants = alloc_constants::<FilmGrainGenData>(&mut td.command_buffer);
        let gen_data = gen_constants.data;
        (*gen_data).params = *grain;
        let (left_pad, right_pad, top_pad, bottom_pad) = (3, 3, 3, 0);
        (*gen_data).luma_block_size_y = luma_block_size_y;
        (*gen_data).luma_block_size_x = luma_block_size_x;
        (*gen_data).luma_grain_stride = luma_block_size_x;
        (*gen_data).chroma_block_size_y = chroma_block_size_y;
        (*gen_data).chroma_block_size_x = chroma_block_size_x;
        (*gen_data).chroma_grain_stride = chroma_block_size_x;
        (*gen_data).left_pad = left_pad;
        (*gen_data).top_pad = top_pad;
        (*gen_data).right_pad = right_pad;
        (*gen_data).bottom_pad = bottom_pad;
        (*gen_data).dst_offset_u = CHROMA_OFFSET_INTS[0] as c_int;
        (*gen_data).dst_offset_v = CHROMA_OFFSET_INTS[1] as c_int;
        // The positions the auto-regression of a sample reads: the rows above, then the samples to its left;
        // for chroma also the luma sample at its place, when luma has grain.
        let lag = grain.ar_coeff_lag;
        let mut pos_ar_index = 0usize;
        let mut position = |row: c_int, col: c_int| {
            for component in 0..2 {
                (*gen_data).pred_pos[pos_ar_index][0][component] = row;
                (*gen_data).pred_pos[pos_ar_index][1][component] = col;
                (*gen_data).pred_pos[pos_ar_index][2][component] = 0;
            }
            pos_ar_index += 1;
        };
        for row in -lag..0 {
            for col in -lag..lag + 1 {
                position(row, col);
            }
        }
        for col in -lag..0 {
            position(0, col);
        }
        if grain.num_y_points > 0 {
            (*gen_data).pred_pos[pos_ar_index][0][1] = 0;
            (*gen_data).pred_pos[pos_ar_index][1][1] = 0;
            (*gen_data).pred_pos[pos_ar_index][2][1] = 1;
        }

        let commands = commands(dec);
        let lib = (*dec).shader_lib;
        let noise = buffer((*dec).filmgrain_noise);
        for (shader, groups_y) in [
            (pipeline(&raw const (*lib).shader_filmgrain_luma_gen), 1),
            (pipeline(&raw const (*lib).shader_filmgrain_chroma_gen), 2),
        ] {
            commands.set_pipeline(shader);
            commands.set_layout(shader.layout());
            commands.bind_read(0, buffer((*dec).filmgrain_random_luma).location());
            commands.bind_read(1, buffer((*dec).filmgrain_random_chroma).location());
            commands.bind_read(2, buffer((*dec).filmgrain_gaus).location());
            commands.bind_write(3, noise.location());
            commands.bind_constants(4, gen_constants.location);
            commands.dispatch(1, groups_y, 1);
            commands.write_barrier(noise);
        }
        perf_gpu_mark(dec, AV1_PERF_GPU_FILM_GRAIN_GEN);
    }
}

/// `av1_filmgrain_run` of the C++: records the grain synthesis for the output of the frame of the
/// current command list. The frame with grain goes into the layout the decoder's back buffer has, which
/// is that of the output image. Without `enable_chroma` only luma gets grain.
///
/// # Safety
/// `dec` is the decoder while it records a frame that is output; `cm` is the frame's.
pub(crate) unsafe fn filmgrain_run(dec: *mut Av1Core, cm: &AV1_COMMON, enable_chroma: c_int, target: GrainTarget) {
    // SAFETY: the caller's contract; the random offsets are a mapped buffer of the slot, sized for the
    // largest frame of the decoder.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let params = &cm.film_grain_params;

        let mut grain = GrainParams {
            apply_grain: params.apply_grain,
            update_parameters: params.update_parameters,
            num_y_points: params.num_y_points,
            num_cb_points: params.num_cb_points,
            num_cr_points: params.num_cr_points,
            scaling_shift: params.scaling_shift,
            ar_coeff_lag: params.ar_coeff_lag,
            ar_coeff_shift: params.ar_coeff_shift,
            cb_mult: params.cb_mult,
            cb_luma_mult: params.cb_luma_mult,
            cb_offset: params.cb_offset,
            cr_mult: params.cr_mult,
            cr_luma_mult: params.cr_luma_mult,
            cr_offset: params.cr_offset,
            overlap_flag: params.overlap_flag,
            clip_to_restricted_range: params.clip_to_restricted_range,
            bit_depth: params.bit_depth,
            chroma_scaling_from_luma: params.chroma_scaling_from_luma,
            grain_scale_shift: params.grain_scale_shift,
            random_seed: u32::from(params.random_seed),
            ..GrainParams::default()
        };
        for i in 0..24 {
            grain.ar_coeffs[0][i][0] = params.ar_coeffs_y[i];
            grain.ar_coeffs[1][i][0] = params.ar_coeffs_cb[i];
            grain.ar_coeffs[2][i][0] = params.ar_coeffs_cr[i];
        }
        grain.ar_coeffs[1][24][0] = params.ar_coeffs_cb[24];
        grain.ar_coeffs[2][24][0] = params.ar_coeffs_cr[24];
        for i in 0..14 {
            grain.scaling_points[i][0][0] = params.scaling_points_y[i][0];
            grain.scaling_points[i][1][0] = params.scaling_points_y[i][1];
            if i < 10 {
                grain.scaling_points[i][0][1] = params.scaling_points_cb[i][0];
                grain.scaling_points[i][1][1] = params.scaling_points_cb[i][1];
                grain.scaling_points[i][0][2] = params.scaling_points_cr[i][0];
                grain.scaling_points[i][1][2] = params.scaling_points_cr[i][1];
            }
        }

        // The templates (7.18.3.3) as the apply shaders read them: luma 73 rows of 82 samples, then the two
        // chroma templates, 38 rows of 44. The pads are the columns and rows the filter of the template
        // does not write (3 left, right and top), around the 64x64 (luma) and 32x32 (chroma) samples a
        // 32x32 block of the frame takes its grain from, at an offset of up to 2 x 3 + 2 x 3 more.
        let left_pad = 3;
        let right_pad = 3; // padding to offset for AR coefficients
        let top_pad = 3;
        let bottom_pad = 0;
        let ar_padding = 3; // maximum lag used for stabilization of AR coefficients
        let luma_subblock_size_y = 32;
        let luma_subblock_size_x = 32;
        let luma_block_size_y = GRAIN_HEIGHT as c_int;
        let luma_block_size_x = GRAIN_WIDTH as c_int;
        let chroma_block_size_y = top_pad + ar_padding + (luma_subblock_size_y >> 1) * 2 + bottom_pad;
        let chroma_block_size_x = left_pad + ar_padding + (luma_subblock_size_x >> 1) * 2 + ar_padding + right_pad;
        debug_assert_eq!(luma_block_size_y, top_pad + 2 * ar_padding + luma_subblock_size_y * 2 + bottom_pad);
        debug_assert_eq!(
            luma_block_size_x,
            left_pad + 2 * ar_padding + luma_subblock_size_x * 2 + 2 * ar_padding + right_pad
        );
        let [grain_offset_u, grain_offset_v] = CHROMA_OFFSET_INTS.map(|ints| ints as c_int);
        debug_assert_eq!(grain_offset_v - grain_offset_u, chroma_block_size_x * chroma_block_size_y);

        // The frame's grain buffer: the templates of the CPU path, then the random offsets.
        let grain_buffer = (*td.filmgrain_rand_offset).host_ptr.cast::<u8>();
        let commands = commands(dec);
        let lib = (*dec).shader_lib;
        let template_location = if FG_GEN_LEGACY.load(Ordering::Relaxed) {
            record_gpu_templates(
                dec,
                &grain,
                luma_block_size_x,
                luma_block_size_y,
                chroma_block_size_x,
                chroma_block_size_y,
            );
            buffer((*dec).filmgrain_noise).location()
        } else {
            let start = perf_ticks();
            let mut templates = Templates::zeroed();
            templates.generate(params);
            // One pass of writes through the mapping, which is write-combined: the filter reads what it
            // has written, which it does in the copy on the stack.
            grain_buffer.copy_from_nonoverlapping((&raw const templates).cast::<u8>(), TEMPLATE_BYTES);
            perf_add(dec, AV1_PERF_CPU_GRAIN_TEMPLATE, start);
            buffer(td.filmgrain_rand_offset).location()
        };

        let constants = alloc_constants::<FilmGrainData>(&mut td.command_buffer);
        let data = constants.data;
        (*data).params = grain;
        (*data).enable_chroma = enable_chroma;

        let plane_words = |buffer: &HwFrameBuffer| {
            buffer.planes.map(|plane| [plane.stride, plane.offset, plane.res_stride, plane.res_offset])
        };
        (*data).dst_planes = plane_words(&(*dec).back_buffer1);

        let src = &*td.dst_frame_buffer;
        (*data).src_planes = plane_words(src);

        (*data).luma_grain_stride = luma_block_size_x;
        (*data).chroma_grain_stride = chroma_block_size_x;
        (*data).grain_offset_u = grain_offset_u;
        (*data).grain_offset_v = grain_offset_v;
        (*data).mc_identity =
            c_int::from((*cm.cur_frame).buf.matrix_coefficients as u32 == AOM_CICP_MC_IDENTITY as u32);
        (*data).left_pad = left_pad;
        (*data).right_pad = right_pad;
        (*data).top_pad = top_pad;
        (*data).bottom_pad = bottom_pad;
        (*data).ar_padding = ar_padding;
        (*data).width = src.y_crop_width;
        (*data).height = src.y_crop_height;
        (*data).is_10x3 = c_int::from(target == GrainTarget::Planes && (*dec).tryhdr10x3 != 0 && src.hbd != 0);
        let direct_v2 = matches!(target, GrainTarget::SemiPlanarTexture(_)) && !FG_LEGACY.load(Ordering::Relaxed);
        // The shared-memory shader processes three adjacent grain blocks per group. The direct
        // shader processes one. Both read the offsets of every block they process.
        let blocks_per_group = if direct_v2 { 1 } else { 3 };
        let groups_x = (src.y_crop_width as u32).div_ceil(luma_subblock_size_x as u32 * blocks_per_group);
        let groups_y = (src.y_crop_height as u32).div_ceil(luma_subblock_size_y as u32);
        let stride = groups_x * blocks_per_group;
        (*data).random_offset_stride = stride as c_int;

        let packed = direct_v2 && params.bit_depth == 10;
        let scaling_lut = &raw mut (*data).scaling_lut;
        write_scaling_lut(&params.scaling_points_y[..params.num_y_points as usize], scaling_lut, 0, packed);
        if params.chroma_scaling_from_luma != 0 {
            // Read back from the record, as the C++ did.
            for i in 0..256 {
                (*scaling_lut)[i][1] = (*scaling_lut)[i][0];
                (*scaling_lut)[i][2] = (*scaling_lut)[i][0];
            }
        } else {
            write_scaling_lut(&params.scaling_points_cb[..params.num_cb_points as usize], scaling_lut, 1, packed);
            write_scaling_lut(&params.scaling_points_cr[..params.num_cr_points as usize], scaling_lut, 2, packed);
        }

        // Start the random register at each visible stripe (AV1 7.18.3.5). Continue it through
        // any extra blocks in the final group so all shader reads have initialized offsets.
        let random_offset = grain_buffer.add(GRAIN_BUFFER_TEMPLATE_BYTES).cast::<c_int>();
        for row in 0..groups_y {
            let mut rng = Rng::new(row as c_int * luma_subblock_size_y, params.random_seed);
            for col in 0..stride {
                *random_offset.add((row * stride + col) as usize) = rng.get_random_number(8);
            }
        }

        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let shader = pipeline(match target {
            GrainTarget::Planes => &raw const (*lib).shader_filmgrain_filter,
            GrainTarget::SemiPlanar => &raw const (*lib).shader_filmgrain_filter_sp,
            GrainTarget::SemiPlanarTexture(_) if direct_v2 => &raw const (*lib).shader_filmgrain_filter_sp_tex_v2,
            GrainTarget::SemiPlanarTexture(_) => &raw const (*lib).shader_filmgrain_filter_sp_tex,
        });
        commands.set_pipeline(shader);
        commands.set_layout(shader.layout());
        commands.bind_read(0, frame_buffers.location());
        commands.bind_read(1, template_location);
        commands.bind_read(2, buffer(td.filmgrain_rand_offset).location().offset(GRAIN_BUFFER_TEMPLATE_BYTES as u64));
        if let GrainTarget::SemiPlanarTexture(pair) = target {
            // The layout without a buffer to write: the constants, then the planes of the texture.
            let views = wrapped_ref::<_, PlaneViews>(&(*dec).plane_views).expect("a direct frame has its views");
            let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device).expect("the decoder has its device");
            commands.bind_constants(3, constants.location);
            commands.bind_write_planes(4, Device::new(device), views, pair);
        } else {
            commands.bind_write(3, frame_buffers.location());
            commands.bind_constants(4, constants.location);
        }
        commands.dispatch(groups_x, groups_y, 1);
        commands.write_barrier(frame_buffers);

        perf_gpu_mark(dec, AV1_PERF_GPU_FILM_GRAIN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `init_scaling_function` of the C++, as it was written.
    fn init_scaling_function_cpp(
        scaling_points: &[[c_int; 2]],
        num_points: c_int,
        scaling_lut: &mut [[c_int; 4]; 256],
        p: usize,
    ) {
        if num_points == 0 {
            for i in 0..256 {
                scaling_lut[i][p] = 0;
            }
            return;
        }
        for i in 0..scaling_points[0][0] {
            scaling_lut[i as usize][p] = scaling_points[0][1];
        }
        for point in 0..(num_points - 1) as usize {
            let delta_y = scaling_points[point + 1][1] - scaling_points[point][1];
            let delta_x = scaling_points[point + 1][0] - scaling_points[point][0];
            let delta: i64 = (delta_y * ((65536 + (delta_x >> 1)) / delta_x)) as i64;
            for x in 0..delta_x {
                scaling_lut[(scaling_points[point][0] + x) as usize][p] =
                    scaling_points[point][1] + ((x as i64 * delta + 32768) >> 16) as c_int;
            }
        }
        for i in scaling_points[(num_points - 1) as usize][0]..256 {
            scaling_lut[i as usize][p] = scaling_points[(num_points - 1) as usize][1];
        }
    }

    #[test]
    fn scaling_function_as_in_cpp() {
        let cases: [&[[c_int; 2]]; 5] = [
            &[],
            &[[0, 20]],
            &[[16, 100], [64, 40], [200, 255]],
            &[[0, 0], [1, 255], [2, 0], [128, 128], [255, 3]],
            &[[40, 60], [41, 61], [250, 10]],
        ];
        for points in cases {
            for p in 0..3 {
                let mut expected = [[7; 4]; 256];
                let mut lut = [[7; 4]; 256];
                init_scaling_function_cpp(points, points.len() as c_int, &mut expected, p);
                // SAFETY: `lut` is a table to write.
                unsafe { write_scaling_lut(points, &mut lut, p, false) };
                assert_eq!(lut, expected);
            }
        }
    }

    #[test]
    fn packed_scaling_matches_interpolation_for_every_pair_of_knots() {
        for scale in 0..=255 {
            for next in 0..=255 {
                let packed = pack_10bit_scaling(scale, next) as u32;
                for fraction in 0..4 {
                    let expected = scale + (((next - scale) * fraction + 2) >> 2);
                    assert_eq!(((packed >> (8 * fraction)) & 255) as c_int, expected);
                }
            }
        }
    }

    #[test]
    fn packed_table_matches_legacy_lut_including_the_last_knot() {
        let points = [[7, 230], [11, 40], [15, 165], [250, 5], [255, 53]];
        let mut knots = [[0; 4]; 256];
        let mut packed = [[0; 4]; 256];
        // SAFETY: both tables are valid for writing, including the component used for V.
        unsafe {
            write_scaling_lut(&points, &mut knots, 2, false);
            write_scaling_lut(&points, &mut packed, 2, true);
        }
        for index in 0..1024 {
            let knot = index >> 2;
            let fraction = index & 3;
            let scale = knots[knot][2];
            let next = knots[(knot + 1).min(255)][2];
            let expected = scale + (((next - scale) * fraction as c_int + 2) >> 2);
            assert_eq!(((packed[knot][2] as u32 >> (8 * fraction)) & 255) as c_int, expected, "{index}");
        }
    }

    /// `RNG` of the C++, as it was written.
    struct RngCpp {
        random_register: u16,
    }

    impl RngCpp {
        fn init(&mut self, luma_line: c_int, seed: u16) {
            let msb: u16 = (seed >> 8) & 255;
            let lsb: u16 = seed & 255;
            self.random_register = (msb << 8) + lsb;
            let luma_num = luma_line >> 5;
            self.random_register ^= (((luma_num * 37 + 178) & 255) << 8) as u16;
            self.random_register ^= ((luma_num * 173 + 105) & 255) as u16;
        }

        fn get_random_number(&mut self, bits: c_int) -> c_int {
            let r = self.random_register;
            let bit: u16 = (r ^ (r >> 1) ^ (r >> 3) ^ (r >> 12)) & 1;
            self.random_register = (r >> 1) | (bit << 15);
            ((self.random_register >> (16 - bits)) as c_int) & ((1 << bits) - 1)
        }
    }

    #[test]
    fn random_register_as_in_cpp() {
        for seed in [0u16, 1, 0x1234, 0xffff, 0xb5a1] {
            for line in [0, 32, 64, 1056, 2144] {
                let mut expected = RngCpp { random_register: 0 };
                expected.init(line, seed);
                let mut rng = Rng::new(line, seed);
                for _ in 0..200 {
                    assert_eq!(rng.get_random_number(8), expected.get_random_number(8));
                }
            }
        }
    }
}

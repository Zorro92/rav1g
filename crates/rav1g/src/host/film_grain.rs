// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Film grain: the grain templates of the frame are generated on the GPU and added to the frame on its
//! way into the output image. From `av1_filmgrain_run` of `libav1/dx/loopfilters.cpp`.

use std::ffi::c_int;

use super::core::{alloc_constants, buffer, commands, perf_gpu_mark, pipeline};
use crate::sys::host::{AOM_CICP_MC_IDENTITY, AV1_COMMON, AV1_PERF_GPU_FILM_GRAIN, Av1Core, HwFrameBuffer};

/// The film grain parameters of a frame as the shaders read them.
#[repr(C)]
#[derive(Clone, Copy)]
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

/// The scaling function of a plane, component `p` of `scaling_lut`: the piecewise linear function through
/// `points` (value, scaling), at the values 0..255.
///
/// # Safety
/// `scaling_lut` is valid for writing.
unsafe fn init_scaling_function(points: &[[c_int; 2]], scaling_lut: *mut [[c_int; 4]; 256], p: usize) {
    // SAFETY: the caller's contract; only component `p` of the 256 entries is written.
    let set = |index: c_int, value: c_int| unsafe { (*scaling_lut)[index as usize][p] = value };
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        for i in 0..256 {
            set(i, 0);
        }
        return;
    };
    for i in 0..first[0] {
        set(i, first[1]);
    }
    for pair in points.windows(2) {
        let delta_y = pair[1][1] - pair[0][1];
        let delta_x = pair[1][0] - pair[0][0];
        let delta = i64::from(delta_y * ((65536 + (delta_x >> 1)) / delta_x));
        for x in 0..delta_x {
            set(pair[0][0] + x, pair[0][1] + ((i64::from(x) * delta + 32768) >> 16) as c_int);
        }
    }
    for i in last[0]..256 {
        set(i, last[1]);
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

/// `av1_filmgrain_run` of the C++: records the grain synthesis for the output of the frame of the
/// current command list. The frame with grain goes into the layout the decoder's back buffer has, which
/// is that of the output image. Without `enable_chroma` only luma gets grain.
///
/// # Safety
/// `dec` is the decoder while it records a frame that is output; `cm` is the frame's.
pub(crate) unsafe fn filmgrain_run(dec: *mut Av1Core, cm: &AV1_COMMON, enable_chroma: c_int) {
    // SAFETY: the caller's contract; the random offsets are a mapped buffer of the slot, sized for the
    // largest frame of the decoder.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let params = &cm.film_grain_params;

        let gen_constants = alloc_constants::<FilmGrainGenData>(&mut td.command_buffer);
        let gen_data = gen_constants.data;

        (*gen_data).params.apply_grain = params.apply_grain;
        (*gen_data).params.ar_coeff_lag = params.ar_coeff_lag;
        (*gen_data).params.ar_coeff_shift = params.ar_coeff_shift;
        (*gen_data).params.bit_depth = params.bit_depth;
        (*gen_data).params.cb_luma_mult = params.cb_luma_mult;
        (*gen_data).params.cb_mult = params.cb_mult;
        (*gen_data).params.cb_offset = params.cb_offset;
        (*gen_data).params.chroma_scaling_from_luma = params.chroma_scaling_from_luma;
        (*gen_data).params.clip_to_restricted_range = params.clip_to_restricted_range;
        (*gen_data).params.cr_luma_mult = params.cr_luma_mult;
        (*gen_data).params.cr_mult = params.cr_mult;
        (*gen_data).params.cr_offset = params.cr_offset;
        (*gen_data).params.grain_scale_shift = params.grain_scale_shift;
        (*gen_data).params.num_cb_points = params.num_cb_points;
        (*gen_data).params.num_cr_points = params.num_cr_points;
        (*gen_data).params.num_y_points = params.num_y_points;
        (*gen_data).params.overlap_flag = params.overlap_flag;
        (*gen_data).params.random_seed = u32::from(params.random_seed);
        (*gen_data).params.update_parameters = params.update_parameters;
        (*gen_data).params.scaling_shift = params.scaling_shift;
        for i in 0..24 {
            (*gen_data).params.ar_coeffs[0][i][0] = params.ar_coeffs_y[i];
            (*gen_data).params.ar_coeffs[1][i][0] = params.ar_coeffs_cb[i];
            (*gen_data).params.ar_coeffs[2][i][0] = params.ar_coeffs_cr[i];
        }
        (*gen_data).params.ar_coeffs[1][24][0] = params.ar_coeffs_cb[24];
        (*gen_data).params.ar_coeffs[2][24][0] = params.ar_coeffs_cr[24];

        for i in 0..14 {
            (*gen_data).params.scaling_points[i][0][0] = params.scaling_points_y[i][0];
            (*gen_data).params.scaling_points[i][1][0] = params.scaling_points_y[i][1];
            if i < 10 {
                (*gen_data).params.scaling_points[i][0][1] = params.scaling_points_cb[i][0];
                (*gen_data).params.scaling_points[i][1][1] = params.scaling_points_cb[i][1];
                (*gen_data).params.scaling_points[i][0][2] = params.scaling_points_cr[i][0];
                (*gen_data).params.scaling_points[i][1][2] = params.scaling_points_cr[i][1];
            }
        }

        let left_pad = 3;
        let right_pad = 3; // padding to offset for AR coefficients
        let top_pad = 3;
        let bottom_pad = 0;
        let ar_padding = 3; // maximum lag used for stabilization of AR coefficients
        let luma_subblock_size_y = 32;
        let luma_subblock_size_x = 32;
        let chroma_subblock_size_y = luma_subblock_size_y >> 1;
        let chroma_subblock_size_x = luma_subblock_size_x >> 1;
        let luma_block_size_y = top_pad + 2 * ar_padding + luma_subblock_size_y * 2 + bottom_pad;
        let luma_block_size_x = left_pad + 2 * ar_padding + luma_subblock_size_x * 2 + 2 * ar_padding + right_pad;
        let chroma_block_size_y = top_pad + ar_padding + chroma_subblock_size_y * 2 + bottom_pad;
        let chroma_block_size_x = left_pad + ar_padding + chroma_subblock_size_x * 2 + ar_padding + right_pad;

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
        (*gen_data).dst_offset_u = luma_block_size_x * luma_block_size_y;
        (*gen_data).dst_offset_v = luma_block_size_x * luma_block_size_y + chroma_block_size_x * chroma_block_size_y;
        // The positions the auto-regression of a sample reads: the rows above, then the samples to its left;
        // for chroma also the luma sample at its place, when luma has grain.
        let lag = params.ar_coeff_lag;
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
        if params.num_y_points > 0 {
            (*gen_data).pred_pos[pos_ar_index][0][1] = 0;
            (*gen_data).pred_pos[pos_ar_index][1][1] = 0;
            (*gen_data).pred_pos[pos_ar_index][2][1] = 1;
        }

        let constants = alloc_constants::<FilmGrainData>(&mut td.command_buffer);
        let data = constants.data;

        // The parameters as the first record has them, every byte of them: the record of the filter is
        // compared between decoders with what it holds besides the fields written above.
        (&raw mut (*data).params).copy_from_nonoverlapping(&raw const (*gen_data).params, 1);
        (*data).enable_chroma = enable_chroma;

        let plane_words = |buffer: &HwFrameBuffer| {
            buffer.planes.map(|plane| [plane.stride, plane.offset, plane.res_stride, plane.res_offset])
        };
        (*data).dst_planes = plane_words(&(*dec).back_buffer1);

        let src = &*td.dst_frame_buffer;
        (*data).src_planes = plane_words(src);

        (*data).luma_grain_stride = luma_block_size_x;
        (*data).chroma_grain_stride = chroma_block_size_x;
        (*data).grain_offset_u = luma_block_size_x * luma_block_size_y;
        (*data).grain_offset_v = luma_block_size_x * luma_block_size_y + chroma_block_size_x * chroma_block_size_y;
        (*data).mc_identity =
            c_int::from((*cm.cur_frame).buf.matrix_coefficients as u32 == AOM_CICP_MC_IDENTITY as u32);
        (*data).left_pad = left_pad;
        (*data).right_pad = right_pad;
        (*data).top_pad = top_pad;
        (*data).bottom_pad = bottom_pad;
        (*data).ar_padding = ar_padding;
        (*data).width = src.y_crop_width;
        (*data).height = src.y_crop_height;
        (*data).random_offset_stride = (src.width / 32) + 1;
        (*data).is_10x3 = c_int::from((*dec).tryhdr10x3 != 0 && src.hbd != 0);
        let scaling_lut = &raw mut (*data).scaling_lut;
        init_scaling_function(&params.scaling_points_y[..params.num_y_points as usize], scaling_lut, 0);
        if params.chroma_scaling_from_luma != 0 {
            // Read back from the record, as the C++ did.
            for i in 0..256 {
                (*scaling_lut)[i][1] = (*scaling_lut)[i][0];
                (*scaling_lut)[i][2] = (*scaling_lut)[i][0];
            }
        } else {
            init_scaling_function(&params.scaling_points_cb[..params.num_cb_points as usize], scaling_lut, 1);
            init_scaling_function(&params.scaling_points_cr[..params.num_cr_points as usize], scaling_lut, 2);
        }

        // The random offset of every grain block of 32x32 luma samples into the grain templates.
        let random_offset = (*td.filmgrain_rand_offset).host_ptr.cast::<c_int>();
        let stride = (*data).random_offset_stride;
        let step = luma_subblock_size_y >> 1;
        let mut y = 0;
        while y < src.height / 2 {
            let mut rng = Rng::new(y * 2, params.random_seed);
            let mut x = 0;
            while x < src.width / 2 {
                *random_offset.offset(((y / step) * stride + x / step) as isize) = rng.get_random_number(8);
                x += step;
            }
            y += step;
        }

        let commands = commands(dec);
        let lib = (*dec).shader_lib;
        let noise = buffer((*dec).filmgrain_noise);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
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

        let shader = pipeline(&raw const (*lib).shader_filmgrain_filter);
        commands.set_pipeline(shader);
        commands.set_layout(shader.layout());
        commands.bind_read(0, frame_buffers.location());
        commands.bind_read(1, noise.location());
        commands.bind_read(2, buffer(td.filmgrain_rand_offset).location());
        commands.bind_write(3, frame_buffers.location());
        commands.bind_constants(4, constants.location);
        commands.dispatch((src.width / 32 + 1) as u32, (src.height / 32 + 1) as u32, 1);
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
                unsafe { init_scaling_function(points, &mut lut, p) };
                assert_eq!(lut, expected);
            }
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

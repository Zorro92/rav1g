/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The C++ side of libav1/dx/av1_compute.cpp for the cargo build. Rust has what the file does
// (crates/rav1g/src/host/compute.rs): the pipeline cache functions of aom/aom_decoder.h are Rust functions
// of those names, and the two functions the frame pipeline calls are calls of it here.
//
// The bytecode of the shaders comes from the headers of the FXC build, which only a C++ file can include:
// this one, which hands them to the Rust as a table by the name of the header. The table has the shaders
// the decoder makes pipelines of, in the order av1_compute.cpp creates them.

#include "dx/av1_compute.h"
#include "dx/types.h"

#include "dx/shaders/h/idct4x4.h"
#include "dx/shaders/h/idct4x8.h"
#include "dx/shaders/h/idct4x16.h"
#include "dx/shaders/h/idct8x4.h"
#include "dx/shaders/h/idct8x8.h"
#include "dx/shaders/h/idct8x16.h"
#include "dx/shaders/h/idct8x32.h"
#include "dx/shaders/h/idct16x4.h"
#include "dx/shaders/h/idct16x8.h"
#include "dx/shaders/h/idct16x16.h"
#include "dx/shaders/h/idct16x32.h"
#include "dx/shaders/h/idct16x64.h"
#include "dx/shaders/h/idct32x8.h"
#include "dx/shaders/h/idct32x16.h"
#include "dx/shaders/h/idct32x32.h"
#include "dx/shaders/h/idct32x64.h"
#include "dx/shaders/h/idct64x16.h"
#include "dx/shaders/h/idct64x32.h"
#include "dx/shaders/h/idct64x64.h"
#include "dx/shaders/h/idct_lossless.h"
#include "dx/shaders/h/idct_sort_blocks.h"
#include "dx/shaders/h/fill_buffer.h"
#include "dx/shaders/h/loop_restoration.h"
#include "dx/shaders/h/film_grain_filter.h"
#include "dx/shaders/h/film_grain_gen_luma.h"
#include "dx/shaders/h/film_grain_gen_chroma.h"
#include "dx/shaders/h/cdef_filter.h"
#include "dx/shaders/h/cdef_filter_v2.h"
#include "dx/shaders/h/intra_v2.h"
#include "dx/shaders/h/intra_v2_hbd.h"
#include "dx/shaders/h/gen_pred_blocks.h"
#include "dx/shaders/h/gen_pred_blocks_fast.h"
#include "dx/shaders/h/inter_fast.h"
#include "dx/shaders/h/inter_compound_fast.h"
#include "dx/shaders/h/inter_fast_hbd.h"
#include "dx/shaders/h/inter_compound_fast_hbd.h"
#include "dx/shaders/h/gen_lf_blocks_vert.h"
#include "dx/shaders/h/gen_lf_blocks_hor.h"
#include "dx/shaders/h/copy_plane.h"
#include "dx/shaders/h/copy_plane_10bit10x3.h"
#include "dx/shaders/h/upscaling.h"
#include "dx/shaders/h/inter_2x2chroma.h"
#include "dx/shaders/h/inter_compound.h"
#include "dx/shaders/h/inter_compound_2x2.h"
#include "dx/shaders/h/inter_compound_diff_chroma.h"
#include "dx/shaders/h/inter_compound_diff_luma.h"
#include "dx/shaders/h/inter_compound_masked.h"
#include "dx/shaders/h/inter_main.h"
#include "dx/shaders/h/inter_obmc_above.h"
#include "dx/shaders/h/inter_obmc_left.h"
#include "dx/shaders/h/inter_warp.h"
#include "dx/shaders/h/inter_scale_warp_compound.h"
#include "dx/shaders/h/intra_filter.h"
#include "dx/shaders/h/intra_main.h"
#include "dx/shaders/h/intra_main_opt.h"
#include "dx/shaders/h/inter_ext_borders.h"
#include "dx/shaders/h/reconstruct_block.h"
#include "dx/shaders/h/loopfilter_vert.h"
#include "dx/shaders/h/loopfilter_hor.h"
#include "dx/shaders/h/inter_compound_2x2_hbd.h"
#include "dx/shaders/h/inter_2x2chroma_hbd.h"
#include "dx/shaders/h/inter_compound_diff_chroma_hbd.h"
#include "dx/shaders/h/inter_compound_diff_luma_hbd.h"
#include "dx/shaders/h/inter_compound_hbd.h"
#include "dx/shaders/h/inter_compound_masked_hbd.h"
#include "dx/shaders/h/inter_main_hbd.h"
#include "dx/shaders/h/inter_obmc_above_hbd.h"
#include "dx/shaders/h/inter_obmc_left_hbd.h"
#include "dx/shaders/h/inter_scale_warp_compound_hbd.h"
#include "dx/shaders/h/inter_warp_hbd.h"
#include "dx/shaders/h/intra_filter_hbd.h"
#include "dx/shaders/h/intra_main_hbd.h"
#include "dx/shaders/h/intra_main_hbd_opt.h"
#include "dx/shaders/h/inter_ext_borders_hbd.h"
#include "dx/shaders/h/reconstruct_block_hbd.h"
#include "dx/shaders/h/loopfilter_vert_hbd.h"
#include "dx/shaders/h/loopfilter_hor_hbd.h"
#include "dx/shaders/h/inter_scale.h"
#include "dx/shaders/h/inter_scale_2x2chroma.h"
#include "dx/shaders/h/inter_scale_compound.h"
#include "dx/shaders/h/inter_scale_compound_diff_chroma.h"
#include "dx/shaders/h/inter_scale_compound_diff_luma.h"
#include "dx/shaders/h/inter_scale_compound_masked.h"
#include "dx/shaders/h/inter_scale_obmc_above.h"
#include "dx/shaders/h/inter_scale_obmc_left.h"
#include "dx/shaders/h/inter_scale_hbd.h"
#include "dx/shaders/h/inter_scale_2x2chroma_hbd.h"
#include "dx/shaders/h/inter_scale_compound_hbd.h"
#include "dx/shaders/h/inter_scale_compound_diff_chroma_hbd.h"
#include "dx/shaders/h/inter_scale_compound_diff_luma_hbd.h"
#include "dx/shaders/h/inter_scale_compound_masked_hbd.h"
#include "dx/shaders/h/inter_scale_obmc_above_hbd.h"
#include "dx/shaders/h/inter_scale_obmc_left_hbd.h"

extern "C" {
int rav1g_wait_shader_create_complete(compute_shader_lib *lib);
HRESULT rav1g_upload_luts(Av1Core *dec);
}

int wait_shader_create_complete(compute_shader_lib *lib) { return rav1g_wait_shader_create_complete(lib); }

HRESULT av1_upload_luts(Av1Core *dec) { return rav1g_upload_luts(dec); }

struct rav1g_shader_bytecode {
  const char *name;
  const BYTE *code;
  unsigned int size;
};

#define RAV1G_SHADER(name) {#name, name, (unsigned int)sizeof(name)}
static const rav1g_shader_bytecode rav1g_shaders[] = {
    RAV1G_SHADER(idct4x4),
    RAV1G_SHADER(idct4x8),
    RAV1G_SHADER(idct4x16),
    RAV1G_SHADER(idct8x4),
    RAV1G_SHADER(idct8x8),
    RAV1G_SHADER(idct8x16),
    RAV1G_SHADER(idct8x32),
    RAV1G_SHADER(idct16x4),
    RAV1G_SHADER(idct16x8),
    RAV1G_SHADER(idct16x16),
    RAV1G_SHADER(idct16x32),
    RAV1G_SHADER(idct16x64),
    RAV1G_SHADER(idct32x8),
    RAV1G_SHADER(idct32x16),
    RAV1G_SHADER(idct32x32),
    RAV1G_SHADER(idct32x64),
    RAV1G_SHADER(idct64x16),
    RAV1G_SHADER(idct64x32),
    RAV1G_SHADER(idct64x64),
    RAV1G_SHADER(idct_lossless),
    RAV1G_SHADER(idct_sort_blocks),
    RAV1G_SHADER(fill_buffer),
    RAV1G_SHADER(loop_restoration),
    RAV1G_SHADER(film_grain_filter),
    RAV1G_SHADER(film_grain_gen_luma),
    RAV1G_SHADER(film_grain_gen_chroma),
    RAV1G_SHADER(cdef_filter),
    RAV1G_SHADER(cdef_filter_v2),
    RAV1G_SHADER(intra_v2),
    RAV1G_SHADER(intra_v2_hbd),
    RAV1G_SHADER(gen_pred_blocks),
    RAV1G_SHADER(gen_pred_blocks_fast),
    RAV1G_SHADER(inter_fast),
    RAV1G_SHADER(inter_compound_fast),
    RAV1G_SHADER(inter_fast_hbd),
    RAV1G_SHADER(inter_compound_fast_hbd),
    RAV1G_SHADER(gen_lf_blocks_vert),
    RAV1G_SHADER(gen_lf_blocks_hor),
    RAV1G_SHADER(copy_plane),
    RAV1G_SHADER(copy_plane_10bit10x3),
    RAV1G_SHADER(upscaling),
    RAV1G_SHADER(inter_2x2chroma),
    RAV1G_SHADER(inter_compound),
    RAV1G_SHADER(inter_compound_2x2),
    RAV1G_SHADER(inter_compound_diff_chroma),
    RAV1G_SHADER(inter_compound_diff_luma),
    RAV1G_SHADER(inter_compound_masked),
    RAV1G_SHADER(inter_main),
    RAV1G_SHADER(inter_obmc_above),
    RAV1G_SHADER(inter_obmc_left),
    RAV1G_SHADER(inter_warp),
    RAV1G_SHADER(inter_scale_warp_compound),
    RAV1G_SHADER(intra_filter),
    RAV1G_SHADER(intra_main),
    RAV1G_SHADER(intra_main_opt),
    RAV1G_SHADER(inter_ext_borders),
    RAV1G_SHADER(reconstruct_block),
    RAV1G_SHADER(loopfilter_vert),
    RAV1G_SHADER(loopfilter_hor),
    RAV1G_SHADER(inter_compound_2x2_hbd),
    RAV1G_SHADER(inter_2x2chroma_hbd),
    RAV1G_SHADER(inter_compound_diff_chroma_hbd),
    RAV1G_SHADER(inter_compound_diff_luma_hbd),
    RAV1G_SHADER(inter_compound_hbd),
    RAV1G_SHADER(inter_compound_masked_hbd),
    RAV1G_SHADER(inter_main_hbd),
    RAV1G_SHADER(inter_obmc_above_hbd),
    RAV1G_SHADER(inter_obmc_left_hbd),
    RAV1G_SHADER(inter_scale_warp_compound_hbd),
    RAV1G_SHADER(inter_warp_hbd),
    RAV1G_SHADER(intra_filter_hbd),
    RAV1G_SHADER(intra_main_hbd),
    RAV1G_SHADER(intra_main_hbd_opt),
    RAV1G_SHADER(inter_ext_borders_hbd),
    RAV1G_SHADER(reconstruct_block_hbd),
    RAV1G_SHADER(loopfilter_vert_hbd),
    RAV1G_SHADER(loopfilter_hor_hbd),
    RAV1G_SHADER(inter_scale),
    RAV1G_SHADER(inter_scale_2x2chroma),
    RAV1G_SHADER(inter_scale_compound),
    RAV1G_SHADER(inter_scale_compound_diff_chroma),
    RAV1G_SHADER(inter_scale_compound_diff_luma),
    RAV1G_SHADER(inter_scale_compound_masked),
    RAV1G_SHADER(inter_scale_obmc_above),
    RAV1G_SHADER(inter_scale_obmc_left),
    RAV1G_SHADER(inter_scale_hbd),
    RAV1G_SHADER(inter_scale_2x2chroma_hbd),
    RAV1G_SHADER(inter_scale_compound_hbd),
    RAV1G_SHADER(inter_scale_compound_diff_chroma_hbd),
    RAV1G_SHADER(inter_scale_compound_diff_luma_hbd),
    RAV1G_SHADER(inter_scale_compound_masked_hbd),
    RAV1G_SHADER(inter_scale_obmc_above_hbd),
    RAV1G_SHADER(inter_scale_obmc_left_hbd),
};

extern "C" const rav1g_shader_bytecode *rav1g_shader_table(unsigned int *count) {
  *count = (unsigned int)(sizeof(rav1g_shaders) / sizeof(rav1g_shaders[0]));
  return rav1g_shaders;
}

/*
 * One shader as a translation unit of sync_check. run.sh builds this file once per shader:
 *   SHADER_FILE    the generated C++ of the shader (to_cpp.py)
 *   SHADER_ENTRY   its entry file (thread group size, call with system values, garbage)
 *   SHADER_NS      namespace for its globals
 *   SHADER_NAME    its name, for the report
 *   SHADER_FAMILY  which set of buffers and constants it takes, see below
 * The input is made here: random, but shaped so that the shader runs through all its paths and so
 * that two thread groups never write the same words (the check compares what a shader writes under
 * two thread orders, and words that two groups fight over would differ for no fault of the shader).
 */
#include <stdio.h>

#include "hlsl.h"
#include "api.h"

static void scramble_bytes(void *p, size_t size, uint64_t *state) {
  uint8_t *bytes = (uint8_t *)p;
  for (size_t i = 0; i + 4 <= size; i += 4) {
    *state = *state * 6364136223846793005ull + 1442695040888963407ull;
    const uint32_t v = (uint32_t)(*state >> 32);
    memcpy(bytes + i, &v, 4);
  }
}

namespace SHADER_NS {

#include SHADER_FILE
#include SHADER_ENTRY

static void put32(uint8_t *buffer, size_t at, uint32_t v) { memcpy(buffer + at, &v, 4); }

template <class T>
static void random_fill(T *object, Rng *rng) {
  uint8_t *bytes = (uint8_t *)object;
  for (size_t i = 0; i < sizeof(T); ++i) bytes[i] = (uint8_t)rng->below(256);
}

#if SHADER_FAMILY == 1 || SHADER_FAMILY == 2
// The inverse transforms (idct_shader_common.h) and the lossless one.
#if SHADER_FAMILY == 2
#define N 4
#endif
static void setup(World *w, Rng *rng, Dispatch *d) {
  buf_blocks.ptr = w->ro[0];
  buf_blocks.size = kRoSize;
  buf_dst.ptr = w->rw[0];
  buf_dst.size = kRwSize;
  buf_input.ptr = w->rw[1];
  buf_input.size = kRwSize;
  for (int p = 0; p < 3; ++p) cb_planes.data[p] = uint4(1024, 4096 + p * 640 * 1024, 0, 0);
  cb_bitdepth = rng->below(2) ? 10 : 8;
  cb_index_offset = (uint)rng->below(4);
  const int blocks = 1 + rng->below(3 * 64 / N);
  cb_wicount = (uint)(blocks * N);
  // Every scan order is a permutation of the coefficient positions.
  const int positions = 4 * ScanSize;
  for (int type = 0; type < 3; ++type) {
    int *scan = &cb_scans.data[type * ScanSize].x;
    for (int i = 0; i < positions; ++i) scan[i] = i;
    for (int i = positions - 1; i > 0; --i) {
      const int other = rng->below(i + 1);
      const int swap = scan[i];
      scan[i] = scan[other];
      scan[other] = swap;
    }
  }
  for (int k = 0; k < blocks + 4; ++k) {
    const uint32_t word = (uint32_t)rng->below(positions + 1) | ((uint32_t)rng->below(3) << 11) |
                          ((uint32_t)rng->below(64) << 15) | ((uint32_t)rng->below(3) << 21);
    put32(w->ro[0], (size_t)k * 16, word);
    put32(w->ro[0], (size_t)k * 16 + 4, (uint32_t)k * 1024);                      // coefficients
    put32(w->ro[0], (size_t)k * 16 + 8, (uint32_t)(k % 8) * 16 | ((uint32_t)(k / 8) * 16 << 16));  // position
    put32(w->ro[0], (size_t)k * 16 + 12, 0);
  }
  d->groups_x = (blocks * N + 63) / 64;
}

#elif SHADER_FAMILY == 3
// Inter prediction (inter_common.h).
static void setup(World *w, Rng *rng, Dispatch *d) {
  pred_blocks.ptr = w->ro[0];
  pred_blocks.size = kRoSize;
  residuals.ptr = w->ro[1];
  residuals.size = kRoSize;
  comp_mask.ptr = w->ro[2];
  comp_mask.size = kRoSize;
  warp_blocks.ptr = w->ro[3];
  warp_blocks.size = kRoSize;
  warp_filter.ptr = w->ro[4];
  warp_filter.size = kRoSize;
  dst_frame.ptr = w->rw[0];
  dst_frame.size = kRwSize;
  // The references in the first megabyte, the planes that are written behind them.
  for (int p = 0; p < 3; ++p) cb_planes.data[p] = int4(1024, (1 << 20) + p * 440 * 1024, 2048, p * 16);
  for (int i = 0; i < 3 * RefCount; ++i)
    cb_refplanes.data[i] = int4(1024, 16384 + i * 40000 + 4 * rng->below(4), 40 + rng->below(160), 4 + rng->below(26));
  cb_dims.data[0] = int4(40 + rng->below(160), 4 + rng->below(26), 0, 0);
  cb_dims.data[1] = int4(40 + rng->below(160), 4 + rng->below(26), 0, 0);
  cb_pixel_max = int4(1023, 1023, 1023, 1023);
  for (int i = 0; i < 8 * 16; ++i) {
    for (int k = 0; k < 2; ++k)
      cb_kernels.data[i].data[k] = int4(rng->below(129) - 32, rng->below(129) - 32, rng->below(129) - 32, rng->below(129) - 32);
  }
  for (int i = 0; i < 8; ++i)
    cb_scale.data[i] = int4((1 << 14) + rng->below(4096) - 2048, 512 + rng->below(1537), (1 << 14) + rng->below(4096) - 2048,
                            512 + rng->below(1537));
  random_fill(&cb_obmc_mask, rng);
  for (int i = 0; i < RefCount; ++i) {
    cb_gm_warp.data[i].info0 = int4(rng->below(2), 0, rng->below(1 << 16), rng->below(1 << 16));
    cb_gm_warp.data[i].info1 = int4((1 << 16) + rng->below(2048) - 1024, rng->below(2048) - 1024, rng->below(2048) - 1024,
                                    (1 << 16) + rng->below(2048) - 1024);
    cb_gm_warp.data[i].info2 = int4(rng->below(512) - 256, rng->below(512) - 256, rng->below(512) - 256, rng->below(512) - 256);
  }
  cb_width_log2 = (uint)rng->below(3);
  cb_height_log2 = (uint)rng->below(3);
  const int unit = 4 << (cb_width_log2 + cb_height_log2);
  cb_wi_count = (uint)(unit * (1 + rng->below(192 / unit)));
  cb_pass_offset = (uint)rng->below(8);
  // Block records. The flags are the same for four records in a row: threads that hand data to each
  // other belong to blocks of one kind.
  uint32_t flags = 0;
  for (int k = 0; k < 208; ++k) {
    if ((k & 3) == 0) {
      flags = (uint32_t)rng->below(3) | ((uint32_t)rng->below(RefCount) << 2) | ((uint32_t)rng->below(512) << 5) |
              ((uint32_t)rng->below(RefCount) << 14) | ((uint32_t)rng->below(1 << 15) << 17);
      // Often: a block of a pair that is written as one word, a block with a mask made from the
      // difference of its two predictions.
      if (rng->below(2)) flags |= 1u << 25;
      if (rng->below(3) == 0) flags = (flags & 0x3fffffffu) | (2u << 30);
    }
    const uint32_t noskip = (uint32_t)rng->below(2) << 13;
    const uint32_t pos = (uint32_t)(k % 16) * 8 | ((uint32_t)(k / 16) * 8 << 16);
    put32(w->ro[0], (size_t)k * 16, pos);
    put32(w->ro[0], (size_t)k * 16 + 4, flags | noskip);
    put32(w->ro[0], (size_t)k * 16 + 8, ((uint32_t)(rng->below(129) - 64) << 16) | (uint32_t)((rng->below(129) - 64) & 0xffff));
    put32(w->ro[0], (size_t)k * 16 + 12, ((uint32_t)(rng->below(129) - 64) << 16) | (uint32_t)((rng->below(129) - 64) & 0xffff));
    // The same block as a warp block of 48 bytes.
    put32(w->ro[3], (size_t)k * 48, pos);
    put32(w->ro[3], (size_t)k * 48 + 4, (flags & 0x1f) | noskip);
    put32(w->ro[3], (size_t)k * 48 + 8, (uint32_t)rng->below(1 << 20));
    put32(w->ro[3], (size_t)k * 48 + 12, (uint32_t)rng->below(1 << 20));
    put32(w->ro[3], (size_t)k * 48 + 16, (uint32_t)((1 << 16) + rng->below(2048) - 1024));
    put32(w->ro[3], (size_t)k * 48 + 20, (uint32_t)(rng->below(2048) - 1024));
    put32(w->ro[3], (size_t)k * 48 + 24, (uint32_t)(rng->below(2048) - 1024));
    put32(w->ro[3], (size_t)k * 48 + 28, (uint32_t)((1 << 16) + rng->below(2048) - 1024));
    for (int i = 0; i < 4; ++i) put32(w->ro[3], (size_t)k * 48 + 32 + 4 * i, (uint32_t)(rng->below(512) - 256));
  }
  d->groups_x = ((int)cb_wi_count + 63) / 64;
}

#elif SHADER_FAMILY == 4
// The deblocking filters.
static void setup(World *w, Rng *rng, Dispatch *d) {
  lf_blocks.ptr = w->ro[0];
  lf_blocks.size = kRoSize;
  dst_frame.ptr = w->rw[0];
  dst_frame.size = kRwSize;
  for (int p = 0; p < 3; ++p) cb_planes.data[p] = int4(2048, 32768 + p * 1024 * 1024, 0, 0);
  for (int i = 0; i < 64; ++i) cb_limits.data[i] = int4(rng->below(24), rng->below(64), rng->below(16), 0);
  const int blocks = 1 + rng->below(48);
  cb_wicount = (uint)(blocks * 4);
  cb_plane = (uint)rng->below(3);
  cb_offset_base = (uint)rng->below(4);
  cb_block_cols = (uint)(4 + rng->below(10));
  cb_block_id_offset = (uint)rng->below(2);
  // Sixteen edge entries per block: level, filter length, distance to the next edge; 0 ends the list.
  for (int k = 0; k < 128; ++k) {
    uint16_t edges[16] = {};
    const int end = rng->below(4) ? 16 : rng->below(16);
    for (int at = 0; at < end;) {
      const int step = 1 + rng->below(4);
      const int level = rng->below(4) ? 1 + rng->below(63) : 0;
      edges[at] = (uint16_t)(level | (rng->below(4) << 6) | (step << 8));
      at += step;
    }
    memcpy(w->ro[0] + (size_t)k * 32, edges, 32);
  }
  d->groups_x = (blocks * 4 + 63) / 64;
}

#elif SHADER_FAMILY == 5
// Loop restoration.
static void setup(World *w, Rng *rng, Dispatch *d) {
  src.ptr = w->ro[0];
  src.size = kRoSize;
  LrTypeSgr.ptr = w->ro[1];
  LrTypeSgr.size = kRoSize;
  LrWiener.ptr = w->ro[2];
  LrWiener.size = kRoSize;
  dst.ptr = w->rw[0];
  dst.size = kRwSize;
  for (int i = 0; i < 16; ++i) data.Sgr_Params.data[i] = int4(rng->below(3), rng->below(2), rng->below(4096), rng->below(4096));
  const int hbd = rng->below(2);
  for (int p = 0; p < 3; ++p) {
    PlaneRestorationData &plane = pl.data[p];
    plane.plane.stride = 1024;
    plane.plane.offset = 8192;
    plane.plane.width = 40 + rng->below(60);
    plane.plane.height = 12 + rng->below(40);
    plane.units.Rows = 1 + rng->below(3);
    plane.units.Cols = 1 + rng->below(3);
    plane.units.Size = 32 << rng->below(2);
    plane.units.Stride = plane.units.Cols;
    plane.pp_offset = 300000;
    plane.dst_offset = 4096;
    plane.Lr_buffer_offset = rng->below(4);
    plane.subsampling = rng->below(2);
    plane.hbd = hbd;
    plane.bit_depth = hbd ? 10 : 8;
  }
  do_restoration = rng->below(8) != 0;
  plane_id = rng->below(3);
  for (int k = 0; k < 64; ++k) {
    put32(w->ro[1], (size_t)k * 16, (uint32_t)rng->below(3));
    put32(w->ro[1], (size_t)k * 16 + 4, (uint32_t)(rng->below(128) - 64));
    put32(w->ro[1], (size_t)k * 16 + 8, (uint32_t)(rng->below(128) - 32));
    put32(w->ro[1], (size_t)k * 16 + 12, (uint32_t)rng->below(16));
    for (int i = 0; i < 16; ++i) put32(w->ro[2], (size_t)k * 64 + 4 * i, (uint32_t)(rng->below(64) - 20));
  }
  d->groups_x = 4;
  d->groups_y = 4;
}

#elif SHADER_FAMILY == 6 || SHADER_FAMILY == 7
// Film grain: the grain templates (6) and their application to the picture (7).
static void grain_params(GrainParams *params, Rng *rng) {
  memset((void *)params, 0, sizeof(*params));
  params->num_y_points = rng->below(3);
  params->num_cb_points = rng->below(3);
  params->num_cr_points = rng->below(3);
  params->chroma_scaling_from_luma = rng->below(2);
  params->scaling_shift = 8 + rng->below(4);
  params->ar_coeff_lag = rng->below(4);
  params->ar_coeff_shift = 6 + rng->below(4);
  params->cb_mult = rng->below(256);
  params->cb_luma_mult = rng->below(256);
  params->cb_offset = rng->below(512);
  params->cr_mult = rng->below(256);
  params->cr_luma_mult = rng->below(256);
  params->cr_offset = rng->below(512);
  params->overlap_flag = rng->below(2);
  params->clip_to_restricted_range = rng->below(2);
  params->bit_depth = rng->below(2) ? 10 : 8;
  params->grain_scale_shift = rng->below(4);
  params->random_seed = (unsigned)rng->below(65536);
  for (int p = 0; p < 3; ++p) {
    for (int i = 0; i < 25; ++i) params->ar_coeffs.data[p].data[i] = int4(rng->below(256) - 128, 0, 0, 0);
  }
}

static void setup(World *w, Rng *rng, Dispatch *d) {
  int *ints0 = (int *)w->ro[0];
  int *ints1 = (int *)w->ro[1];
  int *ints2 = (int *)w->ro[2];
  int *out = (int *)w->rw[0];
#if SHADER_FAMILY == 6
  random.ptr = ints0;
  random.count = 65536;
  random_uv.ptr = ints1;
  random_uv.count = 65536;
  gaussian_sequence.ptr = ints2;
  gaussian_sequence.count = 2048;
  dst.ptr = out;
  dst.count = kRwSize / 4;
  for (int i = 0; i < 65536; ++i) {
    ints0[i] = rng->below(65536);
    ints1[i] = rng->below(65536);
  }
  for (int i = 0; i < 2048; ++i) ints2[i] = rng->below(4096) - 2048;
  for (int i = 0; i < 16384; ++i) out[i] = rng->below(512) - 256;
  grain_params(&data.params, rng);
  // The sizes and paddings of the decoder (loopfilters.cpp).
  data.luma_block_size_y = 73;
  data.luma_block_size_x = 82;
  data.luma_grain_stride = 82;
  data.chroma_block_size_y = 38;
  data.chroma_block_size_x = 44;
  data.chroma_grain_stride = 44;
  data.left_pad = 3;
  data.top_pad = 3;
  data.right_pad = 3;
  data.bottom_pad = 0;
  data.dst_offset_u = 73 * 82;
  data.dst_offset_v = 73 * 82 + 38 * 44;
  memset((void *)&data.pred_pos, 0, sizeof(data.pred_pos));
  const int lag = data.params.ar_coeff_lag;
  int pos = 0;
  for (int row = -lag; row < 0; ++row) {
    for (int col = -lag; col < lag + 1; ++col) {
      data.pred_pos.data[pos].data[0] = int4(row, row, 0, 0);
      data.pred_pos.data[pos].data[1] = int4(col, col, 0, 0);
      ++pos;
    }
  }
  for (int col = -lag; col < 0; ++col) {
    data.pred_pos.data[pos].data[0] = int4(0, 0, 0, 0);
    data.pred_pos.data[pos].data[1] = int4(col, col, 0, 0);
    ++pos;
  }
  if (data.params.num_y_points > 0) data.pred_pos.data[pos].data[2] = int4(0, 1, 0, 0);
  d->groups_y = SHADER_GROUPS_Y;
#else
  src.ptr = w->ro[0];
  src.size = kRoSize;
  grain_block.ptr = ints1;
  grain_block.count = kRoSize / 4;
  random_offset.ptr = ints2;
  random_offset.count = kRoSize / 4;
  dst.ptr = w->rw[0];
  dst.size = kRwSize;
  for (int i = 0; i < 16384; ++i) ints1[i] = rng->below(512) - 256;
  for (int i = 0; i < 1024; ++i) ints2[i] = rng->below(256);
  grain_params(&data.params, rng);
  for (int p = 0; p < 3; ++p) {
    data.src_planes.data[p] = int4(1024, 4096 + p * 200000, 0, 0);
    data.dst_planes.data[p] = int4(1024, 4096 + p * 200000, 0, 0);
  }
  data.enable_chroma = rng->below(4) != 0;
  data.random_offset_stride = 8;
  data.width = 100 + rng->below(60);
  data.height = 40 + rng->below(40);
  data.mc_identity = rng->below(2);
  data.luma_grain_stride = 82;
  data.chroma_grain_stride = 44;
  data.left_pad = 3;
  data.right_pad = 3;
  data.top_pad = 3;
  data.bottom_pad = 0;
  data.ar_padding = 3;
  data.grain_offset_u = 73 * 82;
  data.grain_offset_v = 73 * 82 + 38 * 44;
  data.is_10x3 = data.params.bit_depth == 10 && rng->below(2);
  data.pad = 0;
  for (int i = 0; i < 256; ++i) data.scaling_lut.data[i] = int4(rng->below(256), rng->below(256), rng->below(256), 0);
  d->groups_x = 2;
  d->groups_y = 2;
#endif
}
#else
#error "SHADER_FAMILY"
#endif

static void run(const uint32_t *group, const uint32_t *local) {
  shader_entry(uint3(group[0], group[1], group[2]), uint3(local[0], local[1], local[2]));
}

static ShaderApi api = { SHADER_NAME, kThreadsX, kThreadsY, kThreadsZ, kGroupMemoryArrays, setup, shader_scramble, run, nullptr };
static const int registered = register_shader(&api);

}  // namespace SHADER_NS

/*
 * Copyright 2020 Google LLC
 *
 */

/*
 * Copyright (c) 2020, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#define SubblockW 4
#define SubblockH 4

cbuffer IntraDataCommon : register(b0) {
  int4 cb_planes[3];
  int4 cb_flags;
  int4 cb_filter[5][8][2];
  int4 cb_mode_params_lut[16][7];
  int4 cb_sm_weight_arrays[128];
};

cbuffer PSSLIntraSRT : register(b1) {
  uint cb_wi_count;
  int cb_pass_offset;
  uint cb_width_log2;
  uint cb_height_log2;
  uint cb_fb_base_offset;
  int r[5];
};

ByteAddressBuffer pred_blocks : register(t0);
ByteAddressBuffer residuals : register(t1);
ByteAddressBuffer palette_buf : register(t2);
RWByteAddressBuffer dst_frame : register(u0);

[numthreads(64, 1, 1)] void main(uint3 thread
                                 : SV_DispatchThreadID) {
  if (thread.x >= cb_wi_count) return;

  const int w_log = cb_width_log2;
  const int h_log = cb_height_log2;
  const int subblock = thread.x & ((1 << (w_log + h_log)) - 1);
  const int block_index = cb_pass_offset + (thread.x >> (w_log + h_log));
  uint2 block = pred_blocks.Load2(block_index << 4);
  int x = SubblockW * ((block.x & 0xffff) + (subblock & ((1 << w_log) - 1)));
  int y = SubblockH * ((block.x >> 16) + (subblock >> w_log));

  const int plane = block.y & 3;

  const int do_recon = block.y & 4;
  const int do_palette = block.y & 8;

  const int output_stride = cb_planes[plane].x;
  const int output_offset = cb_planes[plane].y + x + y * output_stride;
  const int res_stride = cb_planes[plane].z;
  const int res_addr = cb_planes[plane].w + 2 * x + y * res_stride;
  for (int i = 0; i < 4; ++i) {
    uint addr = output_offset + i * output_stride;
    uint pixels;
    // A branch, not a select. Flattened, every thread loaded from palette_buf at the frame offset of its
    // row, palette block or not. A slot without a palette buffer has another buffer bound there
    // (av1_prediction_run_all), and that one is smaller than a frame: the load was outside it for the
    // last rows of an 8-bit frame and for the lower half of a frame of 16-bit samples. A device may answer
    // such a load with anything, and the value was not used; WARP reads the memory behind the buffer and
    // loses the device when there is none (docs/warp-decoder.md, "The D3D12 device is removed").
    [branch] if (do_palette)
      pixels = palette_buf.Load(addr - cb_fb_base_offset);
    else
      pixels = dst_frame.Load(addr);
    if (do_recon) {
      int2 res = residuals.Load2(res_addr + i * res_stride);
      pixels = (clamp((int)((pixels >> 0) & 255) + (int)((res.x << 16) >> 16), 0, 255) << 0) |
               (clamp((int)((pixels >> 8) & 255) + (int)(res.x >> 16), 0, 255) << 8) |
               (clamp((int)((pixels >> 16) & 255) + (int)((res.y << 16) >> 16), 0, 255) << 16) |
               (clamp((int)((pixels >> 24) & 255) + (int)(res.y >> 16), 0, 255) << 24);
    }
    dst_frame.Store(addr, pixels);
  }
}
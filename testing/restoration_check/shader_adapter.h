// SPDX-License-Identifier: BSD-2-Clause

// Included inside each shader namespace, with that shader's generated entry point and buffers.

static void run_thread(int thread, void *context) {
  const auto &group = *static_cast<uint3 *>(context);
  shader_entry(group, uint3(thread % kThreadsX, thread / kThreadsX, 0));
}

static void dispatch(const Case &c, std::vector<uint8_t> &result, uint64_t order) {
  const auto &plane = c.plane;
  const int p = c.subsampling ? 2 : 0;
  plane_id = p;
  do_restoration = c.use_units;
  pl[p].plane = {c.stride, c.src_offset, plane.width, plane.height};
  pl[p].units = {c.rows, c.cols, c.unit_size, c.cols};
  pl[p].pp_offset = c.pre_offset;
  pl[p].dst_offset = c.dst_offset;
  pl[p].Lr_buffer_offset = c.first_unit;
  pl[p].subsampling = c.subsampling;
  pl[p].hbd = c.bit_depth > 8;
  pl[p].bit_depth = c.bit_depth;
  pl[p].unit_shift = __builtin_ctz(c.unit_size);

  for (int i = 0; i < SGRPROJ_PARAMS; ++i) {
    const auto &param = sgr_params[i];
    data.Sgr_Params[i] = int4(param.radius[0], param.radius[1], param.strength[0], param.strength[1]);
    for (int j = 0; j < 4; ++j) {
      uint word = 0;
      for (int byte = 0; byte < 4; ++byte) {
        word |= static_cast<uint>(x_by_xplus1[i * 16 + j * 4 + byte] - 1) << (8 * byte);
      }
      data.Sgr_A2Packed[i][j] = word;
    }
  }

  src = {result.data(), result.size()};
  dst = {result.data(), result.size()};
  LrTypeSgr = {reinterpret_cast<const uint8_t *>(c.types.data()), c.types.size() * sizeof(c.types[0])};
  LrWiener = {reinterpret_cast<const uint8_t *>(c.taps.data()), c.taps.size() * sizeof(c.taps[0])};
  shader_scramble(&order);
  for (int y = 0; y < c.dispatch_height; y += kThreadsY) {
    for (int x = 0; x < c.dispatch_width; x += kThreadsX) {
      uint3 group(x / kThreadsX, y / kThreadsY, 0);
      gs_run_group(kThreadsX * kThreadsY, run_thread, &group, &order);
    }
  }
}

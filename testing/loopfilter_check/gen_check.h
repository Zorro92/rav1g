// SPDX-License-Identifier: BSD-2-Clause
#pragma once

struct ModeGrid {
  uint stride, rows;
  std::vector<uint> grid;
  std::vector<MB_MODE_INFO> pool;

  ModeGrid(int mi_cols, int mi_rows)
      : stride((mi_cols + 15) & ~15), rows((mi_rows + 15) & ~15), grid(stride * rows, 0) {
    static_assert(sizeof(MB_MODE_INFO) == ModeInfoSize);
  }

  void add(int x, int y, int w, int h, MB_MODE_INFO mi) {
    mi.mi_position = uint(x << 16) | y;
    pool.push_back(mi);
    const uint address = pool.size() * ModeInfoSize;
    for (int row = y; row < y + h; ++row) {
      std::fill_n(grid.begin() + row * stride + x, w, address);
    }
  }
};

// Tile grid aligned to 64 pixels. Every mode-info record covers a rectangular prediction
// block. Within the block, transform indices can vary, including across subsampled positions.
ModeGrid random_grid(int mi_cols, int mi_rows, int trial) {
  constexpr int bw_log[] = {0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 0, 2, 1, 3, 2, 4};
  constexpr int bh_log[] = {0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4, 5, 4, 5, 2, 0, 3, 1, 4, 2};
  constexpr int tx_w[] = {1, 2, 4, 8, 16, 1, 2, 2, 4, 4, 8, 8, 16, 1, 4, 2, 8, 4, 16};
  constexpr int tx_h[] = {1, 2, 4, 8, 16, 2, 1, 4, 2, 8, 4, 16, 8, 4, 1, 8, 2, 16, 4};
  ModeGrid grid(mi_cols, mi_rows);
  for (uint tile_y = 0; tile_y < grid.rows; tile_y += 16) {
    for (uint tile_x = 0; tile_x < grid.stride; tile_x += 16) {
      uint bsize;
      do {
        bsize = trial % 8 == 0 ? 0 : rng() % std::size(bw_log);
      } while (bw_log[bsize] > 4 || bh_log[bsize] > 4);
      const int w = 1 << bw_log[bsize], h = 1 << bh_log[bsize];
      for (uint y = tile_y; y < tile_y + 16; y += h) {
        for (uint x = tile_x; x < tile_x + 16; x += w) {
          uint intra_tx;
          do {
            intra_tx = rng() % std::size(tx_w);
          } while (tx_w[intra_tx] > w || tx_h[intra_tx] > h);
          const uint ref = rng() % 8;
          const uint skip = rng() % 2;
          const uint segment = rng() % 8;
          MB_MODE_INFO mi{};
          mi.block_type = bsize | (ref << 16);
          mi.modes = rng() % 25;
          mi.intra_mode_flags = rng() % 2 ? 0x100 : 0;
          mi.tx_info = intra_tx | (skip << 8) | (segment << 24);
          for (uint &word : mi.inter_tx_size) {
            for (int byte = 0; byte < 4; ++byte) {
              uint tx;
              do {
                tx = rng() % std::size(tx_w);
              } while (tx_w[tx] > std::max(1, w / 2) || tx_h[tx] > std::max(1, h / 2));
              word |= tx << (byte * 8);
            }
          }
          mi.delta_lf = rng();
          mi.intra_params = rng();
          grid.add(x, y, w, h, mi);
        }
      }
      // Exercise early stops on both sides of an internal boundary and the terminal block.
      if (trial % 7 == 0 && tile_x + tile_y != 0) {
        grid.grid[tile_y * grid.stride + tile_x + trial % 16] = 0;
      }
    }
  }
  return grid;
}

GenDispatch gen_dispatch(const ModeGrid &mi, int mi_cols, int mi_rows, uint plane, int trial) {
  GenDispatch d{};
  d.pool = {mi.pool.data(), mi.pool.size()};
  d.grid = {mi.grid.data(), mi.grid.size()};
  d.mi_stride = mi.stride;
  d.mi_addr_base = ModeInfoSize;
  d.mi_grid_last = mi.grid.size() - 1;
  d.delta_present = trial % 3 != 0;
  d.delta_multi = trial % 3 == 2;
  d.mode_ref_delta = trial % 2;
  d.filter_level = {int(rng() % 64), int(rng() % 64)};
  d.filter_level_u = rng() % 64;
  d.filter_level_v = rng() % 64;
  for (auto &delta : d.mode_deltas) delta.x = int(rng() % 127) - 63;
  for (auto &delta : d.ref_deltas) delta.x = int(rng() % 127) - 63;
  for (uint seg = 0; seg < 8; ++seg) {
    d.seg_features[seg] = {int(rng() % 8 == 0), int(rng() & 0xff), 0, 0};
  }
  for (auto &seg : d.seg_data) seg.x = int(rng() % 127) - 63;
  for (auto &levels : d.levels) levels = {int(rng() % 64), int(rng() % 64), 0, 0};
  d.plane = plane;
  d.mi_cols = plane ? (mi_cols + 1) / 2 : mi_cols;
  d.mi_rows = plane ? (mi_rows + 1) / 2 : mi_rows;
  return d;
}

// AV1 7.14.2–5 on the fixture's mode-info grid. Look up both sides of every
// transform boundary independently. Pack the result into the shader's edge-list format.
struct ReferenceMi {
  int tx_extent, level;
  bool skip_inter, pred_edge;
};

ReferenceMi reference_mi(const GenDispatch &d, bool horizontal, int col, int row, uint address) {
  const auto &mi = d.pool[(address - d.mi_addr_base) / ModeInfoSize];
  const int bsize = mi.block_type & 255, ref = (mi.block_type >> 16) & 255;
  const int segment = (mi.tx_info >> 24) & 7;
  const bool inter = ref > 0 || (mi.intra_mode_flags & 0x100);
  const bool skipped = (mi.tx_info & 0xff00) != 0;
  const int mode = mi.modes & 255;
  const bool mode_delta = mode == 13 || mode == 14 || (mode >= 16 && mode <= 22) || mode == 24;
  constexpr int blocks[][2] = {
      {1, 1}, {1, 2}, {2, 1}, {2, 2}, {2, 4}, {4, 2}, {4, 4}, {4, 8}, {8, 4}, {8, 8}, {8, 16},
      {16, 8}, {16, 16}, {16, 32}, {32, 16}, {32, 32}, {1, 4}, {4, 1}, {2, 8}, {8, 2}, {4, 16}, {16, 4}};
  const auto &block = blocks[bsize];
  const int axis = horizontal;
  const int sub = d.plane != 0;
  const int coord = (horizontal ? row : col) >> sub;
  const int block_extent = std::max(1, block[axis] >> sub);

  int tx_extent = 1;
  if (!d.seg_features[segment].x) {
    if (sub) {
      tx_extent = std::min(block_extent, 8);
    } else {
      uint tx = mi.tx_info & 255;
      if (inter && !skipped) {
        const int w = std::min(16, block[0]), h = std::min(16, block[1]);
        const int cell_w = std::max(1, w / (w >= h ? 2 : 1));
        const int cell_h = std::max(1, h / (h >= w ? 2 : 1));
        const int idx = (row % block[1] / cell_h) * (block[0] / cell_w) + col % block[0] / cell_w;
        tx = (mi.inter_tx_size[idx / 4] >> (idx % 4 * 8)) & 255;
      }
      constexpr int sizes[][2] = {
          {1, 1}, {2, 2}, {4, 4}, {8, 8}, {16, 16}, {1, 2}, {2, 1}, {2, 4}, {4, 2}, {4, 8},
          {8, 4}, {8, 16}, {16, 8}, {1, 4}, {4, 1}, {2, 8}, {8, 2}, {4, 16}, {16, 4}};
      tx_extent = sizes[tx][axis];
    }
  }

  int level;
  if (d.delta_present) {
    const int levels[] = {d.filter_level.x, d.filter_level.y, d.filter_level_u, d.filter_level_v};
    const int index = sub ? d.plane + 1 : axis;
    const uint delta_word = d.delta_multi ? mi.delta_lf >> (8 * index) : mi.intra_params;
    level = std::clamp(levels[index] + int(int8_t(delta_word & 255)), 0, 63);
    if (d.seg_features[segment].y & (2 << index)) {
      level = std::clamp(level + d.seg_data[segment * 8 + index + 1].x, 0, 63);
    }
    if (d.mode_ref_delta) {
      const int multiplier = level < 32 ? 1 : 2;
      level = std::clamp(level + multiplier * (d.ref_deltas[ref].x + (ref ? d.mode_deltas[mode_delta].x : 0)), 0, 63);
    }
  } else {
    const auto &levels = d.levels[((d.plane * 8 + segment) * 8 + ref) * 2 + mode_delta];
    level = horizontal ? levels.y : levels.x;
  }
  return {tx_extent, level, inter && skipped, coord % block_extent == 0};
}

void reference_lists(const GenDispatch &d, bool horizontal, EdgeLists &lists) {
  const int unit = d.plane ? 2 : 1;
  const int extent = horizontal ? d.mi_rows : d.mi_cols;
  for (uint block = 0; block < lists.blocks(); ++block) {
    std::fill_n(lists.words.begin() + (kListBase + block) * 8, 8, 0);
    const int bx = block % lists.block_cols, by = block / lists.block_cols;
    const int start = (horizontal ? by : bx) * kStrip;
    const auto coords = [&](int along) -> std::pair<int, int> {
      const int x = horizontal ? bx : along, y = horizontal ? along : by;
      return {x * unit + unit - 1, y * unit + unit - 1};
    };
    const auto address_at = [&](int x, int y) { return d.grid.words[std::min(y * d.mi_stride + x, d.mi_grid_last)]; };
    for (int along = start; along < std::min(start + kStrip, extent);) {
      const auto [col, row] = coords(along);
      const uint address = address_at(col, row);
      if (!address) break;
      const auto mi = reference_mi(d, horizontal, col, row, address);
      if (along % mi.tx_extent != 0) break;
      int length = 0, level = 0;
      if (along > 0) {
        const auto [prev_col, prev_row] = coords(along - 1);
        const uint prev_address = address_at(prev_col, prev_row);
        if (!prev_address) break;
        const auto prev = reference_mi(d, horizontal, prev_col, prev_row, prev_address);
        if ((mi.level || prev.level) && (!mi.skip_inter || !prev.skip_inter || mi.pred_edge)) {
          level = mi.level ? mi.level : prev.level;
          const int width = std::min(mi.tx_extent, prev.tx_extent);
          length = width == 1 ? 0 : d.plane ? 1 : width == 2 ? 2 : 3;
        }
      }
      lists.set(block, along - start, mi.tx_extent, length, level);
      along += mi.tx_extent;
    }
  }
}

EdgeLists compare_lists(GenDispatch d, bool horizontal, uint &types) {
  EdgeLists expected(d.mi_cols, d.mi_rows, horizontal);
  std::fill(expected.words.begin(), expected.words.end(), kGuard);
  EdgeLists actual = expected;
  d.work_items = expected.blocks();
  d.dst_offset = kListBase * 32;
  d.dst_stride = expected.block_cols * (horizontal ? 2 : 32);
  const GenKernel &now = horizontal ? gen_lf_blocks_hor : gen_lf_blocks_vert;

  reference_lists(d, horizontal, expected);
  d.edges = {{actual.words.data(), actual.words.size()}, actual.words.data()};
  dispatch(now, d, true);
  if (expected.words != actual.words) {
    const auto mismatch = std::mismatch(expected.words.begin(), expected.words.end(), actual.words.begin());
    std::fprintf(stderr, "FAIL: %s plane %u, %ux%u MI, word %zu: expected %08x, got %08x\n",
                 now.name, d.plane, d.mi_cols, d.mi_rows, size_t(mismatch.first - expected.words.begin()),
                 *mismatch.first, *mismatch.second);
    std::exit(1);
  }
  for (uint i = kListBase * 8; i < (kListBase + actual.blocks()) * 8; ++i) {
    for (uint shift : {0u, 16u}) {
      const uint edge = actual.words[i] >> shift;
      if (edge & 63) types |= 1u << ((edge >> 6) & 3);
    }
  }
  return actual;
}

void check_generators() {
  uint types[2] = {};
  for (int trial = 0; trial < 72; ++trial) {
    const int mi_cols = 17 + rng() % 52;
    const int mi_rows = 17 + rng() % 39;
    const ModeGrid mi = random_grid(mi_cols, mi_rows, trial);
    for (uint plane = 0; plane < 3; ++plane) {
      const GenDispatch d = gen_dispatch(mi, mi_cols, mi_rows, plane, trial);
      const EdgeLists vertical = compare_lists(d, false, types[0]);
      const EdgeLists horizontal = compare_lists(d, true, types[1]);
      if (trial % 6 == 0) {
        for (int depth : {8, 10}) {
          Image image(4 * d.mi_cols - 1, 4 * d.mi_rows - 3, depth);
          const int scale = 1 << (depth - 8);
          for (int y = 0; y < int(4 * d.mi_rows); ++y) {
            for (int x = 0; x < int(4 * d.mi_cols); ++x) {
              image.set(x, y, (64 + (x / 4 + y / 4) % 105) * scale + rng() % (2 * scale));
            }
          }
          const int sharpness = trial / 6 % 8;
          const Image after_vertical = compare_vertical(image, vertical, sharpness, plane, false);
          compare_horizontal(after_vertical, horizontal, sharpness, plane, 0, false);
        }
      }
    }
  }
  if (types[0] != 15 || types[1] != 15) {
    std::fprintf(stderr, "FAIL: list fixtures missed a filter length (%x/%x)\n", types[0], types[1]);
    std::exit(1);
  }
  std::puts("PASS: vertical and horizontal edge lists match the scalar reference on 72 randomized mode grids");
  std::puts("PASS: 72 generated plane cases through both vertical passes and the whole-plane horizontal pass");
}

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

EdgeLists compare_lists(GenDispatch d, bool horizontal, uint &types) {
  EdgeLists expected(d.mi_cols, d.mi_rows, horizontal);
  std::fill(expected.words.begin(), expected.words.end(), kGuard);
  EdgeLists actual = expected;
  d.work_items = expected.blocks();
  d.dst_offset = kListBase * 32;
  d.dst_stride = expected.block_cols * (horizontal ? 2 : 32);
  const GenKernel &old = horizontal ? gen_lf_blocks_hor_legacy : gen_lf_blocks_vert_legacy;
  const GenKernel &now = horizontal ? gen_lf_blocks_hor : gen_lf_blocks_vert;

  d.edges = {{expected.words.data(), expected.words.size()}, expected.words.data()};
  dispatch(old, d, false);
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
      compare_lists(d, false, types[0]);
      compare_lists(d, true, types[1]);
    }
  }
  if (types[0] != 15 || types[1] != 15) {
    std::fprintf(stderr, "FAIL: list fixtures missed a filter length (%x/%x)\n", types[0], types[1]);
    std::exit(1);
  }
  std::puts("PASS: vertical and horizontal edge lists, 72 randomized mode grids across all planes and delta modes");
}

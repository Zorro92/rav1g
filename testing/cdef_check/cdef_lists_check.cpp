/*
 * Compare the GPU CDEF list shader, compiled as C++, with the host's list
 * rules. Check the full buffers, including the untouched skip-row padding.
 */
#define GROUP_SCHED_IMPLEMENTATION
#include <random>
#include <vector>

#include "hlsl_shim.h"

long g_out_of_bounds = 0;

namespace shader {
#include "cdef_lists.gen.h"
}

struct Shape {
  uint cols, rows;  // 8x8 blocks in the frame
  uint mi_stride;   // 4x4 units in a grid row, including padding
  uint index_stride, index_rows;
};

struct Lists {
  std::vector<uint> indices;
  std::vector<uint> skips;
  bool operator==(const Lists &) const = default;
};

static Lists empty_lists(const Shape &s) {
  const uint poison = 0xa5a5a5a5;
  return {std::vector<uint>(s.index_stride * s.index_rows + 1, poison),
          std::vector<uint>(s.index_stride * 16 * s.index_rows * 8 + 1, poison)};
}

static Lists run(const Shape &s, const std::vector<uint> &pool, const std::vector<uint> &grid) {
  Lists lists = empty_lists(s);
  shader::mi_pool = {reinterpret_cast<const uint8_t *>(pool.data()), pool.size() * 4};
  shader::mi_grid = {reinterpret_cast<const uint8_t *>(grid.data()), grid.size() * 4};
  shader::cdef_index = {reinterpret_cast<uint8_t *>(lists.indices.data()), lists.indices.size() * 4};
  shader::skips = {reinterpret_cast<uint8_t *>(lists.skips.data()), lists.skips.size() * 4};
  shader::cb_mi_stride = s.mi_stride;
  shader::cb_mi_addr_base = ModeInfoSize;
  shader::cb_index_stride = s.index_stride;
  shader::cb_skips_stride = s.index_stride * 16;
  shader::cb_skip_cols = s.cols;
  shader::cb_skip_rows = s.rows;
  for (uint y = 0; y < s.index_rows * 8; y++) {
    for (uint x = 0; x < s.index_stride * 8; x++) shader::shader_main(uint3{x, y, 0});
  }
  return lists;
}

static bool check_case(uint cols, uint rows, std::mt19937 &rng, bool sweep) {
  // As in the decoder, the MI stride is rounded up to 32 entries. The
  // last group can be partial in either axis. Only its valid flags are written.
  const Shape s = {cols, rows, (2 * cols + 31) & ~31u, (cols + 7) / 8, (rows + 7) / 8};
  std::vector<uint> grid(s.mi_stride * rows * 2, 0xffffffffu);
  std::vector<uint> pool;
  const auto record = [&](bool skipped) {
    const uint entry = static_cast<uint>(pool.size()) * 4 + ModeInfoSize;
    for (int i = 0; i < ModeInfoSize / 4; i++) pool.push_back(rng());
    uint &tx = pool[(entry - ModeInfoSize + TX_INFO_WORD) / 4];
    // The skip byte is true if any of its bits are set. Other bytes can be
    // arbitrary, including a signed 8-bit index outside the normal range.
    tx = (tx & ~0xff00u) | (skipped ? 1u << (8 + rng() % 8) : 0u);
    return entry;
  };
  record(true);  // The shader maps a zero grid entry to this first record.

  // All equality patterns of four grid entries, from one record to four.
  static const uint patterns[][4] = {{0, 0, 0, 0}, {0, 0, 0, 1}, {0, 0, 1, 0}, {0, 0, 1, 1}, {0, 0, 1, 2},
                                     {0, 1, 0, 0}, {0, 1, 0, 1}, {0, 1, 0, 2}, {0, 1, 1, 0}, {0, 1, 1, 1},
                                     {0, 1, 1, 2}, {0, 1, 2, 0}, {0, 1, 2, 1}, {0, 1, 2, 2}, {0, 1, 2, 3}};
  for (uint y = 0; y < rows; y++) {
    for (uint x = 0; x < cols; x++) {
      const uint block = y * cols + x;
      const uint pattern = sweep ? block % 15 : ((rng() & 1) ? 0 : rng() % 15);
      const uint skip_mask = (sweep ? block / 15 : rng()) & 15;
      const uint *slots = patterns[pattern];
      uint entries[4];
      for (uint i = 0; i < 4; i++) {
        entries[i] = !sweep && rng() % 13 == 0 ? 0 : record((skip_mask >> i) & 1);
      }
      const uint at = (y * s.mi_stride + x) * 2;
      grid[at] = entries[slots[0]];
      grid[at + 1] = entries[slots[1]];
      grid[at + s.mi_stride] = entries[slots[2]];
      grid[at + s.mi_stride + 1] = entries[slots[3]];
    }
  }

  Lists expected = empty_lists(s);
  for (uint y = 0; y < rows; y++) {
    for (uint x = 0; x < cols; x++) {
      const uint at = (y * s.mi_stride + x) * 2;
      const auto word = [&](uint unit) {
        const uint entry = grid[unit];
        const uint offset = entry == 0 ? 0 : entry - ModeInfoSize;
        return pool[(offset + TX_INFO_WORD) / 4];
      };
      const uint tx = word(at);
      const auto skipped = [](uint w) { return (w >> 8) % 256 != 0; };
      expected.skips[y * s.index_stride * 16 + x] =
          skipped(tx) && skipped(word(at + 1)) && skipped(word(at + s.mi_stride)) && skipped(word(at + s.mi_stride + 1));
      if (x % 8 == 0 && y % 8 == 0) {
        const uint strength = (tx >> 16) % 256;
        expected.indices[(y / 8) * s.index_stride + x / 8] = strength < 128 ? strength : strength | 0xffffff00u;
      }
    }
  }
  if (run(s, pool, grid) == expected) return true;
  printf("MISMATCH optimized lists on a %ux%u grid of 8x8 blocks\n", cols, rows);
  return false;
}

int main() {
  std::mt19937 rng(20261119);
  constexpr int count = 128;
  // 16x16 blocks cover every equality pattern and skip mask in one dispatch.
  bool ok = check_case(16, 16, rng, true);
  for (int i = 0; i < count && ok && g_out_of_bounds == 0; i++) {
    const uint cols = 1 + rng() % 37;
    const uint rows = 1 + rng() % 35;
    ok = check_case(cols, rows, rng, false);
  }
  printf("cdef_lists_check: out-of-bounds buffer accesses %ld\n", g_out_of_bounds);
  if (!ok || g_out_of_bounds != 0) return 1;
  printf("cdef_lists_check: %d grids, strength and skip buffers match the CPU rules\n", count + 1);
  return 0;
}

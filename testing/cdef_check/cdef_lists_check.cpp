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

namespace shader_v2 {
#include "cdef_lists_v2.gen.h"
}

static uint64_t g_v2_order = 0x9e3779b97f4a7c15ull;
static uint3 g_v2_group;

static void run_v2_thread(int t, void *) {
  const uint3 local = {static_cast<uint>(t & 7), static_cast<uint>(t >> 3), 0};
  const uint3 dtid = {g_v2_group.x * 8u + local.x, g_v2_group.y * 8u + local.y, 0};
  shader_v2::shader_main(g_v2_group, dtid, local, static_cast<uint>(t));
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
  if (!(run(s, pool, grid) == expected)) {
    printf("MISMATCH optimized lists on a %ux%u grid of 8x8 blocks\n", cols, rows);
    return false;
  }

  // Also verify cdef_lists_v2.hlsl (packed 32-bit active masks per 64x32 group) under adversarial scheduling.
  const uint poison = 0xa5a5a5a5u;
  std::vector<uint> v2_indices(s.index_stride * s.index_rows + 1, poison);
  std::vector<uint> v2_masks(s.index_stride * s.index_rows * 2 + 1, poison);
  shader_v2::mi_pool = {reinterpret_cast<const uint8_t *>(pool.data()), pool.size() * 4};
  shader_v2::mi_grid = {reinterpret_cast<const uint8_t *>(grid.data()), grid.size() * 4};
  shader_v2::cdef_index = {reinterpret_cast<uint8_t *>(v2_indices.data()), v2_indices.size() * 4};
  shader_v2::cdef_masks = {reinterpret_cast<uint8_t *>(v2_masks.data()), v2_masks.size() * 4};
  shader_v2::cb_mi_stride = s.mi_stride;
  shader_v2::cb_mi_addr_base = ModeInfoSize;
  shader_v2::cb_index_stride = s.index_stride;
  shader_v2::cb_skips_stride = s.index_stride * 16;
  shader_v2::cb_skip_cols = s.cols;
  shader_v2::cb_skip_rows = s.rows;
  gs_state.shader = "cdef_lists_v2.hlsl";
  gs_state.track_unwritten = true;
  for (uint gy = 0; gy < s.index_rows; gy++) {
    for (uint gx = 0; gx < s.index_stride; gx++) {
      g_v2_group = uint3{gx, gy, 0};
      uint32_t *p = reinterpret_cast<uint32_t *>(shader_v2::active_bit.bytes());
      for (size_t i = 0; i < shader_v2::active_bit.byte_size() / 4; i++) p[i] = rng();
      gs_run_group(64, run_v2_thread, nullptr, &g_v2_order);
    }
  }
  gs_state.track_unwritten = false;

  std::vector<uint> exp_masks(s.index_stride * s.index_rows * 2 + 1, poison);
  for (uint gy = 0; gy < s.index_rows * 2; gy++) {
    for (uint gx = 0; gx < s.index_stride; gx++) {
      uint mask = 0u;
      for (uint ly = 0; ly < 4; ly++) {
        for (uint bx = 0; bx < 8; bx++) {
          const uint col = gx * 8 + bx;
          const uint row = gy * 4 + ly;
          if (col < cols && row < rows && expected.skips[row * s.index_stride * 16 + col] == 0) {
            mask |= 1u << (ly * 8 + bx);
          }
        }
      }
      exp_masks[gy * s.index_stride + gx] = mask;
    }
  }
  if (v2_indices != expected.indices || v2_masks != exp_masks) {
    printf("MISMATCH cdef_lists_v2 on a %ux%u grid of 8x8 blocks\n", cols, rows);
    return false;
  }
  return true;
}

int main() {
  std::mt19937 rng(20261119);
  constexpr int count = 128;
  // 16x16 blocks cover every equality pattern and skip mask in one dispatch.
  bool ok = check_case(16, 16, rng, true);
  for (int i = 0; i < count && ok && g_out_of_bounds == 0 && gs_state.races == 0; i++) {
    const uint cols = 1 + rng() % 37;
    const uint rows = 1 + rng() % 35;
    ok = check_case(cols, rows, rng, false);
  }
  printf("cdef_lists_check: out-of-bounds buffer accesses %ld, races in group memory: %ld\n", g_out_of_bounds,
         gs_state.races);
  if (!ok || g_out_of_bounds != 0 || gs_state.races != 0 || gs_state.sync_errors != 0 ||
      gs_state.plain_barriers != 0 || gs_state.out_of_bounds != 0 || gs_state.runaway_groups != 0) {
    gs_print_races("  ");
    return 1;
  }
  printf("cdef_lists_check: %d grids, strength, skip and packed mask buffers match the CPU rules\n", count + 1);
  return 0;
}

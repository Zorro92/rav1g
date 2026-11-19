// SPDX-License-Identifier: BSD-2-Clause
#define GROUP_SCHED_IMPLEMENTATION
#include "shader_adapter.h"

#include <array>
#include <random>
#include <vector>

namespace {
constexpr int kPad = 16;
constexpr uint kGuard = 0xecececec;
constexpr uint kListBase = 5;
constexpr int kStrip = 16;
constexpr int kGroup = 64;
std::mt19937 rng(0x810db10c);

struct Image {
  int width, height, depth, bytes, stride;
  std::vector<uint> words;

  Image(int width, int height, int depth)
      : width(width), height(height), depth(depth), bytes(depth == 8 ? 1 : 2),
        stride(((width + 2 * kPad) * bytes + 63) & ~63),
        words((height + 2 * kPad) * stride / 4, kGuard) {}

  void set(int x, int y, uint value) {
    const int address = (y + kPad) * stride + (x + kPad) * bytes;
    const uint shift = (address & 3) * 8;
    const uint mask = (1u << depth) - 1;
    uint &word = words[address / 4];
    word = (word & ~(((1u << (8 * bytes)) - 1) << shift)) | ((value & mask) << shift);
  }

  RWByteAddressBuffer buffer() { return {{words.data(), words.size()}, words.data()}; }
};

struct EdgeLists {
  int block_cols, block_rows;
  std::vector<uint> words;

  EdgeLists(int mi_cols, int mi_rows, bool horizontal)
      : block_cols(horizontal ? mi_cols : (mi_cols + 15) / 16),
        block_rows(horizontal ? (mi_rows + 15) / 16 : mi_rows),
        words((kListBase + block_cols * block_rows + 2) * 8, 0) {}

  uint blocks() const { return block_cols * block_rows; }

  void set(uint block, uint pos, uint step, uint type, uint level) {
    const uint edge = (step << 8) | (type << 6) | level;
    words[(kListBase + block) * 8 + pos / 2] |= edge << (16 * (pos & 1));
  }

  ByteAddressBuffer buffer() const { return {words.data(), words.size()}; }
};

void fail(const char *label, const Image &a, const Image &b) {
  for (size_t i = 0; i < a.words.size(); ++i) {
    if (a.words[i] == b.words[i]) continue;
    const int y = (4 * i / a.stride) - kPad;
    const int x = (4 * i % a.stride) / a.bytes - kPad;
    std::fprintf(stderr, "FAIL: %s (%d-bit) at (%d,%d): expected %08x, got %08x\n",
                 label, a.depth, x, y, a.words[i], b.words[i]);
    std::exit(1);
  }
}

FilterDispatch make_dispatch(Image &image, const EdgeLists &edges, int sharpness, uint plane) {
  FilterDispatch d{};
  d.frame = image.buffer();
  d.edges = edges.buffer();
  d.plane = plane;
  d.planes[plane] = {image.stride, kPad * image.stride + kPad * image.bytes, 0, 0};
  d.list_base = kListBase;
  d.block_cols = edges.block_cols;
  for (int level = 0; level < 64; ++level) {
    int inside = level >> ((sharpness > 0) + (sharpness > 4));
    if (sharpness > 0) inside = std::min(inside, 9 - sharpness);
    inside = std::max(inside, 1);
    d.limits[level] = {inside, 2 * (level + 2) + inside, level >> 4, 0};
  }
  return d;
}

void dispatch(const FilterKernel &kernel, const FilterDispatch &d, bool shuffle) {
  kernel.bind(d);
  gs_state.shader = kernel.name;
  std::vector<uint> groups;
  for (uint base = 0; base < d.work_items; base += kGroup) groups.push_back(base);
  if (shuffle) std::shuffle(groups.begin(), groups.end(), rng);
  uint64_t order = rng();
  for (uint base : groups) gs_run_group(kGroup, kernel.entry, &base, &order);
  if (gs_state.races || gs_state.out_of_bounds || gs_state.runaway_groups ||
      gs_state.sync_errors || gs_state.plain_barriers) {
    gs_print_races("  ");
    std::fprintf(stderr, "FAIL: group scheduling or shared memory in %s\n", kernel.name);
    std::exit(1);
  }
}

void compare_vertical(Image initial, const EdgeLists &edges, int sharpness, uint plane) {
  Image reference = initial;
  Image candidate = initial;
  auto old_dispatch = make_dispatch(reference, edges, sharpness, plane);
  auto new_dispatch = make_dispatch(candidate, edges, sharpness, plane);
  for (uint parity = 0; parity < 2; ++parity) {
    old_dispatch.work_items = new_dispatch.work_items = (edges.blocks() + 1 - parity) / 2 * 4;
    old_dispatch.block_offset = new_dispatch.block_offset = parity;
    dispatch(loopfilter_vert_hbd_legacy, old_dispatch, false);
    dispatch(loopfilter_vert_hbd, new_dispatch, true);
    fail(parity == 0 ? "vertical even" : "vertical odd", reference, candidate);
  }
  if (reference.words == initial.words) {
    std::fprintf(stderr, "FAIL: vertical fixture made no changes\n");
    std::exit(1);
  }
}

// A block row per level and a separate four-pixel row for each kind of neighborhood.
// Put all lengths into each strip and include the first edge at interior block boundaries.
void vertical_thresholds(int depth, int sharpness) {
  Image frame(189, 64 * 4, depth);
  EdgeLists edges((frame.width + 3) / 4, frame.height / 4, false);
  const int scale = 1 << (depth - 8);
  for (int level = 0; level < 64; ++level) {
    for (int block_x = 0; block_x < edges.block_cols; ++block_x) {
      const int block = level * edges.block_cols + block_x;
      const int limit = std::min(kStrip, (frame.width + 3) / 4 - block_x * kStrip);
      if (block_x == 0) {
        edges.set(block, 0, 2, 0, 0);
      } else {
        edges.set(block, 0, 2, 2, level);
      }
      for (int pos = 2; pos < limit; pos += 4) {
        edges.set(block, pos, 4, pos / 4, level);
      }
    }
    for (int row = 0; row < 4; ++row) {
      for (int x = 0; x < (frame.width + 3) / 4 * 4; ++x) {
        const int along = (x + 8) % 16;
        const int base = (level * 29 + x / 16 * 3) % 211 + 20;
        int value = base * scale + (along >= 8 ? 3 * scale : 0);
        if (row == 1 && (along == 3 || along == 13)) value += 2 * scale;
        if (row == 2 && (along == 5 || along == 11)) value += 2 * scale;
        if (row == 3) value += int(rng() % (12 * scale)) - 6 * scale;
        frame.set(x, level * 4 + row, value);
      }
    }
  }
  compare_vertical(frame, edges, sharpness, sharpness % 3);
}

// Mix transform walks, zero levels, holes, tail lanes and partial blocks. Each run also
// has two nearby enabled edges within a strip, including a case with an in-place dependency.
void vertical_random(int depth, int trial) {
  Image frame(129 + rng() % 95, 61 + rng() % 37, depth);
  const int mi_cols = (frame.width + 3) / 4;
  const int mi_rows = (frame.height + 3) / 4;
  EdgeLists edges(mi_cols, mi_rows, false);
  const int scale = 1 << (depth - 8);
  for (int y = 0; y < mi_rows * 4; ++y) {
    for (int x = 0; x < mi_cols * 4; ++x) {
      const int base = 40 + (x / 4 + y / 4 * 11) % 160;
      const uint sample = trial % 4 == 0 ? rng() % (256 * scale) : base * scale + rng() % (3 * scale);
      frame.set(x, y, sample);
    }
  }

  for (int row = 0; row < mi_rows; ++row) {
    for (int bx = 0; bx < edges.block_cols; ++bx) {
      const int limit = std::min(kStrip, mi_cols - bx * kStrip);
      const int stop = trial % 5 == 0 ? rng() % (limit + 1) : limit;
      for (int pos = 0; pos < stop;) {
        int step = 1 << (rng() % 5);
        while (pos & (step - 1)) step >>= 1;
        const uint level = (bx == 0 && pos == 0) || rng() % 4 == 0 ? 0 : 1 + rng() % 63;
        edges.set(row * edges.block_cols + bx, pos, step, rng() % 4, level);
        pos += step;
      }
    }
  }

  // The unfiltered frame remains an effective oracle on the busiest random-noise trials
  // because this strip contains deliberately smooth adjacent edges.
  for (int x = 0; x < 16; ++x) {
    for (int y = 0; y < 4; ++y) frame.set(x, y, (100 + x / 4) * scale);
  }
  std::fill_n(edges.words.begin() + kListBase * 8, 8, 0);
  edges.set(0, 0, 1, 0, 0);
  edges.set(0, 1, 1, 1, 31);
  edges.set(0, 2, 1, 2, 63);
  compare_vertical(frame, edges, trial % 8, trial % 3);
}
}  // namespace

int main() {
  gs_state.track_unwritten = true;
  for (int sharpness = 0; sharpness < 8; ++sharpness) vertical_thresholds(10, sharpness);
  for (int trial = 0; trial < 48; ++trial) vertical_random(10, trial);
  std::puts("PASS: vertical even and odd, 10-bit, all 64 levels, 8 sharpness settings and 4 lengths; 48 randomized frames");
}

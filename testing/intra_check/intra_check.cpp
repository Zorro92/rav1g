/*
 * Host check for the test player flag -intrav2: runs the original intra
 * prediction shaders (intra_main + intra_filter, one dispatch pair per
 * wavefront iteration) and the new one (intra_v2, scheduled by
 * libav1/dx/intra_v2_sched.h) as C++ on generated frames and compares the
 * whole frame buffer byte for byte. 8-bit and 10-bit.
 *
 * What is real: the shader code (converted by run.sh), the scheduler header
 * the decoder uses, the lookup tables (pulled from the decoder sources by
 * run.sh). What is modelled here: the block records (a port of the intra
 * part of gen_pred_blocks.hlsl), the iteration grid of av1_mi_push_block,
 * the dispatch loops of av1_prediction_run_all, and the GPU itself.
 *
 * A thread group is emulated with one fiber per thread:
 *   - a group sync blocks a thread until all threads of the group are there;
 *   - a plain GroupMemoryBarrier() lets all other runnable threads reach their
 *     next barrier first. The original shaders use it where they rely on the
 *     threads of a wave running in lockstep, and that is what this gives;
 *   - the group memory starts as garbage, different in every run.
 * Thread groups run one after the other, so the frame buffer is watched for
 * reads that would depend on the order of groups or threads on a GPU (see
 * AccessTracker in hlsl_shim.h).
 */
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <algorithm>
#include <vector>

#include "hlsl_shim.h"
#include "intra_v2_sched.h"
#include "shader_api.h"
#include "tables.gen.h"

long g_out_of_bounds = 0;
AccessTracker g_tracker;

// ---------------------------------------------------------------------------
// Fibers
// ---------------------------------------------------------------------------

enum { kMaxThreads = 384, kStackSize = 256 * 1024 };
enum { StRun, StBlocked, StDone };
enum { TagWave1 = 1, TagWave2 = 2, TagGroupSync = 3 };

static void fiber_entry();

#if defined(__x86_64__) && !defined(INTRA_CHECK_UCONTEXT)
// Saves the callee-saved registers on the current stack and continues on
// another one. swapcontext() does the same with two system calls per switch,
// which is most of the run time of this check.
extern "C" void intra_check_switch(void **save_sp, void *load_sp);
__asm__(".text\n"
        ".globl intra_check_switch\n"
        ".type intra_check_switch,@function\n"
        "intra_check_switch:\n"
        "  pushq %rbp\n"
        "  pushq %rbx\n"
        "  pushq %r12\n"
        "  pushq %r13\n"
        "  pushq %r14\n"
        "  pushq %r15\n"
        "  movq %rsp, (%rdi)\n"
        "  movq %rsi, %rsp\n"
        "  popq %r15\n"
        "  popq %r14\n"
        "  popq %r13\n"
        "  popq %r12\n"
        "  popq %rbx\n"
        "  popq %rbp\n"
        "  ret\n"
        ".size intra_check_switch,.-intra_check_switch\n");

struct Fiber {
  void *sp;
  int state;
  int tag;
};
static Fiber g_fibers[kMaxThreads];
static char *g_stacks;
static void *g_scheduler_sp;
static int g_current;

static void fiber_prepare(int t) {
  uintptr_t *top = (uintptr_t *)(((uintptr_t)(g_stacks + (size_t)(t + 1) * kStackSize)) & ~(uintptr_t)15);
  *--top = 0;                        // keeps the stack of fiber_entry() aligned
  *--top = (uintptr_t)&fiber_entry;  // where the first switch returns to
  for (int i = 0; i < 6; ++i) *--top = 0;
  g_fibers[t].sp = top;
  g_fibers[t].state = StRun;
  g_fibers[t].tag = 0;
}
static void fiber_resume(int t) {
  g_current = t;
  intra_check_switch(&g_scheduler_sp, g_fibers[t].sp);
}
static void fiber_suspend() { intra_check_switch(&g_fibers[g_current].sp, g_scheduler_sp); }
#else
#include <ucontext.h>
struct Fiber {
  ucontext_t context;
  int state;
  int tag;
};
static Fiber g_fibers[kMaxThreads];
static char *g_stacks;
static ucontext_t g_scheduler_context;
static int g_current;

static void fiber_prepare(int t) {
  getcontext(&g_fibers[t].context);
  g_fibers[t].context.uc_stack.ss_sp = g_stacks + (size_t)t * kStackSize;
  g_fibers[t].context.uc_stack.ss_size = kStackSize;
  g_fibers[t].context.uc_link = nullptr;
  makecontext(&g_fibers[t].context, fiber_entry, 0);
  g_fibers[t].state = StRun;
  g_fibers[t].tag = 0;
}
static void fiber_resume(int t) {
  g_current = t;
  swapcontext(&g_scheduler_context, &g_fibers[t].context);
}
static void fiber_suspend() { swapcontext(&g_fibers[g_current].context, &g_scheduler_context); }
#endif

static const ShaderApi *g_api;
static uint32_t g_group;
static long g_sync_errors;

__attribute__((noinline, used)) static void fiber_entry() {
  g_api->run(g_group, (uint32_t)g_current);
  g_fibers[g_current].state = StDone;
  fiber_suspend();
  abort();
}

static void fiber_wait(int state, int tag) {
  g_fibers[g_current].state = state;
  g_fibers[g_current].tag = tag;
  fiber_suspend();
}

void GroupMemoryBarrier() { fiber_wait(StRun, 0); }
void WaveJoin(int level) { fiber_wait(StBlocked, level); }
void GroupMemoryBarrierWithGroupSync() { fiber_wait(StBlocked, TagGroupSync); }
void AllMemoryBarrierWithGroupSync() { fiber_wait(StBlocked, TagGroupSync); }

// Runs threads 0 .. threads - 1 of one thread group to the end.
static void run_group(const ShaderApi *api, uint32_t group, int threads, uint64_t *garbage) {
  api->scramble(garbage);
  g_api = api;
  g_group = group;
  g_tracker.group++;
  g_tracker.phase++;
  for (int t = 0; t < threads; ++t) fiber_prepare(t);
  for (;;) {
    int ran = 0;
    for (int t = 0; t < threads; ++t) {
      if (g_fibers[t].state != StRun) continue;
      g_tracker.thread = (uint32_t)t + 1;
      fiber_resume(t);
      ran = 1;
    }
    if (ran) continue;
    // Nobody can run: release the threads waiting at the innermost point.
    int tag = INT_MAX;
    for (int t = 0; t < threads; ++t) {
      if (g_fibers[t].state == StBlocked) tag = std::min(tag, g_fibers[t].tag);
    }
    if (tag == INT_MAX) break;
    for (int t = 0; t < threads; ++t) {
      if (g_fibers[t].state == StBlocked && g_fibers[t].tag == tag) {
        g_fibers[t].state = StRun;
      } else if (tag == TagGroupSync) {
        g_sync_errors++;  // a group sync that not all threads of the group reach
      }
    }
    if (tag == TagGroupSync) g_tracker.phase++;
  }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

struct Rng {
  uint64_t state;
  uint32_t u32() {
    state += 0x9e3779b97f4a7c15ull;
    uint64_t z = state;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
    return (uint32_t)((z ^ (z >> 31)) >> 16);
  }
  int below(int n) { return (int)(u32() % (uint32_t)n); }
  int chance(int percent) { return below(100) < percent; }
};

enum {
  DC_PRED,
  V_PRED,
  H_PRED,
  D45_PRED,
  D135_PRED,
  D113_PRED,
  D157_PRED,
  D203_PRED,
  D67_PRED,
  SMOOTH_PRED,
  SMOOTH_V_PRED,
  SMOOTH_H_PRED,
  PAETH_PRED,
  UV_CFL_PRED,
};

enum { IntraTypeCount = 10 };  // block types per iteration: 9 sizes and filter intra

struct Block {
  uint32_t x, y, y2, z;  // the record; y2 is the second word of the V record of a chroma block
  int is_uv;
  int bucket;  // block type: 8 - log2(width / 4) - log2(height / 4), or 9 for filter intra
  int tile;
  int iter;  // wavefront iteration of the original path
};

struct Tile {
  int c0, c1, r0, r1;  // in 4x4 units
};

struct Coverage {
  long frames, blocks, luma_mode[16], chroma_mode[16], sizes[10];
  long inter_intra, top_right, bottom_left, upsample, edge_filter, corner_filter, residual, clipped, multi_tile;
  long v1_iterations, v1_dispatches, v1_groups;
  long v2_steps, v2_dispatches, v2_groups, v2_group_passes, v2_max_passes;
};

// The sizes of everything that depends on the frame size only.
struct Geometry {
  int hbd;
  int mi_cols, mi_rows;    // frame size in 4x4 units, even
  int pad_cols, pad_rows;  // padded to 128 pixels
  int sb_log;              // log2 of the superblock size in 4x4 units: 4 or 5
  std::vector<Tile> tiles;
  int stride[3], offset[3], height[3];
  int res_stride[3], res_offset[3];
  size_t pool_size, res_size;
  int cells_y, cells_uv, regions;
};

struct Frame {
  IntraConsts consts;
  std::vector<uint8_t> pixels;  // the frame buffer before intra prediction
  std::vector<uint8_t> residuals;
  std::vector<uint8_t> wedge;
  std::vector<Block> blocks;       // in decode order
  std::vector<uint32_t> records1;  // records in the order of the original path
  std::vector<uint32_t> counts1;   // records per (iteration, type)
  int iterations;
  std::vector<uint32_t> records2;    // records in the order of the new path
  std::vector<uint32_t> block_map2;  // first record per (tile, type)
  int edge_filter;
};

enum { kWedgeEntries = 64 };

class Generator {
 public:
  Generator(const Geometry &geo, Rng *rng, IntraV2Sched *sched, Coverage *cov, Frame *frame)
      : geo_(geo), rng_(*rng), sched_(sched), cov_(cov), frame_(frame) {
    stride_y_ = geo.pad_cols + 2;
    stride_uv_ = (geo.pad_cols >> 1) + 2;
    grid_y_.assign((size_t)stride_y_ * (geo.pad_rows + 2), -1);
    grid_uv_.assign((size_t)stride_uv_ * ((geo.pad_rows >> 1) + 2), -1);
    done_y_.assign((size_t)geo.pad_cols * geo.pad_rows, 0);
    done_uv_.assign((size_t)(geo.pad_cols >> 1) * (geo.pad_rows >> 1), 0);
  }

  void run(int p_intra, int p_inter_intra, int p_split) {
    p_intra_ = p_intra;
    p_inter_intra_ = p_inter_intra;
    p_split_ = p_split;
    for (size_t t = 0; t < geo_.tiles.size(); ++t) {
      tile_ = geo_.tiles[t];
      tile_index_ = (int)t;
      memset(counters_, 0, sizeof(counters_));
      const int sb = 1 << geo_.sb_log;
      for (int row = tile_.r0; row < tile_.r1; row += sb) {
        for (int col = tile_.c0; col < tile_.c1; col += sb) partition(row, col, geo_.sb_log);
      }
    }
  }

 private:
  const Geometry &geo_;
  Rng &rng_;
  IntraV2Sched *sched_;
  Coverage *cov_;
  Frame *frame_;
  int p_intra_ = 100, p_inter_intra_ = 0, p_split_ = 50;
  Tile tile_ = {};
  int tile_index_ = 0;
  uint32_t counters_[IntraTypeCount] = {};
  int stride_y_, stride_uv_;
  std::vector<int> grid_y_, grid_uv_;       // the iteration grids of the original path
  std::vector<uint8_t> done_y_, done_uv_;  // cells decoded so far

  int done(int is_uv, int col, int row) const {
    return is_uv ? done_uv_[(size_t)col + (size_t)row * (geo_.pad_cols >> 1)]
                 : done_y_[(size_t)col + (size_t)row * geo_.pad_cols];
  }
  void mark(int is_uv, int col, int row, int w, int h) {
    const int cols = geo_.pad_cols >> is_uv;
    const int rows = geo_.pad_rows >> is_uv;
    std::vector<uint8_t> &map = is_uv ? done_uv_ : done_y_;
    for (int y = row; y < std::min(row + h, rows); ++y) {
      for (int x = col; x < std::min(col + w, cols); ++x) map[(size_t)x + (size_t)y * cols] = 1;
    }
  }

  // One transform block: its iteration in the original path (as
  // av1_mi_push_block finds it), its place in the new schedule, its record.
  void push(int is_uv, int col, int row, int txw, int txh, int above_left, int above_count, int left_count, int cfl,
            int bucket, uint32_t y, uint32_t y2, uint32_t z) {
    const int w = 1 << txw;
    const int h = 1 << txh;
    const int stride = is_uv ? stride_uv_ : stride_y_;
    int *blk = (is_uv ? grid_uv_.data() : grid_y_.data()) + col + row * stride;
    int iter = -1;
    for (int it = 1 - above_left; it <= above_count; ++it) iter = std::max(iter, blk[it]);
    for (int it = 1; it <= left_count; ++it) iter = std::max(iter, blk[it * stride]);
    if (cfl) {
      const int *y_grid = grid_y_.data() + (col << 1) + (row << 1) * stride_y_;
      for (int yit = 1; yit <= (h << 1); ++yit) {
        for (int xit = 1; xit <= (w << 1); ++xit) iter = std::max(iter, y_grid[xit + yit * stride_y_]);
      }
    }
    ++iter;
    for (int it = 1; it <= h; ++it) blk[it * stride + w] = iter;
    for (int it = 1; it <= w; ++it) blk[it + h * stride] = iter;

    intra_v2_place(sched_, (uint32_t)tile_index_ * IntraTypeCount, is_uv, col, row, txw, txh, above_left, above_count,
                   left_count, cfl, (uint32_t)bucket, counters_[bucket]);
    counters_[bucket] += 1 + is_uv;

    Block b;
    b.x = (uint32_t)col | ((uint32_t)row << 16);
    b.y = y;
    b.y2 = y2;
    b.z = z;
    b.is_uv = is_uv;
    b.bucket = bucket;
    b.tile = tile_index_;
    b.iter = iter;
    frame_->blocks.push_back(b);
    mark(is_uv, col, row, w, h);

    cov_->blocks++;
    cov_->sizes[bucket]++;
    (is_uv ? cov_->chroma_mode : cov_->luma_mode)[(y >> 6) & 15]++;
    if ((int)y < 0) cov_->inter_intra++;
    if (y & (1 << 5)) cov_->residual++;
    if (((y >> 6) & 15) < 14) {
      if (y & (3 << 22)) cov_->upsample++;
      if (y & (15 << 24)) cov_->edge_filter++;
    }
  }

  // The edge bits of a directional mode, as gen_pred_blocks.hlsl sets them.
  uint32_t direction_flags(int mode, int delta, int txw, int txh, int *filter_above, int *filter_left) {
    static const int mode_to_angle_map[] = { 0, 90, 180, 45, 135, 113, 157, 203, 67 };
    int upsample_above = 0;
    int upsample_left = 0;
    const int angle = mode_to_angle_map[mode] + delta * 3;
    if (frame_->edge_filter) {
      const int filt_type = rng_.below(2);  // whether a neighbour block is smooth
      const int d90 = abs(angle - 90);
      const int d180 = abs(angle - 180);
      const int blk_wh = (4 << txw) + (4 << txh);
      upsample_above = d90 != 0 && d90 < 40 && blk_wh <= (16 >> filt_type);
      upsample_left = d180 != 0 && d180 < 40 && blk_wh <= (16 >> filt_type);
      *filter_above = intra_edge_filter_strength(blk_wh, d90, filt_type);
      *filter_left = intra_edge_filter_strength(blk_wh, d180, filt_type);
    }
    return ((uint32_t)upsample_above << 22) | ((uint32_t)upsample_left << 23) | ((uint32_t)(delta + 3) << 28);
  }

  void luma(int mi_row, int mi_col, int bwl, int bhl, int txw, int txh, int mode, int delta, int use_filter,
            int inter_intra, int nonskip) {
    int need_above = (NeedAboveLut >> mode) & 1;
    int need_left = (NeedLeftLut >> mode) & 1;
    const int need_right = (NeedRightLut >> mode) & 1;
    const int need_bot = (NeedBotLut >> mode) & 1;
    const int need_aboveleft = (NeedAboveLeftLut >> mode) & 1;
    const int max_cnt_x = (geo_.mi_cols - mi_col + (1 << txw) - 1) >> txw;
    const int max_cnt_y = (geo_.mi_rows - mi_row + (1 << txh) - 1) >> txh;
    const int unit_x_log = bwl == 5;
    const int unit_y_log = bhl == 5;
    int cnt_y = 1 << (bhl - txh);
    int cnt_x = 1 << (bwl - txw);
    if (cnt_x > max_cnt_x || cnt_y > max_cnt_y) cov_->clipped++;

    const int mode_gpu = use_filter ? 13 : mode ? (mode - 1) : 14;
    uint32_t flags = (uint32_t)txw | ((uint32_t)nonskip << 5) | ((uint32_t)mode_gpu << 6);
    int filter_above = 0;
    int filter_left = 0;
    if (mode >= V_PRED && mode <= D67_PRED) flags |= direction_flags(mode, delta, txw, txh, &filter_above, &filter_left);
    flags |= (uint32_t)inter_intra << 31;
    uint32_t z = 0;
    if (inter_intra)
      z = (uint32_t)rng_.below(kWedgeEntries);
    else if (use_filter)
      z = (uint32_t)txh | ((uint32_t)rng_.below(5) << 4);
    const int bucket = use_filter ? 9 : (8 - txw - txh);

    cnt_x >>= unit_x_log;
    cnt_y >>= unit_y_log;
    for (int unit_y = 0; unit_y <= unit_y_log; ++unit_y) {
      for (int unit_x = 0; unit_x <= unit_x_log; ++unit_x) {
        const int x_start = unit_x * cnt_x;
        const int x_end = std::min(x_start + cnt_x, max_cnt_x);
        const int y_start = unit_y * cnt_y;
        const int y_end = std::min(y_start + cnt_y, max_cnt_y);
        for (int y = y_start; y < y_end; ++y) {
          for (int x = x_start; x < x_end; ++x) {
            const int col = mi_col + (x << txw);
            const int row = mi_row + (y << txh);
            const int subblk_w = 1 << txw;
            const int subblk_h = 1 << txh;
            const int have_top = y || (mi_row > tile_.r0);
            const int have_left = x || (mi_col > tile_.c0);
            int above_available = have_top;
            if (need_above) {
              const int xr = geo_.mi_cols - col - subblk_w;
              int have_top_right = 0;
              if (need_right && have_top && col + subblk_w < tile_.c1) {
                have_top_right = 1;
                for (int i = 0; i < std::min(subblk_w, xr); ++i) have_top_right &= done(0, col + subblk_w + i, row - 1);
                cov_->top_right += have_top_right;
              }
              above_available =
                  (have_top ? std::min(subblk_w, subblk_w + xr) : 0) + (have_top_right ? std::min(subblk_w, xr) : 0);
            }
            int left_available = have_left;
            if (need_left) {
              const int yd = geo_.mi_rows - row - subblk_h;
              int have_bottom_left = 0;
              if (need_bot && have_left && yd > 0 && row + subblk_h < tile_.r1) {
                have_bottom_left = 1;
                for (int i = 0; i < std::min(subblk_h, yd); ++i)
                  have_bottom_left &= done(0, col - 1, row + subblk_h + i);
                cov_->bottom_left += have_bottom_left;
              }
              left_available =
                  (have_left ? std::min(subblk_h, subblk_h + yd) : 0) + (have_bottom_left ? std::min(subblk_h, yd) : 0);
            }
            const uint32_t word = flags | ((uint32_t)above_available << 10) | ((uint32_t)left_available << 16) |
                                  (above_available ? ((uint32_t)filter_above << 24) : 0) |
                                  (left_available ? ((uint32_t)filter_left << 26) : 0);
            push(0, col, row, txw, txh, have_top && have_left && need_aboveleft, above_available, left_available, 0,
                 bucket, word, 0, z);
          }
        }
      }
    }
  }

  void chroma(int mi_row, int mi_col, int bwl, int bhl, int mode, int delta, int inter_intra, int nonskip,
              uint32_t cfl_limits) {
    const int bw = 1 << bwl;
    const int bh = 1 << bhl;
    const int bwl_uv = std::max(0, bwl - 1);
    const int bhl_uv = std::max(0, bhl - 1);
    const int mi_col_uv = mi_col >> 1;
    const int mi_row_uv = mi_row >> 1;
    const int mi_col1 = mi_col & ~1;
    const int mi_row1 = mi_row & ~1;
    const int need_above = (NeedAboveLut >> mode) & 1;
    const int need_left = (NeedLeftLut >> mode) & 1;
    const int need_right = (NeedRightLut >> mode) & 1;
    const int need_bot = (NeedBotLut >> mode) & 1;
    const int need_aboveleft = (NeedAboveLeftLut >> mode) & 1;
    const int txw = std::min(bwl_uv, 3);
    const int txh = std::min(bhl_uv, 3);
    const int unit_x_log = bwl == 5;
    const int unit_y_log = bhl == 5;
    const int cnt_y = 1 << (bhl_uv - txh - unit_y_log);
    const int cnt_x = 1 << (bwl_uv - txw - unit_x_log);

    const int mode_gpu = mode == UV_CFL_PRED ? 15 : mode ? (mode - 1) : 14;
    uint32_t flags = (uint32_t)txw | ((uint32_t)nonskip << 5) | ((uint32_t)mode_gpu << 6);
    int filter_above = 0;
    int filter_left = 0;
    if (mode >= V_PRED && mode <= D67_PRED) flags |= direction_flags(mode, delta, txw, txh, &filter_above, &filter_left);
    flags |= (uint32_t)inter_intra << 31;
    uint32_t mode_u = 1 << 3;
    uint32_t mode_v = 2 << 3;
    uint32_t z = 0;
    if (mode == UV_CFL_PRED) {
      mode_u |= (uint32_t)(rng_.below(33)) << 22;  // alpha index + 16
      mode_v |= (uint32_t)(rng_.below(33)) << 22;
      z = cfl_limits;
    }
    if (inter_intra) z = (uint32_t)rng_.below(kWedgeEntries);
    const int bucket = 8 - txw - txh;

    for (int unit_y = 0; unit_y <= unit_y_log; ++unit_y) {
      for (int unit_x = 0; unit_x <= unit_x_log; ++unit_x) {
        for (int suby = 0; suby < cnt_y; ++suby) {
          for (int subx = 0; subx < cnt_x; ++subx) {
            const int x = subx + unit_x * cnt_x;
            const int y = suby + unit_y * cnt_y;
            const int col = mi_col_uv + (x << txw);
            const int row = mi_row_uv + (y << txh);
            const int subblk_w = 1 << txw;
            const int subblk_h = 1 << txh;
            const int have_top = y || (mi_row1 > tile_.r0);
            const int have_left = x || (mi_col1 > tile_.c0);
            int above_available = have_top;
            if (need_above) {
              const int xr = ((geo_.mi_cols - mi_col - bw) + (2 << bwl_uv) - ((x + 1) << (txw + 1))) >> 1;
              int have_top_right = 0;
              if (need_right && have_top && xr > 0 && mi_col + (((x << txw) + subblk_w) << 1) < tile_.c1) {
                have_top_right = 1;
                for (int i = 0; i < std::min(subblk_w, xr); ++i) have_top_right &= done(1, col + subblk_w + i, row - 1);
                cov_->top_right += have_top_right;
              }
              above_available =
                  (have_top ? std::min(subblk_w, subblk_w + xr) : 0) + (have_top_right ? std::min(subblk_w, xr) : 0);
            }
            int left_available = have_left;
            if (need_left) {
              const int yd = ((geo_.mi_rows - mi_row - bh) + (2 << bhl_uv) - ((y + 1) << (txh + 1))) >> 1;
              int have_bottom_left = 0;
              if (need_bot && have_left && yd > 0 && mi_row + (((y << txh) + subblk_h) << 1) < tile_.r1) {
                have_bottom_left = 1;
                for (int i = 0; i < std::min(subblk_h, yd); ++i)
                  have_bottom_left &= done(1, col - 1, row + subblk_h + i);
                cov_->bottom_left += have_bottom_left;
              }
              left_available =
                  (have_left ? std::min(subblk_h, subblk_h + yd) : 0) + (have_bottom_left ? std::min(subblk_h, yd) : 0);
            }
            const uint32_t word = flags | ((uint32_t)above_available << 10) | ((uint32_t)left_available << 16) |
                                  (above_available ? ((uint32_t)filter_above << 24) : 0) |
                                  (left_available ? ((uint32_t)filter_left << 26) : 0);
            push(1, col, row, txw, txh, need_aboveleft && have_top && have_left, above_available, left_available,
                 mode == UV_CFL_PRED, bucket, word | mode_u, word | mode_v, z);
          }
        }
      }
    }
  }

  // One coding block of 4 << bwl by 4 << bhl pixels.
  void block(int mi_row, int mi_col, int bwl, int bhl) {
    static const int interintra_to_intra_mode[4] = { DC_PRED, V_PRED, H_PRED, SMOOTH_PRED };
    const int bw = 1 << bwl;
    const int bh = 1 << bhl;
    const int is_intra = rng_.chance(p_intra_);
    const int inter_intra =
        !is_intra && bwl >= 1 && bwl <= 3 && bhl >= 1 && bhl <= 3 && abs(bwl - bhl) <= 1 && rng_.chance(p_inter_intra_);
    const int interintra_mode = rng_.below(4);
    const int nonskip = rng_.chance(70);
    const int palette_allowed = is_intra && bwl >= 1 && bhl >= 1 && bwl <= 4 && bhl <= 4;
    const int y_use_palette = palette_allowed && rng_.chance(4);
    const int uv_use_palette = palette_allowed && rng_.chance(4);
    const int angles = bwl >= 1 && bhl >= 1;  // angle deltas from 8x8

    // Luma transform size: the largest of the block, split up to twice.
    int txw = std::min(bwl, 4);
    int txh = std::min(bhl, 4);
    if (!inter_intra) {
      for (int depth = rng_.below(3); depth > 0; --depth) {
        if (txw > txh) {
          --txw;
        } else if (txh > txw) {
          --txh;
        } else if (txw > 0) {
          --txw;
          --txh;
        }
      }
    }
    const int max_cnt_x = (geo_.mi_cols - mi_col + (1 << txw) - 1) >> txw;
    const int max_cnt_y = (geo_.mi_rows - mi_row + (1 << txh) - 1) >> txh;
    const uint32_t cfl_max_x = (uint32_t)(mi_col + (std::min(1 << (bwl - txw), max_cnt_x) << txw)) << 2;
    const uint32_t cfl_max_y = (uint32_t)(mi_row + (std::min(1 << (bhl - txh), max_cnt_y) << txh)) << 2;

    if ((is_intra || inter_intra) && !y_use_palette) {
      const int mode = inter_intra ? interintra_to_intra_mode[interintra_mode] : rng_.below(13);
      const int use_filter = is_intra && mode == DC_PRED && bwl <= 3 && bhl <= 3 && rng_.chance(35);
      const int delta = (is_intra && angles) ? rng_.below(7) - 3 : 0;
      luma(mi_row, mi_col, bwl, bhl, txw, txh, mode, delta, use_filter, inter_intra, nonskip);
    }
    mark(0, mi_col, mi_row, bw, bh);

    // A block narrower or lower than 8 pixels carries the chroma of its 8x8
    // only when it is the last one there.
    const int no_chroma = ((mi_row & 1) == 0 && (bh & 1) == 1) || ((mi_col & 1) == 0 && (bw & 1) == 1);
    if (no_chroma) return;
    if ((is_intra || inter_intra) && !uv_use_palette) {
      const int cfl_allowed = is_intra && bwl <= 3 && bhl <= 3;
      int mode = inter_intra ? interintra_to_intra_mode[interintra_mode] : rng_.below(13);
      if (cfl_allowed && rng_.chance(25)) mode = UV_CFL_PRED;
      const int delta = (is_intra && angles) ? rng_.below(7) - 3 : 0;
      chroma(mi_row, mi_col, bwl, bhl, mode, delta, inter_intra, nonskip, cfl_max_x | (cfl_max_y << 16));
    }
    mark(1, mi_col >> 1, mi_row >> 1, 1 << std::max(0, bwl - 1), 1 << std::max(0, bhl - 1));
  }

  // The partition tree of AV1, with its forced splits at the frame edges.
  void partition(int mi_row, int mi_col, int bsl) {
    if (mi_row >= geo_.mi_rows || mi_col >= geo_.mi_cols) return;
    enum { None, Horz, Vert, Split, Horz4, Vert4 };
    const int hbs = (1 << bsl) >> 1;
    const int qbs = (1 << bsl) >> 2;
    const int has_rows = mi_row + hbs < geo_.mi_rows;
    const int has_cols = mi_col + hbs < geo_.mi_cols;
    int type = Split;
    if (has_rows && has_cols) {
      if (!rng_.chance(std::min(95, p_split_ + 8 * (bsl - 1)))) {
        const int r = rng_.below(10);
        type = r < 5 ? None : r < 7 ? Horz : r < 9 ? Vert : (bsl >= 2 && bsl <= 4) ? (rng_.below(2) ? Horz4 : Vert4) : None;
      }
    } else if (has_cols) {
      type = rng_.below(2) ? Horz : Split;
    } else if (has_rows) {
      type = rng_.below(2) ? Vert : Split;
    }
    switch (type) {
      case None: block(mi_row, mi_col, bsl, bsl); break;
      case Horz:
        block(mi_row, mi_col, bsl, bsl - 1);
        if (has_rows) block(mi_row + hbs, mi_col, bsl, bsl - 1);
        break;
      case Vert:
        block(mi_row, mi_col, bsl - 1, bsl);
        if (has_cols) block(mi_row, mi_col + hbs, bsl - 1, bsl);
        break;
      case Horz4:
        for (int i = 0; i < 4; ++i) {
          if (i > 0 && mi_row + i * qbs >= geo_.mi_rows) break;
          block(mi_row + i * qbs, mi_col, bsl, bsl - 2);
        }
        break;
      case Vert4:
        for (int i = 0; i < 4; ++i) {
          if (i > 0 && mi_col + i * qbs >= geo_.mi_cols) break;
          block(mi_row, mi_col + i * qbs, bsl - 2, bsl);
        }
        break;
      default:
        if (bsl == 1) {
          block(mi_row, mi_col, 0, 0);
          block(mi_row, mi_col + 1, 0, 0);
          block(mi_row + 1, mi_col, 0, 0);
          block(mi_row + 1, mi_col + 1, 0, 0);
        } else {
          partition(mi_row, mi_col, bsl - 1);
          partition(mi_row, mi_col + hbs, bsl - 1);
          partition(mi_row + hbs, mi_col, bsl - 1);
          partition(mi_row + hbs, mi_col + hbs, bsl - 1);
        }
        break;
    }
  }
};

static Geometry make_geometry(Rng *rng, int hbd) {
  Geometry g;
  g.hbd = hbd;
  g.sb_log = 4 + rng->below(2);
  const int max_cells = rng->chance(25) ? 96 : 48;
  g.mi_cols = 2 * (1 + rng->below(max_cells / 2));
  g.mi_rows = 2 * (1 + rng->below(max_cells / 2));
  g.pad_cols = (g.mi_cols + 31) & ~31;
  g.pad_rows = (g.mi_rows + 31) & ~31;

  // Up to 2 x 2 tiles, cut at superblock boundaries.
  const int sb = 1 << g.sb_log;
  int col_cut = 0;
  int row_cut = 0;
  if (g.mi_cols > sb && rng->chance(50)) col_cut = sb * (1 + rng->below((g.mi_cols - 1) / sb));
  if (g.mi_rows > sb && rng->chance(50)) row_cut = sb * (1 + rng->below((g.mi_rows - 1) / sb));
  for (int r = 0; r <= (row_cut != 0); ++r) {
    for (int c = 0; c <= (col_cut != 0); ++c) {
      Tile t;
      t.c0 = c ? col_cut : 0;
      t.c1 = (col_cut && !c) ? col_cut : g.mi_cols;
      t.r0 = r ? row_cut : 0;
      t.r1 = (row_cut && !r) ? row_cut : g.mi_rows;
      g.tiles.push_back(t);
    }
  }

  // Planes with some room around them. 8-bit: one byte per pixel, 10-bit: two.
  const int bytes = 1 + hbd;
  size_t at = 256;
  size_t res_at = 256;
  for (int p = 0; p < 3; ++p) {
    const int w = (g.pad_cols * 4) >> (p > 0);
    const int h = (g.pad_rows * 4) >> (p > 0);
    g.stride[p] = w * bytes;
    g.height[p] = h;
    g.offset[p] = (int)at;
    at += (size_t)g.stride[p] * h + 256;
    g.res_stride[p] = w * 2;
    g.res_offset[p] = (int)res_at;
    res_at += (size_t)g.res_stride[p] * h + 256;
  }
  g.pool_size = at;
  g.res_size = res_at;
  g.cells_y = g.pad_cols * g.pad_rows;
  g.cells_uv = (g.pad_cols >> 1) * (g.pad_rows >> 1);
  g.regions = (g.pad_cols >> 4) * (g.pad_rows >> 4);
  return g;
}

static void make_frame_data(const Geometry &g, Rng *rng, Frame *f) {
  const int pixel_max = g.hbd ? 1023 : 255;
  memset(&f->consts, 0, sizeof(f->consts));
  f->edge_filter = rng->chance(80);
  f->consts.flags[0] = f->edge_filter;
  for (int p = 0; p < 3; ++p) {
    f->consts.planes[p][0] = g.stride[p];
    f->consts.planes[p][1] = g.offset[p];
    f->consts.planes[p][2] = g.res_stride[p];
    f->consts.planes[p][3] = g.res_offset[p];
  }
  for (int m = 0; m < 5; ++m) {
    for (int k = 0; k < 8; ++k) {
      for (int i = 0; i < 8; ++i) f->consts.filter[m][k][i] = av1_filter_intra_taps[m][k][i];
    }
  }
  static_assert(sizeof(f->consts.mode_params_lut) == sizeof(intra_mode_shader_params), "mode table");
  memcpy(f->consts.mode_params_lut, intra_mode_shader_params, sizeof(intra_mode_shader_params));
  for (int i = 0; i < 128; ++i) f->consts.sm_weight_arrays[i][0] = sm_weight_arrays[i];

  // Flat areas, gradients and noise: all give different edge cases.
  f->pixels.assign(g.pool_size, 0);
  f->residuals.assign(g.res_size, 0);
  const int flat = rng->chance(15);
  const int base = rng->below(pixel_max + 1);
  const int extreme = rng->chance(20);
  for (int p = 0; p < 3; ++p) {
    for (int y = 0; y < g.height[p]; ++y) {
      for (int x = 0; x < (g.stride[p] >> g.hbd); ++x) {
        int v = flat ? base + rng->below(5) - 2 : rng->below(pixel_max + 1);
        if (extreme && rng->chance(30)) v = rng->below(2) ? 0 : pixel_max;
        v = std::min(std::max(v, 0), pixel_max);
        const size_t at = (size_t)g.offset[p] + (size_t)y * g.stride[p] + ((size_t)x << g.hbd);
        f->pixels[at] = (uint8_t)v;
        if (g.hbd) f->pixels[at + 1] = (uint8_t)(v >> 8);
        int r = rng->below(pixel_max + 1) - (pixel_max >> 1);
        if (rng->chance(60)) r = rng->below(33) - 16;
        if (rng->chance(2)) r = rng->below(2) ? 32767 : -32768;
        const size_t res_at = (size_t)g.res_offset[p] + (size_t)y * g.res_stride[p] + (size_t)x * 2;
        f->residuals[res_at] = (uint8_t)r;
        f->residuals[res_at + 1] = (uint8_t)(r >> 8);
      }
    }
  }
  f->wedge.resize((kWedgeEntries << 6) + 32 * 32 + 4);
  for (size_t i = 0; i < f->wedge.size(); ++i) f->wedge[i] = (uint8_t)rng->below(65);
}

// Puts the records in the order of the two paths: the original one sorts by
// iteration first, the new one has all blocks in "iteration 0".
static void order_records(const Geometry &g, Frame *f) {
  const int tiles = (int)g.tiles.size();
  f->iterations = 0;
  for (const Block &b : f->blocks) f->iterations = std::max(f->iterations, b.iter + 1);
  f->counts1.assign((size_t)f->iterations * IntraTypeCount, 0);
  f->block_map2.assign((size_t)tiles * IntraTypeCount, 0);
  std::vector<uint32_t> start1((size_t)f->iterations * IntraTypeCount * tiles, 0);
  for (const Block &b : f->blocks) {
    f->counts1[(size_t)b.iter * IntraTypeCount + b.bucket] += 1 + b.is_uv;
    start1[((size_t)b.iter * IntraTypeCount + b.bucket) * tiles + b.tile] += 1 + b.is_uv;
    f->block_map2[(size_t)b.tile * IntraTypeCount + b.bucket] += 1 + b.is_uv;
  }
  uint32_t total = 0;
  for (uint32_t &v : start1) {
    const uint32_t count = v;
    v = total;
    total += count;
  }
  const uint32_t records = total;
  total = 0;
  for (uint32_t &v : f->block_map2) {
    const uint32_t count = v;
    v = total;
    total += count;
  }
  f->records1.assign((size_t)records * 4 + 4, 0);
  f->records2.assign((size_t)records * 4 + 4, 0);
  std::vector<uint32_t> next2 = f->block_map2;
  for (const Block &b : f->blocks) {
    uint32_t *at1 = &f->records1[4 * (size_t)start1[((size_t)b.iter * IntraTypeCount + b.bucket) * tiles + b.tile]];
    uint32_t *at2 = &f->records2[4 * (size_t)next2[(size_t)b.tile * IntraTypeCount + b.bucket]];
    start1[((size_t)b.iter * IntraTypeCount + b.bucket) * tiles + b.tile] += 1 + b.is_uv;
    next2[(size_t)b.tile * IntraTypeCount + b.bucket] += 1 + b.is_uv;
    for (int k = 0; k <= b.is_uv; ++k) {
      at1[4 * k] = at2[4 * k] = b.x;
      at1[4 * k + 1] = at2[4 * k + 1] = k ? b.y2 : b.y;
      at1[4 * k + 2] = at2[4 * k + 2] = b.z;
    }
  }
}

// ---------------------------------------------------------------------------
// The two paths
// ---------------------------------------------------------------------------

struct RunResult {
  long out_of_bounds, races_group, races_thread, sync_errors;
};

static std::vector<uint32_t> g_tags[4];

static void begin_run(const Geometry &g, int track_phase) {
  for (std::vector<uint32_t> &tags : g_tags) tags.assign(g.pool_size / 4 + 1, 0);
  g_tracker = AccessTracker();
  g_tracker.w_unit = g_tags[0].data();
  g_tracker.w_group = g_tags[1].data();
  g_tracker.w_phase = g_tags[2].data();
  g_tracker.w_thread = g_tags[3].data();
  g_tracker.track_phase = track_phase;
  g_out_of_bounds = 0;
  g_sync_errors = 0;
}

static RunResult end_run() {
  RunResult r;
  r.out_of_bounds = g_out_of_bounds;
  r.races_group = g_tracker.races_group;
  r.races_thread = g_tracker.races_thread;
  r.sync_errors = g_sync_errors;
  g_tracker.w_unit = nullptr;
  return r;
}

// The intra loop of av1_prediction_run_all().
static RunResult run_original(const Geometry &g, const Frame &f, std::vector<uint8_t> *pool, uint64_t garbage,
                              Coverage *cov) {
  const ShaderApi *main_api = g.hbd ? &intra_main_hbd_api : &intra_main_api;
  const ShaderApi *filter_api = g.hbd ? &intra_filter_hbd_api : &intra_filter_api;
  ShaderBuffers buffers = {};
  buffers.pred_blocks = (const uint8_t *)f.records1.data();
  buffers.pred_blocks_size = f.records1.size() * 4;
  buffers.residuals = f.residuals.data();
  buffers.residuals_size = f.residuals.size();
  buffers.wedge = f.wedge.data();
  buffers.wedge_size = f.wedge.size();
  buffers.frame = pool->data();
  buffers.frame_size = pool->size();
  main_api->bind(&buffers);
  main_api->set_common(&f.consts);
  filter_api->bind(&buffers);
  filter_api->set_common(&f.consts);

  begin_run(g, 0);
  uint32_t offset = 0;
  for (int it = 0; it < f.iterations; ++it) {
    const uint32_t *counts = &f.counts1[(size_t)it * IntraTypeCount];
    uint32_t srt[10];
    uint32_t wi_count = counts[8] << 2;
    for (int k = 0; k < 8; ++k) {
      srt[k] = counts[k];
      wi_count += counts[k] << (10 - k);
    }
    srt[8] = wi_count;
    srt[9] = offset;
    if (wi_count) {
      main_api->set_srt(srt);
      const uint32_t groups = (wi_count + 255) >> 8;
      for (uint32_t group = 0; group < groups; ++group)
        run_group(main_api, group, (int)std::min<uint32_t>(256, wi_count - group * 256), &garbage);
      if (cov) {
        cov->v1_dispatches++;
        cov->v1_groups += groups;
      }
    }
    for (int k = 0; k < 9; ++k) offset += counts[k];
    if (counts[9]) {
      const uint32_t filter_srt[2] = { counts[9] << 3, offset };
      filter_api->set_srt(filter_srt);
      for (uint32_t group = 0; group < counts[9]; ++group) run_group(filter_api, group, 64, &garbage);
      if (cov) {
        cov->v1_dispatches++;
        cov->v1_groups += counts[9];
      }
    }
    offset += counts[9];
    g_tracker.unit++;  // the UAV barrier
  }
  if (cov) cov->v1_iterations += f.iterations;
  return end_run();
}

struct Schedule {
  std::vector<uint32_t> gpu;  // cell map, then region list
  std::vector<uint32_t> regions;
  volatile long list_count;
  IntraV2Sched sched;
};

static void make_schedule(const Geometry &g, Schedule *s) {
  const size_t cells = (size_t)g.cells_y + g.cells_uv;
  s->gpu.assign(4 * cells, 0);
  s->regions.assign(2 * (size_t)g.regions, 0);
  s->list_count = 0;
  s->sched.cells = s->gpu.data();
  s->sched.regions = s->regions.data();
  s->sched.list = (uint64_t *)(s->gpu.data() + 2 * cells);
  s->sched.list_count = &s->list_count;
  s->sched.map_stride = g.pad_cols;
  s->sched.map_uv_offset = g.cells_y;
  s->sched.regions_stride = g.pad_cols >> 4;
  s->sched.stamp = 0;
}

// The intra loop of av1_prediction_run_all() with -intrav2.
static RunResult run_v2(const Geometry &g, const Frame &f, Schedule *s, std::vector<uint8_t> *pool, uint64_t garbage,
                        Coverage *cov) {
  const ShaderApi *api = g.hbd ? &intra_v2_hbd_api : &intra_v2_api;
  const size_t cells = (size_t)g.cells_y + g.cells_uv;
  ShaderBuffers buffers = {};
  buffers.pred_blocks = (const uint8_t *)f.records2.data();
  buffers.pred_blocks_size = f.records2.size() * 4;
  buffers.residuals = f.residuals.data();
  buffers.residuals_size = f.residuals.size();
  buffers.wedge = f.wedge.data();
  buffers.wedge_size = f.wedge.size();
  buffers.sched = (const uint8_t *)s->gpu.data();
  buffers.sched_size = s->gpu.size() * 4;
  buffers.block_map = (const uint8_t *)f.block_map2.data();
  buffers.block_map_size = f.block_map2.size() * 4;
  buffers.frame = pool->data();
  buffers.frame_size = pool->size();
  api->bind(&buffers);
  api->set_common(&f.consts);

  begin_run(g, 1);
  const int count = (int)s->list_count;
  intra_v2_sort(s->sched.list, count);
  IntraV2Dispatch d;
  for (int first = 0; intra_v2_next_dispatch(s->sched.list, count, first, &d); first += d.count) {
    const uint32_t srt[8] = { (uint32_t)(8 * (cells + d.first)), d.step, d.passes, s->sched.stamp,
                              (uint32_t)s->sched.map_stride, (uint32_t)s->sched.map_uv_offset, 0, 0 };
    api->set_srt(srt);
    for (int group = 0; group < d.count; ++group) run_group(api, (uint32_t)group, 384, &garbage);
    if (cov) {
      cov->v2_dispatches++;
      cov->v2_groups += d.count;
      cov->v2_group_passes += (long)d.count * d.passes;
      cov->v2_max_passes = std::max(cov->v2_max_passes, (long)d.passes);
      cov->v2_steps += d.last_of_step;
    }
    if (d.last_of_step) g_tracker.unit++;  // the UAV barrier
  }
  return end_run();
}

// ---------------------------------------------------------------------------

static void describe(const Geometry &g, const Frame &f, const Schedule &s, size_t byte) {
  for (int p = 0; p < 3; ++p) {
    const size_t begin = (size_t)g.offset[p];
    const size_t end = begin + (size_t)g.stride[p] * g.height[p];
    if (byte < begin || byte >= end) continue;
    const int x = (int)((byte - begin) % g.stride[p]) >> g.hbd;
    const int y = (int)((byte - begin) / g.stride[p]);
    printf("    plane %d, pixel (%d, %d)\n", p, x, y);
    for (const Block &b : f.blocks) {
      if (b.is_uv != (p > 0)) continue;
      const int bx = b.x & 0xffff;
      const int by = b.x >> 16;
      const int wl = b.y & 7;
      const int hl = b.bucket == 9 ? (int)(b.z & 3) : 8 - b.bucket - wl;
      if ((x >> 2) < bx || (x >> 2) >= bx + (1 << wl) || (y >> 2) < by || (y >> 2) >= by + (1 << hl)) continue;
      const uint32_t word = p == 2 ? b.y2 : b.y;
      const size_t cell = b.is_uv ? (size_t)g.cells_y + bx + (size_t)by * (g.pad_cols >> 1) : bx + (size_t)by * g.pad_cols;
      printf("    block at cell (%d, %d), %dx%d, mode %u angle %u, above %u left %u, up %u filt %u/%u, type %d,\n"
             "    record %08x %08x %08x, iteration %d, step %u pass %u, tile %d\n",
             bx, by, 4 << wl, 4 << hl, (word >> 6) & 15, (word >> 28) & 7, (word >> 10) & 63, (word >> 16) & 63,
             (word >> 22) & 3, (word >> 24) & 3, (word >> 26) & 3, b.bucket, b.x, word, b.z, b.iter,
             s.gpu[2 * cell + 1] & 0xffff, (s.gpu[2 * cell + 1] >> 16) & 255, b.tile);
      return;
    }
    printf("    (no intra block there)\n");
    return;
  }
  printf("    byte %zu is outside the planes\n", byte);
}

static size_t first_difference(const std::vector<uint8_t> &a, const std::vector<uint8_t> &b) {
  for (size_t i = 0; i < a.size(); ++i) {
    if (a[i] != b[i]) return i;
  }
  return a.size();
}

int main(int argc, char **argv) {
  int layouts = 120;
  uint64_t seed = 20261111;
  int verbose = 0;
  for (int i = 1; i < argc; ++i) {
    if (!strcmp(argv[i], "--layouts") && i + 1 < argc) {
      layouts = atoi(argv[++i]);
    } else if (!strcmp(argv[i], "--seed") && i + 1 < argc) {
      seed = strtoull(argv[++i], nullptr, 10);
    } else if (!strcmp(argv[i], "-v")) {
      verbose = 1;
    } else {
      fprintf(stderr, "usage: intra_check [--layouts N] [--seed S] [-v]\n");
      return 2;
    }
  }
  g_stacks = (char *)malloc((size_t)kMaxThreads * kStackSize);
  if (!g_stacks) return 2;

  int failures = 0;
  long garbage_dependent = 0;
  long original_races = 0;
  for (int hbd = 0; hbd < 2; ++hbd) {
    Coverage cov = {};
    Rng rng = { seed + (uint64_t)hbd * 7919 };
    for (int layout = 0; layout < layouts && failures < 5; ++layout) {
      const Geometry g = make_geometry(&rng, hbd);
      Schedule schedule;
      make_schedule(g, &schedule);
      // Three frames share the schedule buffers, like the frames of a
      // decoder: the entries of earlier frames stay behind with an old stamp.
      if (layout % 4 == 0) schedule.sched.stamp = 253;
      for (int n = 0; n < 3 && failures < 5; ++n) {
        if (intra_v2_next_stamp(&schedule.sched.stamp)) {
          std::fill(schedule.gpu.begin(), schedule.gpu.end(), 0);
          std::fill(schedule.regions.begin(), schedule.regions.end(), 0);
        }
        schedule.list_count = 0;
        Frame f;
        make_frame_data(g, &rng, &f);
        Generator gen(g, &rng, &schedule.sched, &cov, &f);
        // A key frame, then frames with less and less intra.
        static const int intra_percent[3] = { 100, 45, 12 };
        gen.run(intra_percent[n], rng.below(3) * 20, 15 + rng.below(4) * 20);
        order_records(g, &f);
        cov.frames++;
        if (g.tiles.size() > 1) cov.multi_tile++;

        std::vector<uint8_t> original = f.pixels;
        std::vector<uint8_t> original_again = f.pixels;
        std::vector<uint8_t> v2 = f.pixels;
        const RunResult r1 = run_original(g, f, &original, rng.u32(), &cov);
        const RunResult r1b = run_original(g, f, &original_again, rng.u32(), nullptr);
        const RunResult r2 = run_v2(g, f, &schedule, &v2, rng.u32(), &cov);
        original_races += r1.races_group;

        const char *problem = nullptr;
        size_t at = 0;
        if (original != original_again) {
          // Not a property of the new path, but then there is no single
          // right answer to compare with.
          garbage_dependent++;
          problem = "the original path depends on uninitialised group memory";
          at = first_difference(original, original_again);
        } else if (original != v2) {
          problem = "MISMATCH between the original path and intra_v2";
          at = first_difference(original, v2);
        } else if (r1.sync_errors || r1b.sync_errors || r2.sync_errors) {
          problem = "a group sync was not reached by all threads of a group";
        } else if (r2.out_of_bounds) {
          problem = "intra_v2 reads or writes outside its buffers";
        } else if (r2.races_group) {
          problem = "intra_v2 reads what another thread group of the same step wrote";
        } else if (r2.races_thread) {
          problem = "intra_v2 reads what another thread wrote in the same pass";
        }
        if (verbose || problem) {
          printf("%s layout %d frame %d: %dx%d, %zu tile(s), sb %d, %zu blocks, %d iterations -> %ld region entries\n",
                 hbd ? "10-bit" : "8-bit", layout, n, g.mi_cols * 4, g.mi_rows * 4, g.tiles.size(), 4 << g.sb_log,
                 f.blocks.size(), f.iterations, (long)schedule.list_count);
        }
        if (problem) {
          ++failures;
          printf("  %s\n", problem);
          if (at < original.size()) describe(g, f, schedule, at);
          printf("  out of bounds %ld/%ld, group races %ld/%ld, thread races %ld, sync errors %ld/%ld\n",
                 r1.out_of_bounds, r2.out_of_bounds, r1.races_group, r2.races_group, r2.races_thread, r1.sync_errors,
                 r2.sync_errors);
        }
      }
    }

    printf("%s: %ld frames (%ld with several tiles), %ld transform blocks\n", hbd ? "10-bit" : "8-bit", cov.frames,
           cov.multi_tile, cov.blocks);
    static const char *const names[16] = { "V",      "H",        "D45",      "D135",  "D113", "D157", "D203", "D67",
                                           "SMOOTH", "SMOOTH_V", "SMOOTH_H", "PAETH", "-",    "FILTER", "DC",  "CFL" };
    printf("  luma modes:  ");
    for (int m = 0; m < 16; ++m) {
      if (cov.luma_mode[m]) printf(" %s %ld", names[m], cov.luma_mode[m]);
    }
    printf("\n  chroma modes:");
    for (int m = 0; m < 16; ++m) {
      if (cov.chroma_mode[m]) printf(" %s %ld", names[m], cov.chroma_mode[m]);
    }
    printf("\n  by size (4096 pixels down to 16, then filter intra):");
    for (int k = 0; k < 10; ++k) printf(" %ld", cov.sizes[k]);
    printf("\n  inter-intra %ld, with residual %ld, top-right %ld, bottom-left %ld, edge filter %ld, upsampling %ld,\n"
           "  blocks cut by the frame edge %ld\n",
           cov.inter_intra, cov.residual, cov.top_right, cov.bottom_left, cov.edge_filter, cov.upsample, cov.clipped);
    printf("  original: %ld iterations (UAV barriers), %ld dispatches, %ld thread groups\n", cov.v1_iterations,
           cov.v1_dispatches, cov.v1_groups);
    printf("  intra_v2: %ld steps (UAV barriers), %ld dispatches, %ld thread groups, %.1f passes per group, at most %ld\n",
           cov.v2_steps, cov.v2_dispatches, cov.v2_groups,
           cov.v2_groups ? (double)cov.v2_group_passes / (double)cov.v2_groups : 0.0, cov.v2_max_passes);
  }
  if (original_races) printf("note: %ld order-dependent reads between thread groups in the original path\n", original_races);
  if (garbage_dependent) printf("note: %ld frames where the original path depends on uninitialised memory\n", garbage_dependent);
  if (failures) {
    printf("intra_check: FAILED\n");
    return 1;
  }
  printf("intra_check: intra_v2 output is identical to the original path on every frame\n");
  return 0;
}

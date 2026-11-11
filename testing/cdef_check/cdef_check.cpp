/*
 * Bit-exactness check of cdef_filter_v2.hlsl against cdef_filter.hlsl.
 *
 * Both shaders are compiled as C++ (see run.sh and hlsl_shim.h) and run over
 * the same random frames, one dispatch each. The whole frame pool, padding
 * included, must be byte-identical afterwards.
 *
 * The threads of a group run under an adversarial schedule and the group
 * memory is watched for races (testing/common/group_sched.h): a race, a group
 * sync that not all threads reach, or a plain GroupMemoryBarrier() fails the
 * check, as a frame that differs does.
 *
 * Runs on a Linux host, no GPU:  sh testing/cdef_check/run.sh
 */
#define GROUP_SCHED_IMPLEMENTATION
#include <random>
#include <vector>

#include "hlsl_shim.h"

long g_out_of_bounds = 0;

namespace v1 {
#include "cdef_filter.gen.h"
}
namespace v2 {
#include "cdef_filter_v2.gen.h"
}

// ---------------------------------------------------------------------------
// Thread groups: testing/common/group_sched.h. Between two group syncs every thread runs alone, in an
// order drawn anew at every sync, and the group memory records every read of what another thread wrote
// since the last group sync: a shader passes only if it does not depend on how the threads of a group
// are interleaved.
// ---------------------------------------------------------------------------
static uint64_t g_order_state = 0x2545f4914f6cdd1dull;

static void thread_body(int thread, void *context) {
  void (*body)(int) = reinterpret_cast<void (*)(int)>(context);
  body(thread);
}

// Runs body(thread) for every thread of one group.
static void run_group(int threads, void (*body)(int thread)) {
  gs_run_group(threads, thread_body, reinterpret_cast<void *>(body), &g_order_state);
}

// ---------------------------------------------------------------------------
// One test frame, laid out like the decoder's frame pool.
// ---------------------------------------------------------------------------
struct HostCDefData {  // CDefData of libav1/dx/loopfilters.cpp
  int plane[4];
  int uv_stride;
  int dst_offset[3];
  int uv_offset[2];
  int index_stride;
  int skips_stride;
  int pri_damping;
  int sec_damping;
  int pli;
  int hbd;
  int bit_depth;
  int _dummie[3];
  int cdef_directions[16][2][4];
  int cdef_strength[16][4];
  int cdef_uv_strength[16][4];
};
static_assert(sizeof(HostCDefData) == sizeof(v1::CDefData), "constant buffer layout");
static_assert(sizeof(HostCDefData) == sizeof(v2::CDefData), "constant buffer layout");

struct Case {
  int w, h;        // what the host passes as plane[2], plane[3]
  int hbd;         // 16-bit samples
  int bit_depth;
  int content;     // 0 noise, 1 gradient + noise, 2 flat, 3 edges, 4 extremes
  int strengths;   // 0 random, 1 luma only, 2 chroma only, 3 none, 4 primary only, 5 secondary only
  int skip_mode;   // 0 half, 1 none, 2 all, 3 rare, 4 any integer
  int index_mode;  // 0 random incl. -1, 1 all valid, 2 all -1, 3 with out of range values
  int damping;
};

struct Frame {
  std::vector<uint8_t> pool;
  std::vector<int> indexes;
  std::vector<int> skips;
  HostCDefData cb;
};

static std::mt19937 g_rng(20261111);
static int rnd(int n) { return static_cast<int>(g_rng() % static_cast<uint32_t>(n)); }

static void put_sample(Frame &f, int offset, int stride, int bpp, int x, int y, int v) {
  uint8_t *p = &f.pool[static_cast<size_t>(offset) + static_cast<size_t>(y) * stride + static_cast<size_t>(x) * bpp];
  p[0] = static_cast<uint8_t>(v);
  if (bpp == 2) p[1] = static_cast<uint8_t>(v >> 8);
}

static int content_sample(const Case &c, int plane, int x, int y, int maxv) {
  switch (c.content) {
    case 0: return rnd(maxv + 1);
    case 1: {
      const int v = (x * (3 + plane) + y * (5 - plane)) * (maxv + 1) / 512 + rnd(1 + (maxv >> 4)) + (maxv >> 3);
      return v > maxv ? maxv : v;
    }
    case 2: return (maxv >> 1) + plane;
    case 3: {
      // Straight edges in several directions, a little noise on top.
      const int a = ((x + 2 * y) / 9) & 1;
      const int b = ((3 * x - y + 4096) / 14) & 1;
      const int v = (a ? maxv - (maxv >> 2) : maxv >> 3) + (b ? maxv >> 4 : 0) + rnd(1 + (maxv >> 6));
      return v > maxv ? maxv : v;
    }
    default: return rnd(4) == 0 ? rnd(maxv + 1) : (rnd(2) ? maxv : 0);
  }
}

static void make_frame(const Case &c, Frame &f) {
  const int bpp = c.hbd ? 2 : 1;
  // Frame buffer geometry of av1_reallocate_frame_buffer().
  const int aligned_w = (c.w + 127) & ~127;
  const int aligned_h = (c.h + 127) & ~127;
  const int y_stride = (aligned_w + 32) * bpp;
  const int uv_stride = ((aligned_w >> 1) + 32) * bpp;
  const int y_size = y_stride * aligned_h;
  const int uv_size = uv_stride * (aligned_h >> 1);
  const int frame_size = y_size + 2 * uv_size;
  const int base[2] = {64, 64 + frame_size + 192};  // source frame, destination frame
  f.pool.resize(static_cast<size_t>(base[1]) + frame_size + 64);
  for (uint8_t &b : f.pool) b = static_cast<uint8_t>(g_rng());

  int offsets[2][3];
  for (int i = 0; i < 2; i++) {
    offsets[i][0] = base[i] + 16 * bpp;
    offsets[i][1] = base[i] + y_size + 16 * bpp;
    offsets[i][2] = offsets[i][1] + uv_size;
  }
  // 16-bit frames keep random bits above the sample: the shaders mask them.
  const int maxv = (1 << (c.hbd ? (c.bit_depth > 10 ? 10 : c.bit_depth) : 8)) - 1;
  const int high = c.hbd ? 0xffff & ~((1 << 10) - 1) : 0;
  for (int y = 0; y < c.h; y++)
    for (int x = 0; x < c.w; x++)
      put_sample(f, offsets[0][0], y_stride, bpp, x, y, content_sample(c, 0, x, y, maxv) | (rnd(8) ? 0 : high));
  for (int p = 1; p < 3; p++)
    for (int y = 0; y < (c.h >> 1); y++)
      for (int x = 0; x < (c.w >> 1); x++)
        put_sample(f, offsets[0][p], uv_stride, bpp, x, y, content_sample(c, p, x, y, maxv) | (rnd(8) ? 0 : high));

  const int nhfb = (c.w + 63) >> 6;
  const int nvfb = (c.h + 63) >> 6;
  f.indexes.resize(static_cast<size_t>(nhfb) * nvfb);
  for (int &v : f.indexes) {
    switch (c.index_mode) {
      case 0: v = rnd(5) == 0 ? -1 : rnd(16); break;
      case 1: v = rnd(16); break;
      case 2: v = -1; break;
      default: v = rnd(4) == 0 ? rnd(40) - 12 : rnd(16); break;
    }
  }
  // The host only fills the rows and columns inside the frame; the rest keeps
  // whatever was there, so every entry is random here.
  f.skips.resize(static_cast<size_t>(nhfb) * 16 * nvfb * 16);
  for (int &v : f.skips) {
    switch (c.skip_mode) {
      case 0: v = rnd(2); break;
      case 1: v = 0; break;
      case 2: v = 1; break;
      case 3: v = rnd(12) == 0; break;
      default: v = rnd(3) ? 0 : static_cast<int>(g_rng()); break;
    }
  }

  HostCDefData &cb = f.cb;
  for (size_t i = 0; i < sizeof(cb) / sizeof(int); i++) reinterpret_cast<int *>(&cb)[i] = static_cast<int>(g_rng());
  const int cdef_directions_new[8][4] = {{-1, +1, -2, +2}, {0, +1, -1, +2}, {0, +1, 0, +2}, {0, +1, 1, +2},
                                         {1, +1, 2, +2},   {1, +0, 2, +1},  {1, +0, 2, +0}, {1, +0, 2, -1}};
  for (int i = 0; i < 8; i++) {
    cb.cdef_directions[i][0][1] = cdef_directions_new[i][0];
    cb.cdef_directions[i][0][0] = cdef_directions_new[i][1];
    cb.cdef_directions[i][1][1] = cdef_directions_new[i][2];
    cb.cdef_directions[i][1][0] = cdef_directions_new[i][3];
    cb.cdef_directions[8 + i][0][1] = cdef_directions_new[i][0];
    cb.cdef_directions[8 + i][0][0] = cdef_directions_new[i][1];
    cb.cdef_directions[8 + i][1][1] = cdef_directions_new[i][2];
    cb.cdef_directions[8 + i][1][0] = cdef_directions_new[i][3];
  }
  for (int i = 0; i < 16; i++) {
    int y = rnd(64);
    int uv = rnd(64);
    switch (c.strengths) {
      case 1: uv = 0; break;
      case 2: y = 0; break;
      case 3: y = uv = 0; break;
      case 4: y &= ~3; uv &= ~3; break;
      case 5: y &= 3; uv &= 3; break;
      default:
        if (rnd(4) == 0) y = 0;
        if (rnd(4) == 0) uv = 0;
        break;
    }
    cb.cdef_strength[i][0] = y;
    cb.cdef_uv_strength[i][0] = uv;
  }
  cb.index_stride = nhfb;
  cb.skips_stride = nhfb * 16;
  cb.hbd = c.hbd;
  cb.bit_depth = c.bit_depth;
  cb.pri_damping = c.damping;
  cb.plane[0] = y_stride;
  cb.plane[1] = offsets[0][0];
  cb.plane[2] = c.w;
  cb.plane[3] = c.h;
  cb.uv_stride = uv_stride;
  cb.uv_offset[0] = offsets[0][1];
  cb.uv_offset[1] = offsets[0][2];
  cb.dst_offset[0] = offsets[1][0];
  cb.dst_offset[1] = offsets[1][1];
  cb.dst_offset[2] = offsets[1][2];
}

// ---------------------------------------------------------------------------
// The two dispatches.
// ---------------------------------------------------------------------------
static uint3 g_group;

template <typename T>
static void scramble(T &shared) {
  // Group shared memory starts undefined.
  uint32_t *p = reinterpret_cast<uint32_t *>(shared.bytes());
  for (size_t i = 0; i < shared.byte_size() / 4; i++) p[i] = g_rng();
}

static void v1_thread(int t) {
  const uint3 gtid = {static_cast<uint>(t & 3), static_cast<uint>((t >> 2) & 3), static_cast<uint>(t >> 4)};
  const uint3 dtid = {g_group.x * 4 + gtid.x, g_group.y * 4 + gtid.y, gtid.z};
  v1::shader_main(g_group, dtid, gtid, static_cast<uint>(t));
}

static void v2_thread(int t) {
  const uint3 gtid = {static_cast<uint>(t & 7), static_cast<uint>(t >> 3), 0};
  v2::shader_main(g_group, gtid);
}

static void run_v1(Frame &f) {
  gs_state.shader = "cdef_filter.hlsl";
  v1::cdef_index.ptr = reinterpret_cast<const uint8_t *>(f.indexes.data());
  v1::cdef_index.size = f.indexes.size() * 4;
  v1::skips.ptr = reinterpret_cast<const uint8_t *>(f.skips.data());
  v1::skips.size = f.skips.size() * 4;
  v1::dst.ptr = f.pool.data();
  v1::dst.size = f.pool.size();
  memcpy(&v1::data, &f.cb, sizeof(f.cb));
  // Dispatch((w + 31) >> 5, (h + 31) >> 5, 1), numthreads(4, 4, 8)
  for (uint gy = 0; gy < static_cast<uint>((f.cb.plane[3] + 31) >> 5); gy++) {
    for (uint gx = 0; gx < static_cast<uint>((f.cb.plane[2] + 31) >> 5); gx++) {
      g_group = uint3{gx, gy, 0};
      scramble(v1::input);
      scramble(v1::output);
      scramble(v1::costs);
      scramble(v1::temp);
      run_group(128, v1_thread);
    }
  }
}

static void run_v2(Frame &f) {
  gs_state.shader = "cdef_filter_v2.hlsl";
  v2::cdef_index.ptr = reinterpret_cast<const uint8_t *>(f.indexes.data());
  v2::cdef_index.size = f.indexes.size() * 4;
  v2::skips.ptr = reinterpret_cast<const uint8_t *>(f.skips.data());
  v2::skips.size = f.skips.size() * 4;
  v2::dst.ptr = f.pool.data();
  v2::dst.size = f.pool.size();
  memcpy(&v2::data, &f.cb, sizeof(f.cb));
  // Dispatch((w + 63) >> 6, (h + 63) >> 6, 1), numthreads(8, 8, 1)
  for (uint gy = 0; gy < static_cast<uint>((f.cb.plane[3] + 63) >> 6); gy++) {
    for (uint gx = 0; gx < static_cast<uint>((f.cb.plane[2] + 63) >> 6); gx++) {
      g_group = uint3{gx, gy, 0};
      scramble(v2::window);
      run_group(64, v2_thread);
    }
  }
}

int main() {
  // Width and height as the decoder passes them (multiples of 8), then a few
  // sizes it never produces, which both shaders must still treat alike.
  static const int sizes[][2] = {{8, 8},    {16, 8},   {8, 24},    {32, 32},  {40, 32},  {32, 40},   {64, 64},
                                 {72, 64},  {64, 72},  {96, 96},   {104, 56}, {56, 104}, {128, 128}, {136, 72},
                                 {160, 96}, {200, 88}, {88, 200},  {192, 136}, {30, 20}, {67, 45},   {100, 70}};
  static const int depths[][2] = {{0, 8}, {1, 10}, {1, 8}, {1, 12}};  // hbd, bit depth
  long cases = 0, failed = 0, bytes = 0, changed = 0;

  for (const auto &size : sizes) {
    for (const auto &depth : depths) {
      for (int round = 0; round < 14; round++) {
        Case c;
        c.w = size[0];
        c.h = size[1];
        c.hbd = depth[0];
        c.bit_depth = depth[1];
        c.content = round < 5 ? round : rnd(5);
        c.strengths = round < 6 ? round : (rnd(3) ? 0 : rnd(6));
        c.skip_mode = round < 5 ? round : rnd(5);
        c.index_mode = round < 4 ? (round == 2 ? 1 : round) : (rnd(3) ? 1 : rnd(4));
        c.damping = 3 + rnd(4);

        Frame a;
        make_frame(c, a);
        Frame b = a;
        const std::vector<uint8_t> before = a.pool;
        run_v1(a);
        run_v2(b);

        cases++;
        bytes += static_cast<long>(a.pool.size());
        for (size_t i = 0; i < a.pool.size(); i++) changed += a.pool[i] != before[i];
        size_t first = a.pool.size();
        long diffs = 0;
        for (size_t i = 0; i < a.pool.size(); i++) {
          if (a.pool[i] != b.pool[i]) {
            if (!diffs) first = i;
            diffs++;
          }
        }
        if (diffs) {
          failed++;
          if (failed <= 20)
            printf("MISMATCH %dx%d hbd=%d depth=%d content=%d strengths=%d skips=%d indexes=%d damping=%d: "
                   "%ld bytes differ, first at %zu (dst luma starts at %d)\n",
                   c.w, c.h, c.hbd, c.bit_depth, c.content, c.strengths, c.skip_mode, c.index_mode, c.damping, diffs,
                   first, a.cb.dst_offset[0]);
        }
      }
    }
  }

  printf("cdef_check: %ld frames, %ld bytes compared, %ld bytes written differently from the pool's initial content\n",
         cases, bytes, changed);
  printf("cdef_check: out-of-bounds buffer accesses %ld, mismatching frames %ld\n", g_out_of_bounds, failed);
  printf("cdef_check: %ld group syncs; races in group memory: %ld, group syncs not reached by all threads: %ld, "
         "plain group memory barriers: %ld\n",
         gs_state.group_syncs, gs_state.races, gs_state.sync_errors, gs_state.plain_barriers);
  gs_print_races("  ");
  if (failed || g_out_of_bounds || !changed || gs_state.races || gs_state.sync_errors || gs_state.plain_barriers ||
      gs_state.out_of_bounds) {
    printf("cdef_check: FAILED\n");
    return 1;
  }
  printf("cdef_check: OK, cdef_filter_v2.hlsl output is byte-identical to cdef_filter.hlsl\n");
  return 0;
}

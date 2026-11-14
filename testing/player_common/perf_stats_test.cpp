/*
 * Host-side sanity check for the -gputime summary maths. Not part of the
 * player build. From the repository root:
 *
 *   g++ -std=c++14 -Wall -Wextra -I libav1 -o perf_stats_test testing/player_common/perf_stats_test.cpp
 *   ./perf_stats_test
 */

#include <math.h>
#include <stdio.h>
#include <string.h>

#include "perf_stats.h"

static int g_failed = 0;

#define CHECK(cond)                                           \
  do {                                                        \
    if (!(cond)) {                                            \
      printf("FAILED %s:%d: %s\n", __FILE__, __LINE__, #cond); \
      ++g_failed;                                             \
    }                                                         \
  } while (0)

static bool Near(double a, double b) { return fabs(a - b) < 1e-9; }

static bool Has(const std::vector<std::string>& lines, const char* text) {
  for (size_t i = 0; i < lines.size(); ++i)
    if (lines[i].find(text) != std::string::npos) return true;
  return false;
}

static av1_perf_frame MakeFrame(int n, double complete_ms, int show) {
  av1_perf_frame f;
  memset(&f, 0, sizeof(f));
  f.frame_number = n;
  f.show_frame = show;
  f.intra_iters = 40;
  f.intra_v2 = 1;
  f.intra_dispatches = 12;
  f.intra_barriers = 9;
  f.intra_groups = 300;
  f.intra_passes = 2400;
  f.tile_count = 4;
  f.gpu_valid = 1;
  f.gpu_marks = 4;
  f.gpu_stage_ms[AV1_PERF_GPU_IDCT] = 4.0;
  f.gpu_stage_hits[AV1_PERF_GPU_IDCT] = 1;
  f.gpu_stage_ms[AV1_PERF_GPU_INTRA] = 10.0;
  f.gpu_stage_hits[AV1_PERF_GPU_INTRA] = 1;
  f.gpu_stage_ms[AV1_PERF_GPU_CDEF] = 6.0;
  f.gpu_stage_hits[AV1_PERF_GPU_CDEF] = 1;
  f.gpu_sum_ms = 20.0;
  f.gpu_span_ms = 20.0;
  f.gpu_gap_ms = n ? 30.0 : -1.0;
  f.cpu_ms[AV1_PERF_CPU_ENTROPY_WALL] = 12.0;
  f.cpu_ms[AV1_PERF_CPU_ENTROPY_SUM] = 48.0;
  f.cpu_ms[AV1_PERF_CPU_FRAME] = 25.0;
  f.cpu_ms[AV1_PERF_CPU_WAIT_OUTPUT] = 3.0;
  f.cpu_ms[AV1_PERF_CPU_WAIT_SLOT] = 2.0;
  f.memset_bytes = 8 * 1024 * 1024;
  f.cpu_ms[AV1_PERF_CPU_MEMSET] = 1.0;
  f.cpu_ms[AV1_PERF_CPU_MEMSET_TILE] = 0.25;
  f.cpu_ms[AV1_PERF_CPU_ASYNC_CLEAR] = 3.5;
  f.async_clear_bytes = 6 * 1024 * 1024;
  // -gpuclear: the clears at the end of the command list. (A real frame has them in one of the three places.)
  f.gpu_stage_ms[AV1_PERF_GPU_SLOT_CLEAR] = 0.5;
  f.gpu_stage_hits[AV1_PERF_GPU_SLOT_CLEAR] = 1;
  f.gpu_clear_bytes = 4 * 1024 * 1024;
  f.complete_ms = complete_ms;
  return f;
}

// The inter breakdown of a frame (-gputimedetail), as the decoder hands it over. A 1920x1080 frame's worth of
// work with round numbers: single blocks of 16x16 and 8x8, compound blocks of 16x16, overlapped prediction
// from above and from the left, and a few reconstructed blocks.
static void AddDispatch(av1_perf_inter_detail* d, int section, int list, int width, int height, int records,
                        int threads, int thread_bytes, double ms) {
  av1_perf_inter_section* s = &d->sections[section];
  ++s->dispatches;
  s->groups += (threads + 63) / 64;
  s->threads += threads;
  s->records += records;
  s->pixels += (int64_t)records * width * height;
  s->loaded_bytes += (int64_t)threads * thread_bytes;
  s->lists[list].width = width;
  s->lists[list].height = height;
  s->lists[list].records = records;
  s->lists[list].groups = (threads + 63) / 64;
  s->lists[list].threads = threads;
  if (d->mode == 2) {
    s->lists[list].ms = ms;
    s->ms += ms;
    s->timed = 1;
    ++d->barriers_added;
    ++d->marks;
  }
}

static void SectionTime(av1_perf_inter_detail* d, int section, double ms) {
  d->sections[section].ms += ms;
  d->sections[section].timed = 1;
  ++d->marks;
}

static const av1_perf_inter_detail* MakeDetail(int mode, int scaled) {
  static av1_perf_inter_detail d;
  memset(&d, 0, sizeof(d));
  d.mode = mode;
  d.has_inter = 1;
  d.scaled = scaled;
  d.bytes_per_sample = 2;
  // single: 3000 records of 16x16 (2000 luma blocks, 500 chroma blocks of two records), 16 threads each, and
  // 1000 records of 8x8, 4 threads each: 52000 threads, 3 ms. 264 bytes a thread.
  AddDispatch(&d, AV1_PERF_INTER_SINGLE, 4, 8, 8, 1000, 4000, 264, 0.5);
  AddDispatch(&d, AV1_PERF_INTER_SINGLE, 9, 16, 16, 3000, 48000, 264, 2.5);
  // comp_avg: 1000 records of 16x16, 32 threads of 4x2 each, 432 bytes a thread, 2 ms.
  AddDispatch(&d, AV1_PERF_INTER_COMP_AVG, 9, 16, 16, 1000, 32000, 432, 2.0);
  AddDispatch(&d, AV1_PERF_INTER_OBMC_ABOVE, 5, 8, 4, 100, 200, 264, 0.5);
  AddDispatch(&d, AV1_PERF_INTER_OBMC_LEFT, 5, 4, 8, 100, 200, 264, 0.25);
  AddDispatch(&d, AV1_PERF_INTER_RECON, 7, 8, 8, 50, 200, 0, 1.0);
  if (mode == 1) {
    SectionTime(&d, AV1_PERF_INTER_SINGLE, 3.0);
    SectionTime(&d, AV1_PERF_INTER_COMP_AVG, 2.0);
    SectionTime(&d, AV1_PERF_INTER_OBMC_ABOVE, 0.5);
    SectionTime(&d, AV1_PERF_INTER_OBMC_LEFT, 0.25);
    d.sections[AV1_PERF_INTER_RECON].ms = 1.0;
    d.sections[AV1_PERF_INTER_RECON].timed = 1;
  }
  // The barriers: pass 1 waits 1 ms at its barrier, pass 2 a quarter, pass 3 half.
  SectionTime(&d, AV1_PERF_INTER_SYNC0, 1.0);
  SectionTime(&d, AV1_PERF_INTER_SYNC1, 0.25);
  SectionTime(&d, AV1_PERF_INTER_SYNC2, 0.5);

  av1_perf_inter_work* w = &d.sections[AV1_PERF_INTER_SINGLE].work;
  w->blocks_y = 2500;  // 2000 of 16x16 and 500 of 8x8
  w->blocks_uv = 750;  // 500 of 16x16 and 250 of 8x8, two records each
  w->pixels_y = 2000 * 256 + 500 * 64;
  w->pixels_uv = 2 * (500 * 256 + 250 * 64);
  w->ref_blocks = 2500 + 2 * 750;
  w->footprint = 2000 * 23 * 23 + 500 * 15 * 15 + 2 * (500 * 23 * 23 + 250 * 15 * 15);
  w->footprint_mv = w->footprint / 2;
  w = &d.sections[AV1_PERF_INTER_COMP_AVG].work;
  w->blocks_y = 1000;
  w->pixels_y = 1000 * 256;
  w->ref_blocks = 2000;
  w->footprint = 2000 * 23 * 23;
  w->footprint_mv = w->footprint;
  // The overlapped predictions: 50 luma blocks and 25 chroma blocks each.
  w = &d.sections[AV1_PERF_INTER_OBMC_ABOVE].work;
  w->blocks_y = 50;
  w->blocks_uv = 25;
  w->pixels_y = 50 * 32;
  w->pixels_uv = 2 * 25 * 32;
  w->ref_blocks = 100;
  d.sections[AV1_PERF_INTER_OBMC_LEFT].work = *w;

  d.blocks.blocks = 3500;
  d.blocks.area = 2000 * 256 + 500 * 64 + 1000 * 256;  // 800000
  d.blocks.area_compound = 1000 * 256;                 // 32 %
  d.blocks.area_obmc = 80000;                          // 10 %
  d.blocks.area_scaled = scaled ? d.blocks.area : 0;
  d.blocks.ref_area = d.blocks.area + d.blocks.area_compound;  // 1056000
  d.blocks.ref_area_mv[0] = 264000;                            // 25 %
  d.blocks.ref_area_mv[3] = d.blocks.ref_area - 264000;
  d.blocks.ref_area_filter[0][0] = d.blocks.ref_area;
  d.blocks.ref_area_filter[1][0] = d.blocks.ref_area / 2;
  d.blocks.ref_area_filter[1][2] = d.blocks.ref_area / 2;
  return &d;
}

// A frame with an inter stage of 7.5 ms (the sections and barriers of MakeDetail) and 1 ms of reconstruction.
static av1_perf_frame MakeInterFrame(int n, double complete_ms, const av1_perf_inter_detail* detail) {
  av1_perf_frame f = MakeFrame(n, complete_ms, 1);
  f.gpu_stage_ms[AV1_PERF_GPU_INTER] = 7.5;
  f.gpu_stage_hits[AV1_PERF_GPU_INTER] = 1;
  f.gpu_stage_ms[AV1_PERF_GPU_RECON] = 1.0;
  f.gpu_stage_hits[AV1_PERF_GPU_RECON] = 1;
  f.inter = detail;
  return f;
}

int main() {
  using namespace perfstats;
  const OutputWaits no_waits = {0, 0.0, 0.0};

  // Series: empty and small inputs.
  {
    Series s;
    CHECK(s.Count() == 0);
    CHECK(Near(s.Mean(), 0.0));
    CHECK(Near(s.Percentile(50), 0.0));
    CHECK(Near(s.Percentile(95), 0.0));
    CHECK(Near(s.Max(), 0.0));
    s.Add(7.0);
    CHECK(Near(s.Mean(), 7.0));
    CHECK(Near(s.Percentile(50), 7.0));
    CHECK(Near(s.Percentile(95), 7.0));
    Series t;
    for (int i = 1; i <= 100; ++i) t.Add((double)(101 - i));  // 100..1, unsorted
    CHECK(Near(t.Mean(), 50.5));
    CHECK(Near(t.Percentile(50), 50.0));
    CHECK(Near(t.Percentile(95), 95.0));
    CHECK(Near(t.Percentile(100), 100.0));
    CHECK(Near(t.Percentile(0), 1.0));
    CHECK(Near(t.Max(), 100.0));
  }

  // Zero frames: a summary, no crash, no division by zero.
  {
    PerfStats st;
    std::vector<std::string> lines = st.Summary(no_waits);
    CHECK(!lines.empty());
    CHECK(Has(lines, "0 frames decoded"));
    CHECK(Has(lines, "no frame records"));
    CHECK(st.FrameLines().empty());
  }

  // One frame: no wall interval yet, no warm-up.
  {
    PerfStats st;
    st.AddFrame(MakeFrame(0, 1000.0, 1));
    st.AddOutput(1000.0);
    std::vector<std::string> lines = st.Summary(no_waits);
    CHECK(Has(lines, "1 frames decoded"));
    CHECK(Has(lines, "too few frames for a warm-up"));
    CHECK(st.FrameLines().size() == 1);
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
    }
  }

  // A frame without GPU timestamps.
  {
    PerfStats st;
    av1_perf_frame f = MakeFrame(0, 10.0, 1);
    f.gpu_valid = 0;
    st.AddFrame(f);
    CHECK(Has(st.Summary(no_waits), "NO GPU TIMESTAMPS"));
  }

  // A steady run: 100 decoded frames 50 ms apart, every fifth one hidden.
  {
    PerfStats st;
    CHECK(PerfStats::Warmup(100) == (size_t)kWarmupFrames);
    CHECK(PerfStats::Warmup(20) == 0);
    for (int i = 0; i < 100; ++i) {
      const int show = (i % 5) != 4;
      st.AddFrame(MakeFrame(i, 1000.0 + 50.0 * i, show));
      if (show) st.AddOutput(1000.0 + 50.0 * i);
      st.AddDecodeCall(30.0);
    }
    CHECK(st.Frames() == 100);
    CHECK(st.Outputs() == 80);
    CHECK(Near(PerfStats::Blocked(MakeFrame(1, 0.0, 1)), 5.0));
    const OutputWaits waits = {3, 120.0, 60.0};
    std::vector<std::string> lines = st.Summary(waits);
    CHECK(Has(lines, "100 frames decoded, 80 output; warm-up 10 skipped, 90 measured"));
    CHECK(Has(lines, "72 shown, 18 hidden"));
    CHECK(Has(lines, "20.0 fps decoded"));
    CHECK(Has(lines, "16.1 fps output"));  // 62.14 ms between the 70 measured output frames
    CHECK(Has(lines, "40.0 % of wall"));   // 20 ms GPU busy of 50 ms wall
    CHECK(Has(lines, "gpu intra"));
    CHECK(Has(lines, "50.0%"));  // intra is 10 of the 20 ms stage sum
    CHECK(Has(lines, "(not dispatched)"));
    CHECK(Has(lines, "output-buffer waits: 3, total 120.0 ms, longest 60.0 ms"));
    CHECK(Has(lines, "1.25 decoded frames each"));
    CHECK(Has(lines, "dispatches mean 12.0 max 12, UAV barriers mean 9.0 max 9, thread groups mean 300 max 300, "
                     "passes per group 8.0 (intra_v2 on 90 of 90 frames)"));
    // The decode thread's memsets and the clears moved to the completion thread (-asyncclear), side by side.
    CHECK(Has(lines, "cpu memsets (frame + tile)                 90    1.250    1.250    1.250    1.250   "
                     "8.0 MB cleared per frame"));
    CHECK(Has(lines, "cpu clears, completion thread              90    3.500    3.500    3.500    3.500   "
                     "6.0 MB cleared per frame"));
    CHECK(Has(lines, "gpu clears, end of the command list        90    0.500    0.500    0.500    0.500   "
                     "4.0 MB cleared per frame"));
    CHECK(Has(lines, "gpu slot_clear"));
    CHECK(Has(lines, "cleared per frame, bytes: cpu decode and tile threads 8388608, cpu completion thread 6291456, "
                     "gpu 4194304"));
    CHECK(Has(lines, "cpu async_clear"));
    CHECK(Has(lines, "cpu wait_slot"));
    CHECK(Has(st.FrameLines(), " memset=1.00 memset_tile=0.25 "));
    CHECK(Has(st.FrameLines(), " async_clear=3.50"));
    CHECK(Has(st.FrameLines(), " slot_clear=0.50 |"));
    CHECK(Has(st.FrameLines(), " | cleared memset=8388608 completion=6291456 gpu=4194304"));
    CHECK(st.FrameLines().size() == 100);
    CHECK(Has(st.FrameLines(), "iters=40 iv2=1 idisp=12 ibarr=9 igroups=300 ipasses=2400 tiles=4"));
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
      CHECK(lines[i].size() < 300);
    }
    st.Reset();
    CHECK(st.Frames() == 0 && st.Outputs() == 0);
  }

  // A stream with hidden frames that are shown later: 100 decoded frames 50 ms apart, every fifth one hidden
  // and output 5 ms later by a show_existing_frame, whose copy is a record of its own. The copies are outputs,
  // not decoded frames: they must not change the per-frame lines, and they must be in the per-OUTPUT line.
  {
    PerfStats st;
    for (int i = 0; i < 100; ++i) {
      const int show = (i % 5) != 4;
      st.AddFrame(MakeFrame(i, 1000.0 + 50.0 * i, show));
      if (show) {
        st.AddOutput(1000.0 + 50.0 * i);
      } else {
        av1_perf_frame sef;
        memset(&sef, 0, sizeof(sef));
        sef.frame_number = i;
        sef.is_sef = 1;
        sef.show_frame = 1;
        sef.intra_iters = -1;
        sef.gpu_valid = 1;
        sef.gpu_marks = 3;
        sef.gpu_stage_ms[AV1_PERF_GPU_OUTPUT_COPY] = 2.0;
        sef.gpu_stage_hits[AV1_PERF_GPU_OUTPUT_COPY] = 1;
        sef.gpu_sum_ms = 2.0;
        sef.gpu_span_ms = 2.0;
        sef.gpu_gap_ms = 1.0;
        sef.cpu_ms[AV1_PERF_CPU_FRAME] = 1.0;
        sef.cpu_ms[AV1_PERF_CPU_WAIT_SLOT] = 0.5;
        sef.complete_ms = 1005.0 + 50.0 * i;
        st.AddFrame(sef);
        st.AddOutput(1005.0 + 50.0 * i);
      }
    }
    CHECK(st.Frames() == 120);
    CHECK(st.Outputs() == 100);
    std::vector<std::string> lines = st.Summary(no_waits);
    // The first ten records are nine decoded frames and one copy.
    CHECK(Has(lines, "100 frames decoded, 100 output; warm-up 9 skipped, 91 measured"));
    CHECK(Has(lines, "72 shown, 19 hidden, 19 show-existing copies"));
    CHECK(Has(lines, "show-existing copies: 20 (19 measured): gpu busy mean 2.000 ms, cpu decode thread mean "
                     "1.000 ms, blocked mean 0.500 ms"));
    CHECK(Has(lines, "20.0 fps decoded"));  // 50 ms between decoded frames, the copies in between left out
    CHECK(Has(lines, "20.0 fps output"));
    CHECK(Has(lines, "40.0 % of wall"));
    CHECK(Has(lines, "cpu memsets (frame + tile)                 91    1.250    1.250    1.250    1.250"));
    CHECK(Has(lines, "cpu wait_slot                              91    2.000"));
    CHECK(Has(lines, "(intra_v2 on 91 of 91 frames)"));
    // One decoded frame and a fifth of a copy per output: 20 + 0.2 * 2, 25 + 0.2 * 1, 5 + 0.2 * 0.5.
    CHECK(Has(lines, "1.00 decoded frames each"));
    CHECK(Has(lines, "gpu busy 20.40 ms, cpu decode thread 25.20 ms"));
    CHECK(Has(lines, "blocked 5.10 ms"));
    CHECK(st.FrameLines().size() == 120);
    CHECK(Has(st.FrameLines(), " sef=1 "));
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
      CHECK(lines[i].size() < 300);
    }
  }

  // Only copies in the statistics: no decoded frame to divide by.
  {
    PerfStats st;
    av1_perf_frame sef;
    memset(&sef, 0, sizeof(sef));
    sef.is_sef = 1;
    sef.show_frame = 1;
    sef.intra_iters = -1;
    sef.complete_ms = 10.0;
    st.AddFrame(sef);
    st.AddOutput(10.0);
    std::vector<std::string> lines = st.Summary(no_waits);
    CHECK(Has(lines, "0 frames decoded, 1 output"));
    CHECK(Has(lines, "show-existing copies: 1 (1 measured)"));
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
    }
  }

  // Without the inter breakdown nothing of it is in the summary or in the frame lines.
  {
    PerfStats st;
    for (int i = 0; i < 100; ++i) st.AddFrame(MakeFrame(i, 1000.0 + 50.0 * i, 1));
    CHECK(!Has(st.Summary(no_waits), "inter breakdown"));
    CHECK(!Has(st.Summary(no_waits), "inter single"));
    CHECK(!Has(st.FrameLines(), " inter "));
    CHECK(st.FrameLines().size() == 100);
  }

  // The inter breakdown, section timestamps only (-gputimedetail): 100 frames, every tenth an intra frame
  // that has a reconstruction but no inter stage.
  {
    PerfStats st;
    for (int i = 0; i < 100; ++i) {
      if (i % 10 == 5) {
        av1_perf_inter_detail intra;
        memcpy(&intra, MakeDetail(1, 0), sizeof(intra));
        for (int k = 0; k < AV1_PERF_INTER_RECON; ++k) memset(&intra.sections[k], 0, sizeof(intra.sections[k]));
        memset(&intra.blocks, 0, sizeof(intra.blocks));
        intra.has_inter = 0;
        av1_perf_frame f = MakeFrame(i, 1000.0 + 50.0 * i, 1);
        f.gpu_stage_ms[AV1_PERF_GPU_RECON] = 1.0;
        f.gpu_stage_hits[AV1_PERF_GPU_RECON] = 1;
        f.inter = &intra;
        st.AddFrame(f);
      } else {
        st.AddFrame(MakeInterFrame(i, 1000.0 + 50.0 * i, MakeDetail(1, 0)));
      }
      // The record is valid during the call only: what the statistics keep is their own.
      memset(const_cast<av1_perf_inter_detail*>(MakeDetail(1, 0)), 0xff, sizeof(av1_perf_inter_detail));
      st.AddOutput(1000.0 + 50.0 * i);
    }
    std::vector<std::string> lines = st.Summary(no_waits);
    CHECK(Has(lines, "inter breakdown (-gputimedetail): 81 frames with an inter stage of 90 measured with the "
                     "breakdown, 0 of them with scaled references"));
    CHECK(Has(lines, "timestamps added per frame: mean 7.0 max 7 (no barriers added)"));
    CHECK(Has(lines, "inter counters: the dispatches and the tile threads agree on all 81 frames"));
    // Times: per frame that has the section, the average over the frames with an inter stage, the share of it.
    CHECK(Has(lines, "inter single             ovl        81    3.000    3.000    3.000    3.000    3.000   40.0%"));
    CHECK(Has(lines, "inter comp_avg           ovl        81    2.000    2.000    2.000    2.000    2.000   26.7%"));
    CHECK(Has(lines, "inter sync0              sync       81    1.000    1.000    1.000    1.000    1.000   13.3%"));
    CHECK(Has(lines, "inter pass 1 (to sync0)  excl       81    6.000    6.000    6.000    6.000    6.000   80.0%"));
    CHECK(Has(lines, "inter pass 2 (to sync1)  excl       81    0.750    0.750    0.750    0.750    0.750   10.0%"));
    CHECK(Has(lines, "inter pass 3 (to sync2)  excl       81    0.750    0.750    0.750    0.750    0.750   10.0%"));
    CHECK(Has(lines, "inter passes 1 to 3      excl       81    7.500    7.500    7.500    7.500    7.500  100.0%"));
    CHECK(Has(lines, "inter stage (gpu inter)  excl       81    7.500    7.500    7.500    7.500    7.500  100.0%"));
    // recon is a stage of its own, in the intra frames too: no share of the inter stage.
    CHECK(Has(lines, "inter recon              excl       90    1.000    1.000    1.000    1.000    1.000"));
    CHECK(!Has(lines, "inter warp "));
    // Work: 52000 threads in 813 groups (63 + 750), 99.9 % of them with work; 3 ms for 832000 samples is 3.61
    // ns each; 52000 * 264 bytes are 13.09 MB, in 3 ms 4.58 GB/s; 1812000 samples of footprint are 3.46 MB.
    CHECK(Has(lines, "inter single                 81   2.0     813   99.9     4000    2500      750    544    288   4000 "
                     "  13.09   3.46  1.73   3.61   4.58"));
    CHECK(Has(lines, "inter comp_avg               81   1.0     500  100.0     1000    1000        0    256      0   2000 "
                     "  13.18   2.02  2.02   7.81   6.91"));
    CHECK(Has(lines, "inter recon                  90   1.0       4   78.1       50"));
    CHECK(Has(lines, "reference bytes loaded 26.38 MB, footprint 5.47 MB, footprint by motion vector 3.75 MB, samples "
                     "written 2.09 MB; loaded/written 12.6, footprint/written 2.6"));
    CHECK(Has(lines, "inter blocks per frame: 3500 blocks, 0.800 Mpx luma; of that area: compound 32.0%, OBMC 10.0%, "
                     "warped 0.0%"));
    CHECK(Has(lines, "by the fraction of the motion vector: none 25.0%, horizontal only 0.0%, vertical only 0.0%, both "
                     "75.0%"));
    CHECK(Has(lines, "horizontal 8-tap 100.0%, 4-tap 0.0%, bilinear 0.0%; vertical 8-tap 50.0%, 4-tap 0.0%, bilinear "
                     "50.0%"));
    CHECK(Has(lines, "no times: run -gputimeisolate for those"));
    CHECK(Has(lines, "list single            8x8          81     1000       63   99.2"));
    CHECK(Has(lines, "list single            16x16        81     3000      750  100.0"));
    CHECK(Has(lines, "list recon             8x8          90       50        4   78.1"));
    // The stage lines are what they are without the breakdown.
    CHECK(Has(lines, "gpu inter                    81    7.500"));
    std::vector<std::string> frames = st.FrameLines();
    CHECK(frames.size() == 200);
    CHECK(Has(frames, "frame 12 inter mode=1 scaled=0 blocks=3500 p1=6.000 p2=0.750 p3=0.750 | single=3.000:813:4000 "
                      "comp_avg=2.000:500:1000 sync0=1.000 obmc_above=0.500:4:100 sync1=0.250 obmc_left=0.250:4:100 "
                      "sync2=0.500 recon=1.000:4:50"));
    CHECK(Has(frames, "frame 15 inter mode=1 scaled=0 blocks=0 p1=0.000 p2=0.000 p3=0.000 | recon=1.000:4:50"));
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
      CHECK(lines[i].size() < 300);
    }
  }

  // The same with a barrier behind every dispatch (-gputimeisolate): every time is exclusive, the dispatches
  // have times of their own, and the frames with scaled references are listed apart.
  {
    PerfStats st;
    for (int i = 0; i < 100; ++i) {
      st.AddFrame(MakeInterFrame(i, 1000.0 + 50.0 * i, MakeDetail(2, i >= 90)));
      st.AddOutput(1000.0 + 50.0 * i);
    }
    std::vector<std::string> lines = st.Summary(no_waits);
    CHECK(Has(lines, "inter breakdown (-gputimeisolate: a UAV barrier and a timestamp behind every dispatch): 90 "
                     "frames with an inter stage of 90 measured with the breakdown, 10 of them with scaled references"));
    CHECK(Has(lines, "isolate mode added per frame: UAV barriers mean 6.0 max 6, timestamps mean 9.0 max 9"));
    CHECK(Has(lines, "inter single             excl       80    3.000    3.000    3.000    3.000    2.667   35.6%"));
    CHECK(Has(lines, "inter single/scaled      excl       10    3.000    3.000    3.000    3.000    0.333    4.4%"));
    CHECK(Has(lines, "inter pass 1 (to sync0)  excl       90    6.000"));
    CHECK(Has(lines, "inter recon              excl       90    1.000    1.000    1.000    1.000    1.000"));
    CHECK(Has(lines, "list single            8x8          80     1000       63   99.2    0.5000    5.93%   7.81"));
    CHECK(Has(lines, "list single/scaled     16x16        10     3000      750  100.0    2.5000    3.70%   3.26"));
    CHECK(Has(lines, "list recon             8x8          90       50        4   78.1    1.0000    0.00% 312.50"));
    CHECK(Has(lines, "scaled reference 11.1%"));
    CHECK(Has(st.FrameLines(), "frame 95 inter mode=2 scaled=1 "));
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
      CHECK(lines[i].size() < 300);
    }
  }

  // Counters that do not fit together are reported: a section with a record more than the blocks pushed, and
  // luma that is in no section.
  {
    PerfStats st;
    for (int i = 0; i < 30; ++i) {
      av1_perf_inter_detail d;
      memcpy(&d, MakeDetail(1, 0), sizeof(d));
      if (i == 22) d.sections[AV1_PERF_INTER_COMP_AVG].records += 1;
      if (i == 25) d.blocks.area += 16;
      st.AddFrame(MakeInterFrame(i, 1000.0 + 50.0 * i, &d));
    }
    std::vector<std::string> lines = st.Summary(no_waits);
    CHECK(Has(lines, "inter counters DISAGREE on 2 of 20 frames; first: record 22, section 6: 1001 records and 256000 "
                     "samples dispatched, 1000 blocks (Y + 2 UV) and 256000 samples pushed"));
    InterFrame luma(*MakeDetail(1, 0));
    CHECK(luma.Mismatch().empty());
    luma.blocks.area += 16;
    CHECK(luma.Mismatch() == "800000 luma samples in the sections, 800016 in the inter blocks");
  }

  // A breakdown on a frame without GPU timestamps, and on too few frames for a warm-up: no division by zero.
  {
    PerfStats st;
    av1_perf_frame f = MakeInterFrame(0, 10.0, MakeDetail(1, 0));
    f.gpu_valid = 0;
    st.AddFrame(f);
    // The counters are there without timestamps, the passes have no times.
    CHECK(Has(st.Summary(no_waits), "1 frames with an inter stage of 1 measured with the breakdown"));
    CHECK(Has(st.Summary(no_waits), "inter pass 1 (to sync0)  excl        0    0.000"));
    CHECK(Has(st.Summary(no_waits), "list single            16x16         1     3000"));
    st.AddFrame(MakeInterFrame(1, 20.0, MakeDetail(2, 1)));
    std::vector<std::string> lines = st.Summary(no_waits);
    CHECK(Has(lines, "2 frames with an inter stage of 2 measured with the breakdown, 1 of them with scaled"));
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
    }
    st.Release();
    CHECK(st.Frames() == 0);
    CHECK(!Has(st.Summary(no_waits), "inter breakdown"));
  }

  // The record limit holds.
  {
    PerfStats st;
    for (int i = 0; i < kMaxFrames + 5; ++i) st.AddFrame(MakeFrame(i, (double)i, 1));
    CHECK(st.Frames() == (size_t)kMaxFrames);
    CHECK(Has(st.Summary(no_waits), "5 later frames not recorded"));
  }

  if (g_failed) {
    printf("%d check(s) failed\n", g_failed);
    return 1;
  }
  printf("perf_stats: all checks passed\n");
  return 0;
}

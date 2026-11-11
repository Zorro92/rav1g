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
  f.complete_ms = complete_ms;
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
    CHECK(st.FrameLines().size() == 100);
    for (size_t i = 0; i < lines.size(); ++i) {
      CHECK(lines[i].find("nan") == std::string::npos);
      CHECK(lines[i].find("inf") == std::string::npos);
      CHECK(lines[i].size() < 300);
    }
    st.Reset();
    CHECK(st.Frames() == 0 && st.Outputs() == 0);
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

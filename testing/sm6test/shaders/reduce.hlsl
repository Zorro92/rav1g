// Shader Model 6 test (docs/test-player.md, "Shader Model 6 test"): a reduction of the kind a decoder has,
// the sum of the 64 values of a thread group and, for every thread, the sum of the values before its own
// (exclusive prefix sum), three ways. 4 thread groups of 64, each with 64 values of its own.
//
//   REDUCE_GROUPSHARED  group memory only, as a Shader Model 5 shader does it: six steps, a group sync
//                       behind each (cs_6_0)
//   REDUCE_WAVE         WaveActiveSum and WavePrefixSum inside the wave, the sums of the waves put
//                       together through group memory behind one group sync; works with waves of any
//                       size (cs_6_0). The prefix sum is by thread only where the lanes of a wave are
//                       consecutive threads of the group in order.
//   REDUCE_WAVE_ONLY    WaveActiveSum and WavePrefixSum and nothing else, with [WaveSize(64)]: right only
//                       where the 64 threads of a group are the 64 lanes of one wave, in order (cs_6_6)
//
// Record of thread i: dst[3 * i] the prefix sum, dst[3 * i + 1] the sum, dst[3 * i + 2] the lane count
// (0 from the group memory path).

StructuredBuffer<uint> src : register(t0);
RWStructuredBuffer<uint> dst : register(u0);

#if defined(REDUCE_GROUPSHARED)

groupshared uint g_a[64];
groupshared uint g_b[64];

[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID, uint gi : SV_GroupIndex) {
  const uint v = src[tid.x];
  g_a[gi] = v;
  GroupMemoryBarrierWithGroupSync();
  g_b[gi] = g_a[gi] + (gi >= 1 ? g_a[gi - 1] : 0);
  GroupMemoryBarrierWithGroupSync();
  g_a[gi] = g_b[gi] + (gi >= 2 ? g_b[gi - 2] : 0);
  GroupMemoryBarrierWithGroupSync();
  g_b[gi] = g_a[gi] + (gi >= 4 ? g_a[gi - 4] : 0);
  GroupMemoryBarrierWithGroupSync();
  g_a[gi] = g_b[gi] + (gi >= 8 ? g_b[gi - 8] : 0);
  GroupMemoryBarrierWithGroupSync();
  g_b[gi] = g_a[gi] + (gi >= 16 ? g_a[gi - 16] : 0);
  GroupMemoryBarrierWithGroupSync();
  g_a[gi] = g_b[gi] + (gi >= 32 ? g_b[gi - 32] : 0);
  GroupMemoryBarrierWithGroupSync();
  // g_a: the sum of the values up to and including the thread's own.
  dst[tid.x * 3] = g_a[gi] - v;
  dst[tid.x * 3 + 1] = g_a[63];
  dst[tid.x * 3 + 2] = 0;
}

#elif defined(REDUCE_WAVE)

groupshared uint g_first[64];
groupshared uint g_sum[64];

[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID, uint gi : SV_GroupIndex) {
  const uint v = src[tid.x];
  const uint wave_sum = WaveActiveSum(v);
  const uint wave_prefix = WavePrefixSum(v);
  const uint first = WaveReadLaneFirst(gi);  // the first thread of this thread's wave
  g_first[gi] = first;
  g_sum[gi] = wave_sum;
  GroupMemoryBarrierWithGroupSync();
  uint before = 0;
  uint total = 0;
  for (uint j = 0; j < 64; ++j) {
    if (g_first[j] == j) {  // thread j is the first of a wave
      total += g_sum[j];
      if (j < first) before += g_sum[j];
    }
  }
  dst[tid.x * 3] = before + wave_prefix;
  dst[tid.x * 3 + 1] = total;
  dst[tid.x * 3 + 2] = WaveGetLaneCount();
}

#elif defined(REDUCE_WAVE_ONLY)

[WaveSize(64)]
[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
  const uint v = src[tid.x];
  dst[tid.x * 3] = WavePrefixSum(v);
  dst[tid.x * 3 + 1] = WaveActiveSum(v);
  dst[tid.x * 3 + 2] = WaveGetLaneCount();
}

#else
#error one of REDUCE_GROUPSHARED, REDUCE_WAVE, REDUCE_WAVE_ONLY
#endif

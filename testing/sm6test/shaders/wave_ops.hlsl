// Shader Model 6 test (docs/test-player.md, "Shader Model 6 test"): the wave intrinsics.
//
// 256 threads (4 thread groups of 64) each read one value of the input and write a record of what the
// wave intrinsics return for it. How a device packs threads into waves is its own business, so a record
// also says which wave the thread was in: its lane and the thread number of the first lane of its wave.
// The host groups the threads by that and computes every other field from the values of the group.
//
// Built as cs_6_0 (the device chooses the wave), as cs_6_6 with WAVE_SIZE=<n> ([WaveSize(n)]: the shader
// asks for waves of n lanes, and a device without them refuses the pipeline state) and as cs_6_8 with
// WAVE_MIN, WAVE_MAX and WAVE_PREF ([WaveSize(min, max, preferred)]).

#define REC 24

StructuredBuffer<uint> src : register(t0);
RWStructuredBuffer<uint> dst : register(u0);

#if defined(WAVE_SIZE)
[WaveSize(WAVE_SIZE)]
#elif defined(WAVE_MIN)
[WaveSize(WAVE_MIN, WAVE_MAX, WAVE_PREF)]
#endif
[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
  const uint id = tid.x;
  const uint v = src[id];
  const bool odd = (v & 1) != 0;
  const uint lane = WaveGetLaneIndex();
  const uint first_lane = WaveReadLaneFirst(lane);
  const uint o = id * REC;

  dst[o + 0] = WaveGetLaneCount();
  dst[o + 1] = lane;
  dst[o + 2] = WaveReadLaneFirst(id);  // names the wave
  dst[o + 3] = WaveActiveSum(v);
  dst[o + 4] = WavePrefixSum(v);
  dst[o + 5] = WaveReadLaneFirst(v);
  dst[o + 6] = WaveReadLaneAt(v, first_lane);
  dst[o + 7] = first_lane;
  const uint4 ballot = WaveActiveBallot(odd);
  dst[o + 8] = ballot.x;
  dst[o + 9] = ballot.y;
  dst[o + 10] = ballot.z;
  dst[o + 11] = ballot.w;
  dst[o + 12] = WaveActiveCountBits(odd);
  dst[o + 13] = WavePrefixCountBits(odd);
  dst[o + 14] = WaveActiveMin(v);
  dst[o + 15] = WaveActiveMax(v);
  dst[o + 16] = WaveActiveBitOr(v);
  dst[o + 17] = WaveActiveBitAnd(v);
  dst[o + 18] = WaveActiveBitXor(v);
  uint flags = 0;
  if (WaveActiveAllEqual(v)) flags |= 1;
  if (WaveActiveAnyTrue(odd)) flags |= 2;
  if (WaveActiveAllTrue(odd)) flags |= 4;
  if (WaveIsFirstLane()) flags |= 8;
  dst[o + 19] = flags;

  // Inside a branch only the lanes that took it are active.
  uint branch_sum;
  uint branch_count;
  if (odd) {
    branch_sum = WaveActiveSum(v);
    branch_count = WaveActiveCountBits(true);
  } else {
    branch_sum = WaveActiveSum(v * 3);
    branch_count = WaveActiveCountBits(true) + 1000;
  }
  dst[o + 20] = branch_sum;
  dst[o + 21] = branch_count;

  // The value of the neighbouring lane, as a filter that reads the sample next to its own would. Defined
  // where that lane is active: the host checks it for the threads whose wave has a thread in that lane.
  dst[o + 22] = WaveReadLaneAt(v, lane ^ 1);
  dst[o + 23] = 0x600D0000 | id;
}

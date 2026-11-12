// Shader Model 6 test (docs/test-player.md, "Shader Model 6 test"): 16-bit loads and stores (cs_6_2,
// -enable-16bit-types). The input is a buffer of 16-bit samples, two bytes each, as a frame buffer of
// 10-bit samples is; a thread loads four of them with one 16-bit vector load and one more with a load at
// an address that is a multiple of 2 and not of 4, and writes 16 bytes:
//
//   +0   32-bit store: the rounded average of the four samples, and the single sample in the high half
//   +4   32-bit store: first sample minus fourth, sign-extended
//   +8   16-bit stores: the average, the single sample (+10), the difference (+12), 0xA000 | thread (+14)
//
// The first two words judge the 16-bit loads alone, the last two the 16-bit stores.

ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);

[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
  const uint id = tid.x;
  const uint16_t4 s = src.Load<uint16_t4>(id * 8);
  const uint16_t t = src.Load<uint16_t>(id * 8 + 2);
  const uint16_t avg = (s.x + s.y + s.z + s.w + 2) >> 2;
  const int16_t diff = (int16_t)s.x - (int16_t)s.w;
  dst.Store<uint>(id * 16, (uint)avg | ((uint)t << 16));
  dst.Store<uint>(id * 16 + 4, (uint)(int)diff);
  dst.Store<uint16_t>(id * 16 + 8, avg);
  dst.Store<uint16_t>(id * 16 + 10, t);
  dst.Store<int16_t>(id * 16 + 12, diff);
  dst.Store<uint16_t>(id * 16 + 14, (uint16_t)(0xA000 | id));
}

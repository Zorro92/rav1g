// Shader Model 6 test (docs/test-player.md, "Shader Model 6 test"): the control. A cs_6_0 shader with nothing
// of Shader Model 6 in it but its bytecode (DXIL): when this one does not run, no DXIL shader does.
//
//   thread i of 256:  dst[i] = src[i] * 3 + i

StructuredBuffer<uint> src : register(t0);
RWStructuredBuffer<uint> dst : register(u0);

[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
  const uint id = tid.x;
  dst[id] = src[id] * 3 + id;
}

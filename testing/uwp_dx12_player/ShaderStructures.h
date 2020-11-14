#pragma once

namespace av1xboxplayer {
// Used to send per-vertex data to the vertex shader.
struct VertexPositionTex {
  DirectX::XMFLOAT3 pos;
  DirectX::XMFLOAT2 texcoord;
  // FLOAT X, Y, Z;      // position
  // DirectX::XMFLOAT4 Color;    // color
};
}  // namespace av1xboxplayer
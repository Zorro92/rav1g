/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The semi-planar output in the test player (-output p010, -interop; docs/output.md): the NV12 or P010
// textures the decoder writes its frames into, and the way back from such a texture to the three planes
// the player's md5 check and file output read.
//
// -output p010 (nv12 is the same word): the textures are D3D12 textures that shaders may write, so the
// decoder's output pass writes them directly where the device allows it. -output p010-copy: textures
// shaders may not write, which the decoder fills by one copy a plane. A frame is read back with a copy
// into the readback buffer of its image on the decoder's queue.
//
// -interop: what an application with a D3D11 video path does. The textures are created by a D3D11
// device on the adapter of the D3D12 device (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | SHARED) and opened in
// D3D12 through their shared handle; the decoder's output fence is created shareable and opened in D3D11
// (ID3D11Device5::OpenSharedFence). For every frame the D3D11 context waits for the frame's value of the
// fence (ID3D11DeviceContext4::Wait), and the frame is read from the D3D11 texture. Every step says in the
// log ("INTEROP ...") what it did, and with what result where it failed.

#ifndef PLAYER_COMMON_SEMI_PLANAR_H_
#define PLAYER_COMMON_SEMI_PLANAR_H_

#include <stdint.h>
#include <vector>
#include <wrl/client.h>
#include <d3d12.h>
#include <d3d11_4.h>

#include "common/frame_outs.h"
#include "rav1g.h"

class SemiPlanarOutput : public SemiPlanarTextures {
 public:
  // mode: 1 textures shaders may write, 2 textures they may not. queue: the queue the decoder submits to.
  SemiPlanarOutput(ID3D12Device* device, ID3D12CommandQueue* queue, int mode, bool interop);
  ~SemiPlanarOutput();

  // The texture of an image of the pool (SemiPlanarTextures).
  HRESULT Create(OutBuffer* buffer, uint32_t width, uint32_t height, frame_buffer_type fb_type,
                 uint64_t* bytes) override;

  // The output fence of the decoder that was just created (rav1g_decoder_output_fence). With -interop it
  // is opened in D3D11. False when that failed.
  bool SetFence(ID3D12Fence* fence);

  // A frame the decoder has handed over, in the image `buffer`. With -interop the D3D11 context waits for
  // the frame's value of the fence. With `read` the frame is read back from its texture and `planar` is the
  // frame with three planes of its own samples, as the planar output has them; it is valid until the next
  // call. False when a step failed (the log says which).
  bool Frame(const Rav1gFrame* frame, OutBuffer* buffer, bool read, Rav1gFrame* planar);

  // One line about what the task did, for the end of the task.
  void Report();

 private:
  bool CreateD3D11();
  bool ReadD3D12(const Rav1gFrame* frame, OutBuffer* buffer);
  bool ReadD3D11(const Rav1gFrame* frame, OutBuffer* buffer, Rav1gFrame* planar);
  void ToPlanes(const Rav1gFrame* frame, const uint8_t* luma, size_t luma_pitch, const uint8_t* chroma,
                size_t chroma_pitch, Rav1gFrame* planar);

  Microsoft::WRL::ComPtr<ID3D12Device> device_;
  Microsoft::WRL::ComPtr<ID3D12CommandQueue> queue_;
  const int mode_;
  const bool interop_;
  // The readback on D3D12.
  Microsoft::WRL::ComPtr<ID3D12CommandAllocator> allocator_;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> list_;
  Microsoft::WRL::ComPtr<ID3D12Fence> read_fence_;
  uint64_t read_value_ = 0;
  HANDLE read_event_ = NULL;
  // -interop: the D3D11 device, its context, the decoder's fence as D3D11 has it, a staging texture.
  Microsoft::WRL::ComPtr<ID3D11Device5> device11_;
  Microsoft::WRL::ComPtr<ID3D11DeviceContext4> context11_;
  Microsoft::WRL::ComPtr<ID3D11Fence> fence11_;
  Microsoft::WRL::ComPtr<ID3D11Texture2D> staging_;
  D3D11_TEXTURE2D_DESC staging_desc_ = {};
  bool d3d11_tried_ = false;
  bool logged_texture_ = false;
  std::vector<uint8_t> planes_;
  // For the report.
  uint64_t frames_ = 0;
  uint64_t waited_ = 0;
  uint64_t read_ = 0;
  uint64_t failed_ = 0;
};

#endif  // PLAYER_COMMON_SEMI_PLANAR_H_

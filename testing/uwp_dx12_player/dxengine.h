/*
 * Copyright 2020 Google LLC
 *
 */

/*
 * Copyright (c) 2020, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

#pragma once
#include "pch.h"

#include <mutex>
#include "common\log.h"
#include <memory>

#include <d3d11on12.h>
#include <d2d1_3.h>
#include <dwrite.h>

using namespace DirectX;

// Note that while ComPtr is used to manage the lifetime of resources on the CPU,
// it has no understanding of the lifetime of resources on the GPU. Apps must account
// for the GPU lifetime of resources to avoid destroying objects that may still be
// referenced by the GPU.
// An example of this can be found in the class method: OnDestroy().
using Microsoft::WRL::ComPtr;

#define CHECK_HRESULT(hr)                  \
  {                                        \
    HRESULT _hr_ = hr;                     \
    if (FAILED(_hr_)) {                    \
      XB_LOGE << "DirectX error "          \
              << "0x" << std::hex << _hr_; \
      return _hr_;                         \
    }                                      \
  }

#define CHECK_HRESULT_0(hr)                \
  {                                        \
    HRESULT _hr_ = hr;                     \
    if (FAILED(_hr_)) {                    \
      XB_LOGE << "DirectX error "          \
              << "0x" << std::hex << _hr_; \
    }                                      \
  }

#define SET_D3D12_NAME(x) x->SetName(L#x);
#define SET_D3D12_NAME_ARR(x, i)           \
  {                                        \
    wchar_t ss[64];                        \
    swprintf_s(ss, 64, L"%s[%d]", L#x, i); \
    x[i]->SetName(ss);                     \
  }

typedef enum { ftNone = -1, ft8bit = 0, ft10bit, ft10x3 } frame_type;

struct OneRenderTask {
  ComPtr<ID3D12Resource> textures[3];
  uint32_t render_width;
  uint32_t render_height;
  frame_type type;
  int shown;
  std::wstring screen_msg;
};

class DXEngine
{
 public:
  struct fRect {
    float left;
    float top;
    float right;
    float bottom;
  };
  DXEngine(Windows::UI::Core::CoreWindow ^ window);

  int OnInit();
  void OnUpdate();
  void OnRender(std::shared_ptr<OneRenderTask> task);
  void OnDestroy();
  void OnResize();
  int Ready() { return ready_; }

  ID3D12Device* Device() { return m_device.Get(); }

 private:
  void GetHardwareAdapter(IDXGIFactory2* pFactory, IDXGIAdapter1** ppAdapter);
  void RenderUI(std::shared_ptr<OneRenderTask> task);
  void UpdateRenderTargetSize();

 private:
  static const UINT FrameCount = 4;
  static const UINT TextureWidth = 256;
  static const UINT TextureHeight = 256;
  static const UINT TexturePixelSize = 4;  // The number of bytes used to represent a pixel in the texture.

  BOOL first_time_ = 1;

  struct Vertex {
    XMFLOAT3 position;
    XMFLOAT2 uv;
  };

  // Pipeline objects.
  CD3DX12_VIEWPORT m_viewport;
  CD3DX12_RECT m_scissorRect;
  ComPtr<IDXGISwapChain3> m_swapChain;
  ComPtr<ID3D12Device> m_device;
  ComPtr<ID3D12Resource> m_renderTargets[FrameCount];
  ComPtr<ID3D12CommandAllocator> m_commandAllocator;
  ComPtr<ID3D12CommandQueue> m_commandQueue;
  ComPtr<ID3D12RootSignature> m_rootSignature;
  ComPtr<ID3D12DescriptorHeap> m_rtvHeap;
  ComPtr<ID3D12DescriptorHeap> m_srvHeap;
  ComPtr<ID3D12PipelineState> m_pipelineState;
  ComPtr<ID3D12GraphicsCommandList> m_commandList;
  UINT m_rtvDescriptorSize;
  UINT m_srvDescriptorSize;

  // App resources.
  ComPtr<ID3D12Resource> m_vertexBuffer;
  D3D12_VERTEX_BUFFER_VIEW m_vertexBufferView;

  ComPtr<ID3D12Resource> textureUploadHeap_[3];
  ComPtr<ID3D12Resource> textureDefaultHeap_[3];

  ComPtr<ID3D12Resource> textures_[3];

  ComPtr<ID3D12Resource> vertexUploadBuffer_;
  ComPtr<ID3D12Resource> downloadBuffer_;
  ComPtr<ID3D12Resource> hdr_buffer_;

  ComPtr<ID3D11On12Device> d11on12device_;
  ComPtr<ID3D11DeviceContext> d11on12device_context_;
  ComPtr<ID2D1Factory3> d2d1factory_;
  ComPtr<IDWriteFactory> d2writeFactory_;
  ComPtr<ID2D1Device2> d2d1device2_;
  ComPtr<ID2D1DeviceContext2> d2d1deviceContext_;
  ComPtr<ID3D11Resource> wrappedBackBuffers_[FrameCount];
  ComPtr<ID2D1Bitmap1> d2d1renderTargets_[FrameCount];

  ComPtr<ID2D1SolidColorBrush> textBrush_;
  ComPtr<IDWriteTextFormat> textFormat_;

  // Synchronization objects.
  UINT m_frameIndex;
  HANDLE m_fenceEvent;
  ComPtr<ID3D12Fence> m_fence;
  UINT64 m_fenceValue;

  int LoadPipeline();
  int LoadAssets();
  void InternalRender(BOOL reset, std::shared_ptr<OneRenderTask> task);
  void setVertexes(Vertex* vertexes, UINT size);
  void setVertexes();
  void PopulateCommandList(BOOL reset, std::shared_ptr<OneRenderTask> task);
  void WaitForPreviousFrame();

  BOOL hasTexture_ = FALSE;
  std::mutex cs_render_;
  unsigned char color_offset_ = 0;
  UINT renderWidth_ = 0;
  UINT renderHeight_ = 0;
  frame_type renderType_ = ftNone;

  UINT strides_[3];
  int upload_buffer_size_[3];
  UINT download_buffer_size_ = 0;
  UINT m_width;
  UINT m_height;
  bool m_useWarpDevice = false;
  int ready_ = 0;
  Platform::Agile<Windows::UI::Core::CoreWindow> m_window;
  Windows::Foundation::Size m_logicalSize;
  float m_dpi;
  float m_effectiveDpi;
  std::mutex cs_;
};

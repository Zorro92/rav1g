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

#include "frame_outs.h"
#include <assert.h>
#include <chrono>
#include "d3dx12.h"

#if defined(_DEBUG)
inline void SetName(ID3D12Object* pObject, LPCWSTR name) { pObject->SetName(name); }
inline void SetNameIndexed(ID3D12Object* pObject, LPCWSTR name, UINT index) {
  WCHAR fullName[50];
  if (swprintf_s(fullName, L"%s[%u]", name, index) > 0) {
    pObject->SetName(fullName);
  }
}

#define NAME_D3D12_OBJECT(x) SetName(x.Get(), L#x)
#define NAME_D3D12_OBJECT_INDEXED(x, n) SetNameIndexed(x[n].Get(), L#x, n)
#else
#define NAME_D3D12_OBJECT(x)
#define NAME_D3D12_OBJECT_INDEXED(x, n)
#endif

OutBufferWrapper::~OutBufferWrapper() {
  if (storage_ && buffer_) {
    XB_LOGD << "Release OutBuffer " << buffer_;
    storage_->Push(buffer_);
  }
}

void OutBufferWrapper::ReleaseRented() {
  if (storage_ && buffer_) {
    storage_->ReleaseRentedBuffer(buffer_);
  }
}

OutBufferStorage::OutBufferStorage(int queue_size, ID3D12Device* dx12_device)
    : storage_(queue_size), queue_free_(queue_size), queue_size_(queue_size), dx12_device_(dx12_device) {
  for (int i = 0; i < queue_size_; i++) {
    OutBuffer& item = storage_[i];
    item.shown = 0;
    queue_free_.Push(&item);
  }
}

void OutBufferStorage::Push(OutBuffer* frame) { queue_free_.Push(frame); }

std::shared_ptr<OutBufferWrapper> OutBufferStorage::GetFreeBuffer(uint32_t width, uint32_t height,
                                                                  frame_buffer_type fb_type, size_t buffer_size,
                                                                  uint32_t flags) {
  HRESULT hr;
  int has_errors = 0;
  int wait_no = 0;
  std::chrono::steady_clock::time_point wait_start;
  if (wait_logging_ && queue_free_.IsEmpty()) {
    size_t rented = 0;
    {
      std::unique_lock<std::mutex> lock(cs_);
      wait_no = ++wait_count_;
      rented = rented_.size();
    }
    wait_start = std::chrono::steady_clock::now();
    // "held by decoder" buffers are rented and not yet handed back as a ready frame
    // (frames in flight and hidden frames); the rest sit in the player's display queue.
    XB_LOGI << "GPUTIME output-buffer wait #" << wait_no << " BLOCKED: pool " << queue_size_
            << ", free 0, held by decoder " << rented << ", queued or on display "
            << (queue_size_ - (int)rented);
  }
  queue_free_.DoForAll(1, [&](OutBuffer* buffer) {
    if (flags & CBUFTEXTURE) {
      if (buffer->renderWidth != width || buffer->renderHeight != height || buffer->fb_type != fb_type) {
        for (int i = 0; i < 3; i++) {
          const int subsampling = i > 0;
          const int _width = (fb_type == fbt10x3) ? (((width + subsampling) >> subsampling) + 2) / 3
                                                  : ((width + subsampling) >> subsampling);
          const int _height = (height + subsampling) >> subsampling;

          // Create interop resources
          D3D12_RESOURCE_DESC textureDesc = {};
          textureDesc.MipLevels = 1;
          textureDesc.Format = (fb_type == fbt10bit)
                                   ? DXGI_FORMAT_R16_UNORM
                                   : ((fb_type == fbt8bit) ? DXGI_FORMAT_R8_UNORM : DXGI_FORMAT_R10G10B10A2_UNORM);
          textureDesc.Width = _width;
          textureDesc.Height = _height;
          textureDesc.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
          textureDesc.DepthOrArraySize = 1;
          textureDesc.SampleDesc.Count = 1;
          textureDesc.SampleDesc.Quality = 0;
          textureDesc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
          textureDesc.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
          hr = dx12_device_->CreateCommittedResource(
              &CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_DEFAULT), D3D12_HEAP_FLAG_SHARED, &textureDesc,
              D3D12_RESOURCE_STATE_COPY_DEST, 0,
              IID_PPV_ARGS(&buffer->d3d12textures[i]));
          if (FAILED(hr)) {
            has_errors = 1;
            return 0;
          }
          NAME_D3D12_OBJECT_INDEXED(buffer->d3d12textures, i);
        }
        buffer->renderWidth = width;
        buffer->renderHeight = height;
        buffer->fb_type = fb_type;
      }
    }
    if (flags & CBUFREADBACK) {
      if (buffer->buffer_size < buffer_size) {
        // Create readback buffer (if any)
        const D3D12_HEAP_TYPE heapType = D3D12_HEAP_TYPE_READBACK;
        const D3D12_RESOURCE_STATES state = D3D12_RESOURCE_STATE_COPY_DEST;
        D3D12_HEAP_PROPERTIES heapProp;
        heapProp.Type = heapType;
        heapProp.CPUPageProperty = D3D12_CPU_PAGE_PROPERTY_UNKNOWN;  //
        heapProp.MemoryPoolPreference = D3D12_MEMORY_POOL_UNKNOWN;
        heapProp.CreationNodeMask = 0;
        heapProp.VisibleNodeMask = 0;

        const D3D12_RESOURCE_FLAGS flags = D3D12_RESOURCE_FLAG_NONE;
        hr = dx12_device_->CreateCommittedResource(&heapProp, D3D12_HEAP_FLAG_NONE,
                                                   &CD3DX12_RESOURCE_DESC::Buffer(buffer_size, flags), state, NULL,
                                                   IID_PPV_ARGS(&buffer->d3d12buffer));

        if (SUCCEEDED(hr)) {
          hr = buffer->d3d12buffer->Map(0, NULL, &buffer->pMem);
          if (FAILED(hr)) {
            has_errors = 1;
            return 0;
          }
          buffer->d3d12buffer->Unmap(0, 0);
        }
        buffer->buffer_size = buffer_size;
      }
    }
    return 1;
  });
  if (has_errors) {
    XB_LOGE << "Failed to allocate output buffers.";
    queue_free_.DoForAll(1, [&](OutBuffer* buffer) {
      buffer->Reset();
      return 1;
    });
    return 0;
  }
  OutBuffer* buffer = queue_free_.Pull();
  if (wait_no) {
    const double waited_ms =
        std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - wait_start).count();
    {
      std::unique_lock<std::mutex> lock(cs_);
      wait_total_ms_ += waited_ms;
      if (waited_ms > wait_max_ms_) wait_max_ms_ = waited_ms;
    }
    XB_LOGI << "GPUTIME output-buffer wait #" << wait_no << " resumed after " << waited_ms << " ms";
  }
  if (!(flags & CBUFREADBACK)) {
    buffer->d3d12buffer.Reset();
    buffer->pMem = 0;
  }
  std::unique_lock<std::mutex> lock(cs_);
  OutBufferWrapper* bf = new (std::nothrow) OutBufferWrapper(buffer, this);
  if (!bf) return 0;
  std::shared_ptr<OutBufferWrapper> retbuf = std::shared_ptr<OutBufferWrapper>(bf);
  rented_.push_back(retbuf);
  return retbuf;
}

std::shared_ptr<OutBufferWrapper> OutBufferStorage::ReleaseRentedBuffer(void* ptr) {
  std::unique_lock<std::mutex> lock(cs_);
  for (auto iter = rented_.begin(); iter != rented_.end(); iter++) {
    if ((*iter)->Buffer() == ptr) {
      XB_LOGD << "ReleaseRentedBuffer " << ptr;
      std::shared_ptr<OutBufferWrapper> buff = *iter;
      rented_.erase(iter);
      return buff;
    }
  }
  assert(0 && "buffer not found");
  return 0;
}

std::shared_ptr<OutBufferWrapper> OutBufferStorage::FindRentedBuffer(void* ptr) {
  std::unique_lock<std::mutex> lock(cs_);
  for (auto iter = rented_.begin(); iter != rented_.end(); iter++) {
    if ((*iter)->Buffer() == ptr) {
      std::shared_ptr<OutBufferWrapper> buff = *iter;
      return buff;
    }
  }
  assert(0 && "buffer not found");
  return 0;
}

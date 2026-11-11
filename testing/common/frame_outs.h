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
#include "frame_queue.h"
#include "log.h"
#include <vector>
#include <list>
#include <mutex>
#include <memory>
#include <atomic>
#include "aom/aom_decoder.h"

#include <wrl.h>
#include <wrl/client.h>
#include <d3d12.h>
#include <assert.h>

using namespace Microsoft::WRL;

#define MEM_BLOCK_SIZE 65536
#define TEXTURE_MEM_ALIGNMENT 256
#define TEXTURE_MEM_ALIGNMENT_1 (TEXTURE_MEM_ALIGNMENT - 1)
#define TEXTURE_MEM_ALIGNMENT_SHIFT 8
#define MEM_ALIGN_64K (1 << 16)

typedef struct OutBuffer {
  void* pMem;
  size_t memSize;
  uint32_t frameWidth;
  uint32_t frameHeight;

  uint32_t renderWidth;
  uint32_t renderHeight;

  uint8_t* planes[3];
  uint32_t strides[3];
  frame_buffer_type fb_type;
  ComPtr<ID3D12Resource> d3d12buffer;
  ComPtr<ID3D12Resource> d3d12textures[3];
  size_t buffer_size;

  uint64_t pts;
  uint32_t frame_no;
  uint32_t frame_no_offset;
  uint32_t shown;
  OutBuffer() { memset(this, 0, sizeof(OutBuffer)); }
  ~OutBuffer() {}
  void Reset() {
    for (int i = 0; i < 3; i++) {
      d3d12textures[i].Reset();
      d3d12buffer.Reset();
      planes[i] = 0;
      strides[i] = 0;
    }
    pMem = 0;
    memSize = 0;
    renderWidth = 0;
    renderHeight = 0;
    frameWidth = 0;
    frameHeight = 0;
    fb_type = fbtNone;
  }
} OutBuffer;

class OutBufferStorage;
class OutBufferWrapper {
 public:
  OutBufferWrapper(OutBuffer* buffer, OutBufferStorage* storage) : buffer_(buffer), storage_(storage) {}
  ~OutBufferWrapper();
  OutBuffer* Buffer() { return buffer_; }
  uint32_t Shown() { return buffer_->shown; }
  void SetShown(uint32_t val) { buffer_->shown = val; }
  void ReleaseRented();

 private:
  OutBuffer* buffer_;
  OutBufferStorage* storage_;
};

typedef enum { CBUFREADBACK = 0x1, CBUFTEXTURE = 0x2, CBUFALL = 0x3 } cbufFlags;

class OutBufferStorage {
 public:
  OutBufferStorage(int queue_size, ID3D12Device* dx12_device);
  ~OutBufferStorage() {
    if (queue_free_.Size() != queue_size_) {
      XB_LOGE << "OutBufferStorage still has unreleashed buffers!! " << (queue_size_ - queue_free_.Size());
      rented_.clear();
      XB_LOGE << "Release it anyway";
    }
  }

  size_t GetSize() { return queue_free_.Size(); }
  // Bytes of the textures the pool has created so far (it creates them on the first frame of a size).
  uint64_t TextureBytes() const { return texture_bytes_total_.load(); }
  std::shared_ptr<OutBufferWrapper> GetFreeBuffer(uint32_t width, uint32_t height, frame_buffer_type fb_type,
                                                  size_t buffer_size, uint32_t flags);
  std::shared_ptr<OutBufferWrapper> ReleaseRentedBuffer(void* ptr);
  std::shared_ptr<OutBufferWrapper> FindRentedBuffer(void* ptr);

  // Timing runs: log and count every time GetFreeBuffer finds the pool empty.
  void SetWaitLogging(bool on) { wait_logging_ = on; }
  void ResetWaitStats() {
    std::unique_lock<std::mutex> lock(cs_);
    wait_count_ = 0;
    wait_total_ms_ = 0.0;
    wait_max_ms_ = 0.0;
  }
  void GetWaitStats(int* count, double* total_ms, double* max_ms) {
    std::unique_lock<std::mutex> lock(cs_);
    *count = wait_count_;
    *total_ms = wait_total_ms_;
    *max_ms = wait_max_ms_;
  }

 private:
  void Push(OutBuffer* frame);
  friend class OutBufferWrapper;

 private:
  std::vector<OutBuffer> storage_;
  std::list<std::shared_ptr<OutBufferWrapper> > rented_;
  myqueue<OutBuffer*> queue_free_;
  int queue_size_;
  std::mutex cs_;
  ComPtr<ID3D12Device> dx12_device_;
  std::atomic<uint64_t> texture_bytes_total_{0};
  bool wait_logging_ = false;
  int wait_count_ = 0;
  double wait_total_ms_ = 0.0;
  double wait_max_ms_ = 0.0;
};

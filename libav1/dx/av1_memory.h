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
#include "dx\vp9dx_linear_allocator.h"
#include <d3d12.h>
#include <wrl.h>
#include <dxgi1_4.h>
#include <d3dx12.h>

enum {
  DevMemPoolSize = 128,
  HostMemPoolSize = 128,
  ShaderPoolSize = 80,
};

enum BufferTypes {
  ByteBuffer,
  StructuredBuffer,
  ConstBuffer,
  BufferUnknown,
};

enum MemoryType {
  DeviceOnly = 0,
  DeviceUpload = 1,
  HostRW = 2,
  ReadBack = 3,
  DeviceOnlyConst = 4,
  UploadUAV = 5,
};

using Microsoft::WRL::ComPtr;

struct GpuBufferObject {
  ID3D12Resource* dev;
  void* host_ptr;
  size_t size;
  size_t max_size;
  MemoryType memtype;
  const char* name;
};

struct dx_compute_context {
  Microsoft::WRL::ComPtr<ID3D12Device> device;
  Microsoft::WRL::ComPtr<ID3D12CommandQueue> queue;
  Microsoft::WRL::ComPtr<ID3D12CommandQueue> queue_direct;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list;
};

class av1_memory_manager_base {
 public:
  virtual GpuBufferObject* create_buffer(size_t size, MemoryType mem) = 0;
  virtual void* host_allocate(size_t size, int align = 0) = 0;
};

struct av1_memory_allocator : public av1_memory_manager_base {
 public:
  av1_memory_allocator() {}
  void setup(uint8_t* host_mem, size_t host_size);
  void set_dx_context(dx_compute_context* ctx) { context = ctx; }
  virtual GpuBufferObject* create_buffer(size_t size, MemoryType mem);
  virtual void* host_allocate(size_t size, int align = 0);
  void release();

 private:
  dx_compute_context* context;
  GpuBufferObject dev_buffer_pool[DevMemPoolSize];

  int shader_obj_count;
  int host_obj_count;
  int dev_obj_count;
  uint8_t* host_base_addr;
  size_t host_max_size;
  size_t host_offset;
};

struct av1_memory_allocator_dummy : public av1_memory_manager_base {
 public:
  av1_memory_allocator_dummy() : host_ptr(0), device_ptr(0) {}
  virtual GpuBufferObject* create_buffer(size_t size, MemoryType mem);
  virtual void* host_allocate(size_t size, int align = 0);
  size_t get_host_size() { return host_ptr; }
  size_t get_device_size() { return device_ptr; }

 private:
  size_t host_ptr;
  size_t device_ptr;
};

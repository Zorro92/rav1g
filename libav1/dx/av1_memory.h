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
  MemoryTypeCount = 6,
};

using Microsoft::WRL::ComPtr;

struct GpuBufferObject {
  ID3D12Resource* dev;
  void* host_ptr;
  size_t size;
  size_t max_size;
  MemoryType memtype;
  const char* name;  // what the buffer is, for the MEMORY log lines
  int slot;          // pipeline slot that owns it, -1: shared by all slots
};

struct dx_compute_context {
  Microsoft::WRL::ComPtr<ID3D12Device> device;
  Microsoft::WRL::ComPtr<ID3D12CommandQueue> queue;
  Microsoft::WRL::ComPtr<ID3D12CommandQueue> queue_direct;
  Microsoft::WRL::ComPtr<ID3D12GraphicsCommandList> command_list;
};

// One line to the log callback of av1_set_log_callback() (the debugger output without one). printf format,
// no line end. The lines about memory start with "MEMORY ".
void av1_log(int is_error, const char* format, ...);
// The same, and the line is kept as the reason the decoder creation failed (av1_last_create_error()). Only
// the first reason of a creation is kept: it is the allocation that failed, later ones are consequences.
void av1_log_create_error(const char* format, ...);
void av1_clear_create_error();
// -poisonbuf (av1_set_poison_buffers(AV1_POISON_BUFFERS), dx/av1_perf.h): fills the constants a stage is
// about to write (ComputeCommandBuffer::Alloc) with the byte 0xA5. Does nothing otherwise.
void av1_poison_constants(void* ptr, size_t size);
const char* av1_memory_type_name(MemoryType mem);

// Video memory budget and usage of the adapter the device is on (IDXGIAdapter3::QueryVideoMemoryInfo).
struct av1_video_memory {
  HRESULT hr_adapter;    // getting the adapter of the device
  HRESULT hr_local;      // DXGI_MEMORY_SEGMENT_GROUP_LOCAL
  HRESULT hr_non_local;  // DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL
  DXGI_QUERY_VIDEO_MEMORY_INFO local;
  DXGI_QUERY_VIDEO_MEMORY_INFO non_local;
};
void av1_query_video_memory(ID3D12Device* device, av1_video_memory* info);
// Logs it as one "MEMORY dxgi <label>: ..." line; says so when the query failed.
void av1_log_video_memory(int is_error, const char* label, const av1_video_memory* info);

class av1_memory_manager_base {
 public:
  virtual GpuBufferObject* create_buffer(size_t size, MemoryType mem, const char* name = 0, int slot = -1) = 0;
  virtual void* host_allocate(size_t size, int align = 0) = 0;
};

struct av1_memory_allocator : public av1_memory_manager_base {
 public:
  av1_memory_allocator() {}
  void setup(uint8_t* host_mem, size_t host_size);
  void set_dx_context(dx_compute_context* ctx) { context = ctx; }
  // Both return NULL when the allocation fails, after logging a "MEMORY FAILED" line that names it.
  virtual GpuBufferObject* create_buffer(size_t size, MemoryType mem, const char* name = 0, int slot = -1);
  virtual void* host_allocate(size_t size, int align = 0);
  // Releases every buffer. The descriptors are free again afterwards.
  void release();
  // Releases the buffers of one memory type and returns their bytes (and their number in |count|). For a
  // release that is logged type by type; release() must follow it before the allocator is used again.
  size_t release_type(MemoryType mem, int* count);

  // What was allocated so far, for the MEMORY log lines.
  int buffer_count() const { return dev_obj_count; }
  const GpuBufferObject* buffer(int index) const { return &dev_buffer_pool[index]; }
  size_t device_bytes() const;
  size_t device_bytes(MemoryType mem) const { return dev_bytes[mem]; }
  int device_buffers(MemoryType mem) const { return dev_count[mem]; }
  size_t host_bytes_used() const { return host_offset; }
  size_t host_bytes_total() const { return host_max_size; }

 private:
  dx_compute_context* context;
  GpuBufferObject dev_buffer_pool[DevMemPoolSize];

  int shader_obj_count;
  int host_obj_count;
  int dev_obj_count;
  size_t dev_bytes[MemoryTypeCount];
  int dev_count[MemoryTypeCount];
  uint8_t* host_base_addr;
  size_t host_max_size;
  size_t host_offset;
};

struct av1_memory_allocator_dummy : public av1_memory_manager_base {
 public:
  av1_memory_allocator_dummy() : host_ptr(0), device_ptr(0) {}
  virtual GpuBufferObject* create_buffer(size_t size, MemoryType mem, const char* name = 0, int slot = -1);
  virtual void* host_allocate(size_t size, int align = 0);
  size_t get_host_size() { return host_ptr; }
  size_t get_device_size() { return device_ptr; }

 private:
  size_t host_ptr;
  size_t device_ptr;
};

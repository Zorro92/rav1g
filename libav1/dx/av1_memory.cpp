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

#include "dx/av1_memory.h"
#include "aom_dsp/aom_dsp_common.h"
#include <d3d12.h>
#include <assert.h>

enum {
  HostMemAlign = 32,
  DeviceMemAlign = 256,
  HostMemAlign1 = HostMemAlign - 1,
  DeviceMemAlign1 = DeviceMemAlign - 1,
};

void av1_memory_allocator::setup(uint8_t *host_mem, size_t host_size) {
  size_t haddr = (size_t)host_mem;
  size_t hoffset = ((haddr + 255) & ~255) - haddr;
  host_base_addr = host_mem + hoffset;
  host_max_size = host_size - hoffset;
  host_offset = 0;
  shader_obj_count = 0;
  host_obj_count = 0;
  dev_obj_count = 0;
  memset(dev_buffer_pool, 0, sizeof(dev_buffer_pool));
}

HRESULT upload_helper(dx_compute_context *context, ID3D12Resource *dst, void *init, size_t size, size_t src_size) {
  Microsoft::WRL::ComPtr<ID3D12Device> device = context->device;
  ComPtr<ID3D12Resource> res;
  HRESULT hr = device->CreateCommittedResource(&CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_UPLOAD), D3D12_HEAP_FLAG_NONE,
                                               &CD3DX12_RESOURCE_DESC::Buffer(size), D3D12_RESOURCE_STATE_GENERIC_READ,
                                               NULL, IID_PPV_ARGS(&res));

  if (FAILED(hr)) return hr;

  void *ptr = NULL;
  hr = res->Map(0, &CD3DX12_RANGE(0, 0), &ptr);
  if (FAILED(hr)) return hr;

  memcpy(ptr, init, src_size);
  res->Unmap(0, NULL);

  ComPtr<ID3D12CommandAllocator> computeAllocator;
  hr = device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&computeAllocator));
  if (FAILED(hr)) return hr;

  ComPtr<ID3D12GraphicsCommandList> clist;
  hr = device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, computeAllocator.Get(), NULL, IID_PPV_ARGS(&clist));
  if (FAILED(hr)) return hr;

  clist->CopyResource(dst, res.Get());
  clist->ResourceBarrier(
      1, &CD3DX12_RESOURCE_BARRIER::Transition(dst, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_GENERIC_READ));
  clist->Close();

  ID3D12CommandList *cl[] = {clist.Get()};
  Microsoft::WRL::ComPtr<ID3D12CommandQueue> queue = context->queue_direct;
  queue->ExecuteCommandLists(1, cl);

  ComPtr<ID3D12Fence> fence;
  hr = device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence));
  if (FAILED(hr)) return hr;

  queue->Signal(fence.Get(), 1);
  HANDLE event = CreateEvent(nullptr, false, false, nullptr);
  fence->SetEventOnCompletion(1, event);

  WaitForSingleObject(event, INFINITE);
  return hr;
}

GpuBufferObject *av1_memory_allocator::create_buffer(size_t size, MemoryType mem) {
  assert(dev_obj_count < DevMemPoolSize);
  GpuBufferObject *obj = &dev_buffer_pool[dev_obj_count++];
  obj->host_ptr = NULL;
  obj->memtype = mem;
  obj->size = (size + DeviceMemAlign1) & (~(DeviceMemAlign1));

  D3D12_HEAP_TYPE heapLut[] = {D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_TYPE_UPLOAD, D3D12_HEAP_TYPE_CUSTOM,
                               D3D12_HEAP_TYPE_READBACK, D3D12_HEAP_TYPE_DEFAULT};
  const D3D12_HEAP_TYPE heapType = heapLut[mem];
  const D3D12_RESOURCE_STATES state = (mem == DeviceOnly) ? D3D12_RESOURCE_STATE_UNORDERED_ACCESS
                                                          : (mem == DeviceOnlyConst)
                                                                ? D3D12_RESOURCE_STATE_COPY_DEST
                                                                : (mem == ReadBack) ? D3D12_RESOURCE_STATE_COPY_DEST
                                                                                    : D3D12_RESOURCE_STATE_GENERIC_READ;

  D3D12_HEAP_PROPERTIES heapProp;
  heapProp.Type = heapType;
  heapProp.CPUPageProperty = mem == HostRW ? D3D12_CPU_PAGE_PROPERTY_WRITE_BACK : D3D12_CPU_PAGE_PROPERTY_UNKNOWN;  //
  heapProp.MemoryPoolPreference = mem == HostRW ? D3D12_MEMORY_POOL_L0 : D3D12_MEMORY_POOL_UNKNOWN;
  heapProp.CreationNodeMask = 0;
  heapProp.VisibleNodeMask = 0;

  const D3D12_RESOURCE_FLAGS flags =
      (mem == DeviceOnly) ? D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS : D3D12_RESOURCE_FLAG_NONE;
  HRESULT hr = context->device->CreateCommittedResource(&heapProp, D3D12_HEAP_FLAG_NONE,
                                                        &CD3DX12_RESOURCE_DESC::Buffer(obj->size, flags), state, NULL,
                                                        __uuidof(*obj->dev), reinterpret_cast<void **>(&obj->dev));

  if (SUCCEEDED(hr) && mem != DeviceOnly && mem != DeviceOnlyConst) {
    hr = obj->dev->Map(0, &CD3DX12_RANGE(0, 0), &obj->host_ptr);
  }

  return SUCCEEDED(hr) ? obj : NULL;
}

void *av1_memory_allocator::host_allocate(size_t size, int align) {
  align = AOMMAX(align, HostMemAlign) - 1;
  size_t addr_offset = ((host_offset + align) & ~align) - host_offset;
  size += addr_offset;
  size = (size + HostMemAlign1) & (~HostMemAlign1);
  if ((host_offset + size) > host_max_size) return NULL;
  uint8_t *ptr = host_base_addr + host_offset + addr_offset;
  host_offset += size;
  return ptr;
}

void av1_memory_allocator::release() {
  for (int i = 0; i < dev_obj_count; ++i) {
    GpuBufferObject *obj = &dev_buffer_pool[i];
    if (!obj->dev) continue;

    if (obj->host_ptr) {
      obj->dev->Unmap(0, &CD3DX12_RANGE(0, 0));
      obj->host_ptr = NULL;
    }

    obj->dev->Release();
    obj->dev = NULL;
  }
}

GpuBufferObject *av1_memory_allocator_dummy::create_buffer(size_t size, MemoryType mem) {
  size_t addr_offset = ((device_ptr + DeviceMemAlign1) & ~DeviceMemAlign1) - device_ptr;
  size += addr_offset;
  size = (size + DeviceMemAlign1) & (~DeviceMemAlign1);
  device_ptr += size;
  return NULL;
}

void *av1_memory_allocator_dummy::host_allocate(size_t size, int align) {
  align = AOMMAX(align, HostMemAlign) - 1;
  size_t addr_offset = ((host_ptr + align) & ~align) - host_ptr;
  size += addr_offset;
  size = (size + HostMemAlign1) & (~HostMemAlign1);
  host_ptr += size;
  return NULL;
}

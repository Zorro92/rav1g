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
#include "dx/av1_perf.h"
#include "aom_dsp/aom_dsp_common.h"
#include <d3d12.h>
#include <assert.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

enum {
  HostMemAlign = 32,
  DeviceMemAlign = 256,
  HostMemAlign1 = HostMemAlign - 1,
  DeviceMemAlign1 = DeviceMemAlign - 1,
};

// ---------------------------------------------------------------------------
// Log lines for the application (av1_perf.h). The decoder has no other way to say why a creation failed.
// ---------------------------------------------------------------------------
static av1_log_cb_fn_t g_log_cb = NULL;
static void *g_log_priv = NULL;
static char g_create_error[512] = "";

extern "C" void av1_set_log_callback(av1_log_cb_fn_t cb, void *priv) {
  g_log_priv = priv;
  g_log_cb = cb;
}

extern "C" const char *av1_last_create_error(void) { return g_create_error; }

void av1_clear_create_error() { g_create_error[0] = 0; }

static void log_line(int is_error, const char *line) {
  const av1_log_cb_fn_t cb = g_log_cb;
  if (cb) {
    cb(g_log_priv, is_error, line);
  } else {
    OutputDebugStringA(line);
    OutputDebugStringA("\n");
  }
}

void av1_log(int is_error, const char *format, ...) {
  char line[768];
  va_list args;
  va_start(args, format);
  vsnprintf(line, sizeof(line), format, args);
  va_end(args);
  line[sizeof(line) - 1] = 0;
  log_line(is_error, line);
}

extern "C" void av1_log_error(const char *line) { log_line(1, line); }

void av1_log_create_error(const char *format, ...) {
  char line[sizeof(g_create_error)];
  va_list args;
  va_start(args, format);
  vsnprintf(line, sizeof(line), format, args);
  va_end(args);
  line[sizeof(line) - 1] = 0;
  if (!g_create_error[0]) memcpy(g_create_error, line, sizeof(line));
  log_line(1, line);
}

const char *av1_memory_type_name(MemoryType mem) {
  static const char *const names[MemoryTypeCount] = {"DeviceOnly", "DeviceUpload",    "HostRW",
                                                     "ReadBack",   "DeviceOnlyConst", "UploadUAV"};
  return (mem >= 0 && mem < MemoryTypeCount) ? names[mem] : "unknown";
}

static double to_mib(unsigned long long bytes) { return (double)bytes / (1024.0 * 1024.0); }

void av1_query_video_memory(ID3D12Device *device, av1_video_memory *info) {
  memset(info, 0, sizeof(*info));
  info->hr_local = E_FAIL;
  info->hr_non_local = E_FAIL;
  ComPtr<IDXGIFactory4> factory;
  ComPtr<IDXGIAdapter3> adapter;
  HRESULT hr = device ? CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)) : E_POINTER;
  if (SUCCEEDED(hr)) hr = factory->EnumAdapterByLuid(device->GetAdapterLuid(), IID_PPV_ARGS(&adapter));
  info->hr_adapter = hr;
  if (FAILED(hr)) return;
  info->hr_local = adapter->QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &info->local);
  info->hr_non_local = adapter->QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &info->non_local);
}

static int print_segment(char *dst, size_t size, const char *name, HRESULT hr,
                         const DXGI_QUERY_VIDEO_MEMORY_INFO *seg) {
  if (FAILED(hr)) return snprintf(dst, size, "%s QueryVideoMemoryInfo failed (hr 0x%08X)", name, (unsigned)hr);
  return snprintf(dst, size, "%s budget %.1f MiB, usage %.1f MiB, reserved %.1f MiB of %.1f MiB available", name,
                  to_mib(seg->Budget), to_mib(seg->CurrentUsage), to_mib(seg->CurrentReservation),
                  to_mib(seg->AvailableForReservation));
}

void av1_log_video_memory(int is_error, const char *label, const av1_video_memory *info) {
  if (FAILED(info->hr_adapter)) {
    av1_log(is_error, "MEMORY dxgi %s: not available, no IDXGIAdapter3 for the device (hr 0x%08X)", label,
            (unsigned)info->hr_adapter);
    return;
  }
  char local[224];
  char non_local[224];
  print_segment(local, sizeof(local), "local", info->hr_local, &info->local);
  print_segment(non_local, sizeof(non_local), "non-local", info->hr_non_local, &info->non_local);
  av1_log(is_error, "MEMORY dxgi %s: %s; %s", label, local, non_local);
}

extern "C" void av1_log_video_memory_now(void *d3d12device, const char *label) {
  av1_video_memory info;
  av1_query_video_memory(static_cast<ID3D12Device *>(d3d12device), &info);
  av1_log_video_memory(0, label ? label : "", &info);
}

// Debugging aid of the test programs (-poisonmi, -poisonbuf; dx/av1_perf.h): the buffers the CPU writes for
// the shaders start filled with a byte that is not 0 instead of with what the memory held. 0 (the default):
// nothing is filled. The fill is right behind the creation of a buffer (create_buffer), so everything the
// decoder clears or initialises afterwards (the coefficients, which have to be 0 between two frames, the
// grids cleared per frame) is as in a run without it.
static int g_poison_buffers = AV1_POISON_OFF;

extern "C" void av1_set_poison_buffers(int level) { g_poison_buffers = level; }

// The constants of the stages are different: their memory is used again in every frame (types.h,
// ComputeCommandBuffer), so a fill at creation is gone after the first frame, and what a record then starts
// with is the record an earlier frame had at the same place, mostly a plausible one. With -poisonbuf every
// record is filled again when it is handed out, before its writer has written anything.
void av1_poison_constants(void *ptr, size_t size) {
  if (g_poison_buffers == AV1_POISON_BUFFERS) memset(ptr, 0xA5, size);
}

static int poisoned(const GpuBufferObject *obj) {
  if (!obj->host_ptr || g_poison_buffers == AV1_POISON_OFF) return 0;
  if (g_poison_buffers == AV1_POISON_MODE_INFO) return strcmp(obj->name, "mode_info_pool") == 0;
  // Written by the CPU, read by the GPU. A readback buffer goes the other way.
  return obj->memtype == HostRW || obj->memtype == DeviceUpload || obj->memtype == UploadUAV;
}

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
  memset(dev_bytes, 0, sizeof(dev_bytes));
  memset(dev_count, 0, sizeof(dev_count));
}

size_t av1_memory_allocator::device_bytes() const {
  size_t total = 0;
  for (int i = 0; i < MemoryTypeCount; ++i) total += dev_bytes[i];
  return total;
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
  CloseHandle(event);
  return hr;
}

GpuBufferObject *av1_memory_allocator::create_buffer(size_t size, MemoryType mem, const char *name, int slot) {
  static const char *const heap_names[MemoryTypeCount] = {"DEFAULT",  "UPLOAD",  "CUSTOM L0 write-back",
                                                          "READBACK", "DEFAULT", "CUSTOM L0 write-combine"};
  if (!name) name = "unnamed";
  char owner[24] = "";
  if (slot >= 0) snprintf(owner, sizeof(owner), " of slot %d", slot);
  const size_t aligned_size = (size + DeviceMemAlign1) & (~(DeviceMemAlign1));
  const unsigned long long total = device_bytes();

  // The pool of buffer descriptors is fixed. Refuse instead of writing behind it (in a Release build there is
  // no assert to stop that).
  if (dev_obj_count >= DevMemPoolSize) {
    av1_log_create_error(
        "MEMORY FAILED buffer %s%s: %llu bytes (%.1f MiB), type %s: the decoder's pool of %d buffer descriptors "
        "is full; allocated before it: %llu bytes (%.1f MiB) in %d buffers",
        name, owner, (unsigned long long)aligned_size, to_mib(aligned_size), av1_memory_type_name(mem),
        (int)DevMemPoolSize, total, to_mib(total), dev_obj_count);
    return NULL;
  }

  GpuBufferObject *obj = &dev_buffer_pool[dev_obj_count];
  obj->dev = NULL;
  obj->host_ptr = NULL;
  obj->memtype = mem;
  obj->size = aligned_size;
  obj->name = name;
  obj->slot = slot;

  D3D12_HEAP_TYPE heapLut[] = {D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_TYPE_UPLOAD, D3D12_HEAP_TYPE_CUSTOM,
                               D3D12_HEAP_TYPE_READBACK, D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_TYPE_CUSTOM };
  D3D12_RESOURCE_STATES stateLut[] = { D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_STATE_GENERIC_READ,
                               D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS };
  D3D12_CPU_PAGE_PROPERTY pagePropLut[] = { D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                              D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE };

  D3D12_HEAP_PROPERTIES heapProp;
  heapProp.Type = heapLut[mem];
  heapProp.CPUPageProperty = pagePropLut[mem];  //
  heapProp.MemoryPoolPreference = (mem == HostRW || mem == UploadUAV) ? D3D12_MEMORY_POOL_L0 : D3D12_MEMORY_POOL_UNKNOWN;
  heapProp.CreationNodeMask = 0;
  heapProp.VisibleNodeMask = 0;

  const D3D12_RESOURCE_FLAGS flags =
      (mem == DeviceOnly || mem == UploadUAV) ? D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS : D3D12_RESOURCE_FLAG_NONE;
  HRESULT hr = context->device->CreateCommittedResource(&heapProp, D3D12_HEAP_FLAG_NONE,
                                                        &CD3DX12_RESOURCE_DESC::Buffer(obj->size, flags), stateLut[mem], NULL,
                                                        __uuidof(*obj->dev), reinterpret_cast<void **>(&obj->dev));
  const char *step = "CreateCommittedResource";

  if (SUCCEEDED(hr) && mem != DeviceOnly && mem != DeviceOnlyConst) {
    hr = obj->dev->Map(0, &CD3DX12_RANGE(0, 0), &obj->host_ptr);
    step = "Map";
  }

  if (FAILED(hr)) {
    // Nothing of a failed buffer stays behind: its descriptor is not counted and release() never sees it.
    if (obj->dev) obj->dev->Release();
    obj->dev = NULL;
    obj->host_ptr = NULL;
    av1_log_create_error(
        "MEMORY FAILED buffer %s%s: %llu bytes (%.1f MiB), type %s, heap %s, %s hr 0x%08X; allocated before it: "
        "%llu bytes (%.1f MiB) in %d buffers",
        name, owner, (unsigned long long)aligned_size, to_mib(aligned_size), av1_memory_type_name(mem),
        heap_names[mem], step, (unsigned)hr, total, to_mib(total), dev_obj_count);
    return NULL;
  }

  if (poisoned(obj)) memset(obj->host_ptr, 0xA5, obj->size);

  ++dev_obj_count;
  dev_bytes[mem] += obj->size;
  ++dev_count[mem];
  return obj;
}

void *av1_memory_allocator::host_allocate(size_t size, int align) {
  align = AOMMAX(align, HostMemAlign) - 1;
  size_t addr_offset = ((host_offset + align) & ~align) - host_offset;
  size += addr_offset;
  size = (size + HostMemAlign1) & (~HostMemAlign1);
  if ((host_offset + size) > host_max_size) {
    av1_log_create_error(
        "MEMORY FAILED host pool: %llu bytes asked, %llu of %llu bytes in use (the pool is sized by "
        "av1_codec_query_memory_requirements; its configuration must be the one the decoder is created with)",
        (unsigned long long)size, (unsigned long long)host_offset, (unsigned long long)host_max_size);
    return NULL;
  }
  uint8_t *ptr = host_base_addr + host_offset + addr_offset;
  host_offset += size;
  return ptr;
}

size_t av1_memory_allocator::release_type(MemoryType mem, int *count) {
  size_t bytes = 0;
  int released = 0;
  for (int i = 0; i < dev_obj_count; ++i) {
    GpuBufferObject *obj = &dev_buffer_pool[i];
    if (!obj->dev || obj->memtype != mem) continue;

    if (obj->host_ptr) {
      obj->dev->Unmap(0, &CD3DX12_RANGE(0, 0));
      obj->host_ptr = NULL;
    }

    obj->dev->Release();
    obj->dev = NULL;
    bytes += obj->size;
    ++released;
  }
  dev_bytes[mem] = 0;
  dev_count[mem] = 0;
  if (count) *count = released;
  return bytes;
}

void av1_memory_allocator::release() {
  for (int t = 0; t < MemoryTypeCount; ++t) release_type((MemoryType)t, NULL);
  dev_obj_count = 0;
}

GpuBufferObject *av1_memory_allocator_dummy::create_buffer(size_t size, MemoryType mem, const char *name, int slot) {
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

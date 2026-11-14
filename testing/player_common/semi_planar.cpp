/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

#include "semi_planar.h"

#include <string.h>
#include <dxgi1_4.h>

#include "common/log.h"

// D3D11CreateDevice, for -interop.
#pragma comment(lib, "d3d11.lib")

using Microsoft::WRL::ComPtr;

namespace {

std::string Hr(HRESULT hr) {
  char text[16];
  snprintf(text, sizeof(text), "0x%08X", (unsigned)hr);
  return text;
}

const char* FormatName(bool p010) { return p010 ? "P010" : "NV12"; }

// The rows of a semi-planar image in a buffer, as the decoder lays one out: bytes of a row, and where the
// plane of U and V starts.
void Layout(uint32_t width, uint32_t height, bool p010, uint32_t* pitch, uint64_t* chroma) {
  *pitch = (width * (p010 ? 2 : 1) + 255) & ~255u;
  *chroma = ((uint64_t)*pitch * height + 511) & ~(uint64_t)511;
}

D3D12_RESOURCE_BARRIER Transition(ID3D12Resource* resource, D3D12_RESOURCE_STATES before,
                                  D3D12_RESOURCE_STATES after) {
  D3D12_RESOURCE_BARRIER barrier = {};
  barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
  barrier.Transition.pResource = resource;
  barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
  barrier.Transition.StateBefore = before;
  barrier.Transition.StateAfter = after;
  return barrier;
}

}  // namespace

SemiPlanarOutput::SemiPlanarOutput(ID3D12Device* device, ID3D12CommandQueue* queue, int mode, bool interop)
    : device_(device), queue_(queue), mode_(mode), interop_(interop) {
  XB_LOGI << "OUTPUT semi-planar: an image is one NV12 (8-bit) or P010 (10-bit) texture, "
          << (interop_ ? "created by a D3D11 device and opened in D3D12 (-interop)"
                       : (mode_ == 2 ? "which shaders may not write (-output p010-copy)"
                                     : "which shaders may write (-output p010)"))
          << "; frames are not presented";
}

SemiPlanarOutput::~SemiPlanarOutput() {
  if (read_event_) CloseHandle(read_event_);
}

bool SemiPlanarOutput::CreateD3D11() {
  if (d3d11_tried_) return device11_ && context11_;
  d3d11_tried_ = true;
  // A D3D11 device on the adapter of the D3D12 device: a texture is shared between devices of one adapter.
  ComPtr<IDXGIFactory4> factory;
  HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&factory));
  ComPtr<IDXGIAdapter1> adapter;
  if (SUCCEEDED(hr)) hr = factory->EnumAdapterByLuid(device_->GetAdapterLuid(), IID_PPV_ARGS(&adapter));
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: the adapter of the D3D12 device was not found (hr " << Hr(hr) << ")";
    return false;
  }
  const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
  D3D_FEATURE_LEVEL level = D3D_FEATURE_LEVEL_11_0;
  ComPtr<ID3D11Device> device;
  ComPtr<ID3D11DeviceContext> context;
  hr = D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, NULL, 0, levels, 2, D3D11_SDK_VERSION, &device,
                         &level, &context);
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: D3D11CreateDevice on the adapter of the D3D12 device (hr " << Hr(hr) << ")";
    return false;
  }
  hr = device.As(&device11_);
  if (SUCCEEDED(hr)) hr = context.As(&context11_);
  if (FAILED(hr)) {
    device11_.Reset();
    context11_.Reset();
    XB_LOGE << "INTEROP FAILED: the D3D11 device has no ID3D11Device5 or its context no ID3D11DeviceContext4, "
               "which open and wait for a shared fence (hr "
            << Hr(hr) << ")";
    return false;
  }
  XB_LOGI << "INTEROP D3D11 device created on the adapter of the D3D12 device, feature level 0x" << std::hex
          << (unsigned)level << std::dec << ", with ID3D11Device5 and ID3D11DeviceContext4";
  return true;
}

HRESULT SemiPlanarOutput::Create(OutBuffer* buffer, uint32_t width, uint32_t height, frame_buffer_type fb_type,
                                 uint64_t* bytes) {
  const bool p010 = fb_type == fbtP010;
  // The size of the frame, both rounded up to even: what a 4:2:0 texture can have.
  const uint32_t tex_width = (width + 1) & ~1u;
  const uint32_t tex_height = (height + 1) & ~1u;
  const DXGI_FORMAT format = p010 ? DXGI_FORMAT_P010 : DXGI_FORMAT_NV12;
  *bytes = (uint64_t)tex_width * tex_height * 3 / 2 * (p010 ? 2 : 1);
  if (!interop_) {
    D3D12_RESOURCE_DESC desc = {};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width = tex_width;
    desc.Height = tex_height;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.Format = format;
    desc.SampleDesc.Count = 1;
    desc.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    desc.Flags = mode_ == 2 ? D3D12_RESOURCE_FLAG_NONE : D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    D3D12_HEAP_PROPERTIES heap = {};
    heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    // The state the decoder takes the texture in and leaves it in.
    HRESULT hr = device_->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON,
                                                  NULL, IID_PPV_ARGS(&buffer->d3d12textures[0]));
    if (FAILED(hr) && desc.Flags != D3D12_RESOURCE_FLAG_NONE) {
      // A device without such textures for shaders to write: the decoder copies into one they may not.
      if (!logged_texture_) {
        logged_texture_ = true;
        XB_LOGI << "OUTPUT the device has no " << FormatName(p010) << " texture that shaders may write (hr " << Hr(hr)
                << "): the textures are created without, and the decoder copies into them";
      }
      desc.Flags = D3D12_RESOURCE_FLAG_NONE;
      hr = device_->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, NULL,
                                            IID_PPV_ARGS(&buffer->d3d12textures[0]));
    }
    if (FAILED(hr)) {
      XB_LOGE << "OUTPUT FAILED: " << FormatName(p010) << " texture of " << tex_width << "x" << tex_height
              << (mode_ == 2 ? "" : " with D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS") << " (hr " << Hr(hr) << ")";
      return hr;
    }
    *bytes = device_->GetResourceAllocationInfo(0, 1, &desc).SizeInBytes;
    return S_OK;
  }

  if (!CreateD3D11()) return E_FAIL;
  D3D11_TEXTURE2D_DESC desc = {};
  desc.Width = tex_width;
  desc.Height = tex_height;
  desc.MipLevels = 1;
  desc.ArraySize = 1;
  desc.Format = format;
  desc.SampleDesc.Count = 1;
  desc.Usage = D3D11_USAGE_DEFAULT;
  desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED;
  // With unordered access the texture is one the decoder's shaders may write. A device that has no such
  // NV12 or P010 texture gets one without, which the decoder fills by copies.
  ComPtr<ID3D11Texture2D> texture;
  HRESULT hr = E_FAIL;
  HRESULT hr_uav = S_OK;
  if (mode_ != 2) {
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS;
    hr = hr_uav = device11_->CreateTexture2D(&desc, NULL, &texture);
  }
  if (FAILED(hr)) {
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    hr = device11_->CreateTexture2D(&desc, NULL, &texture);
  }
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: D3D11 CreateTexture2D of a " << FormatName(p010) << " texture of " << tex_width << "x"
            << tex_height << " with D3D11_RESOURCE_MISC_SHARED_NTHANDLE | SHARED (hr " << Hr(hr)
            << "; with D3D11_BIND_UNORDERED_ACCESS hr " << Hr(hr_uav) << ")";
    return hr;
  }
  ComPtr<IDXGIResource1> shared;
  HANDLE handle = NULL;
  hr = texture.As(&shared);
  if (SUCCEEDED(hr))
    hr = shared->CreateSharedHandle(NULL, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, NULL, &handle);
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: IDXGIResource1::CreateSharedHandle of the D3D11 texture (hr " << Hr(hr) << ")";
    return hr;
  }
  hr = device_->OpenSharedHandle(handle, IID_PPV_ARGS(&buffer->d3d12textures[0]));
  CloseHandle(handle);
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: ID3D12Device::OpenSharedHandle of the D3D11 texture (hr " << Hr(hr) << ")";
    return hr;
  }
  buffer->d3d11texture = texture;
  if (!logged_texture_) {
    logged_texture_ = true;
    const D3D12_RESOURCE_DESC opened = buffer->d3d12textures[0]->GetDesc();
    XB_LOGI << "INTEROP texture: " << FormatName(p010) << " " << tex_width << "x" << tex_height
            << " created by D3D11 with D3D11_RESOURCE_MISC_SHARED_NTHANDLE | SHARED, bind flags SHADER_RESOURCE"
            << ((desc.BindFlags & D3D11_BIND_UNORDERED_ACCESS)
                    ? " | UNORDERED_ACCESS"
                    : (mode_ == 2 ? "" : " (with UNORDERED_ACCESS the creation failed, hr " + Hr(hr_uav) + ")"))
            << "; opened in D3D12: format " << (int)opened.Format << ", flags 0x" << std::hex << (unsigned)opened.Flags
            << std::dec
            << ((opened.Flags & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
                    ? " (shaders may write it: the direct path)"
                    : " (shaders may not write it: the decoder copies into it)");
  }
  return S_OK;
}

bool SemiPlanarOutput::SetFence(ID3D12Fence* fence) {
  fence11_.Reset();
  if (!interop_) return true;
  if (!fence) {
    XB_LOGE << "INTEROP FAILED: the decoder has no output fence";
    return false;
  }
  if (!CreateD3D11()) return false;
  HANDLE handle = NULL;
  HRESULT hr = device_->CreateSharedHandle(fence, NULL, GENERIC_ALL, NULL, &handle);
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: ID3D12Device::CreateSharedHandle of the decoder's output fence (hr " << Hr(hr) << ")";
    return false;
  }
  hr = device11_->OpenSharedFence(handle, IID_PPV_ARGS(&fence11_));
  CloseHandle(handle);
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: ID3D11Device5::OpenSharedFence of the decoder's output fence (hr " << Hr(hr) << ")";
    return false;
  }
  XB_LOGI << "INTEROP fence: the decoder's output fence (D3D12_FENCE_FLAG_SHARED) was opened in D3D11";
  return true;
}

void SemiPlanarOutput::ToPlanes(const Rav1gFrame* frame, const uint8_t* luma, size_t luma_pitch,
                                const uint8_t* chroma, size_t chroma_pitch, Rav1gFrame* planar) {
  const uint32_t width = frame->width, height = frame->height;
  const uint32_t width_c = (width + 1) / 2, height_c = (height + 1) / 2;
  const bool p010 = frame->format == RAV1G_IMAGE_FORMAT_P010;
  const uint32_t bytes = p010 ? 2 : 1;
  const size_t luma_size = (size_t)width * bytes * height;
  const size_t chroma_size = (size_t)width_c * bytes * height_c;
  planes_.resize(luma_size + 2 * chroma_size);
  uint8_t* out = planes_.data();
  for (uint32_t y = 0; y < height; y++) {
    const uint8_t* row = luma + y * luma_pitch;
    if (p010) {
      // The 10 bits of a sample are the high bits of its 16.
      uint16_t* dst = (uint16_t*)(out + (size_t)y * width * 2);
      for (uint32_t x = 0; x < width; x++) dst[x] = (uint16_t)((row[2 * x] | (row[2 * x + 1] << 8)) >> 6);
    } else {
      memcpy(out + (size_t)y * width, row, width);
    }
  }
  for (int plane = 0; plane < 2; plane++) {
    uint8_t* dst_plane = out + luma_size + plane * chroma_size;
    for (uint32_t y = 0; y < height_c; y++) {
      const uint8_t* row = chroma + y * chroma_pitch;
      if (p010) {
        uint16_t* dst = (uint16_t*)(dst_plane + (size_t)y * width_c * 2);
        for (uint32_t x = 0; x < width_c; x++) {
          const uint8_t* s = row + (2 * x + plane) * 2;
          dst[x] = (uint16_t)((s[0] | (s[1] << 8)) >> 6);
        }
      } else {
        uint8_t* dst = dst_plane + (size_t)y * width_c;
        for (uint32_t x = 0; x < width_c; x++) dst[x] = row[2 * x + plane];
      }
    }
  }
  *planar = *frame;
  planar->format = p010 ? RAV1G_IMAGE_FORMAT_BITS10 : RAV1G_IMAGE_FORMAT_BITS8;
  planar->packed_10x3 = false;
  planar->planes[0] = out;
  planar->planes[1] = out + luma_size;
  planar->planes[2] = out + luma_size + chroma_size;
  planar->strides[0] = width * bytes;
  planar->strides[1] = planar->strides[2] = width_c * bytes;
}

bool SemiPlanarOutput::ReadD3D12(const Rav1gFrame* frame, OutBuffer* buffer) {
  ID3D12Resource* texture = buffer->d3d12textures[0].Get();
  if (!texture || !buffer->d3d12buffer || !buffer->pMem) {
    XB_LOGE << "OUTPUT FAILED: the image of the frame has no texture or no readback buffer";
    return false;
  }
  HRESULT hr = S_OK;
  if (!list_) {
    const D3D12_COMMAND_LIST_TYPE type = queue_->GetDesc().Type;
    hr = device_->CreateCommandAllocator(type, IID_PPV_ARGS(&allocator_));
    if (SUCCEEDED(hr)) hr = device_->CreateCommandList(0, type, allocator_.Get(), NULL, IID_PPV_ARGS(&list_));
    if (SUCCEEDED(hr)) hr = list_->Close();
    if (SUCCEEDED(hr)) hr = device_->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&read_fence_));
    if (SUCCEEDED(hr)) {
      read_event_ = CreateEventEx(NULL, NULL, 0, EVENT_ALL_ACCESS);
      if (!read_event_) hr = HRESULT_FROM_WIN32(GetLastError());
    }
    if (FAILED(hr)) {
      list_.Reset();
      XB_LOGE << "OUTPUT FAILED: the command list of the readback (hr " << Hr(hr) << ")";
      return false;
    }
  }
  const D3D12_RESOURCE_DESC desc = texture->GetDesc();
  const bool p010 = desc.Format == DXGI_FORMAT_P010;
  uint32_t pitch;
  uint64_t chroma;
  Layout((uint32_t)desc.Width, desc.Height, p010, &pitch, &chroma);
  hr = allocator_->Reset();
  if (SUCCEEDED(hr)) hr = list_->Reset(allocator_.Get(), NULL);
  if (FAILED(hr)) {
    XB_LOGE << "OUTPUT FAILED: the command list of the readback (hr " << Hr(hr) << ")";
    return false;
  }
  D3D12_RESOURCE_BARRIER barrier = Transition(texture, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE);
  list_->ResourceBarrier(1, &barrier);
  for (UINT plane = 0; plane < 2; plane++) {
    D3D12_TEXTURE_COPY_LOCATION dst = {};
    dst.pResource = buffer->d3d12buffer.Get();
    dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    dst.PlacedFootprint.Offset = plane ? chroma : 0;
    dst.PlacedFootprint.Footprint.Format =
        p010 ? (plane ? DXGI_FORMAT_R16G16_TYPELESS : DXGI_FORMAT_R16_TYPELESS)
             : (plane ? DXGI_FORMAT_R8G8_TYPELESS : DXGI_FORMAT_R8_TYPELESS);
    dst.PlacedFootprint.Footprint.Width = (UINT)desc.Width >> plane;
    dst.PlacedFootprint.Footprint.Height = desc.Height >> plane;
    dst.PlacedFootprint.Footprint.Depth = 1;
    dst.PlacedFootprint.Footprint.RowPitch = pitch;
    D3D12_TEXTURE_COPY_LOCATION src = {};
    src.pResource = texture;
    src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    src.SubresourceIndex = plane;
    list_->CopyTextureRegion(&dst, 0, 0, 0, &src, NULL);
  }
  barrier = Transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
  list_->ResourceBarrier(1, &barrier);
  hr = list_->Close();
  if (SUCCEEDED(hr)) {
    ID3D12CommandList* lists[] = {list_.Get()};
    queue_->ExecuteCommandLists(1, lists);
    hr = queue_->Signal(read_fence_.Get(), ++read_value_);
  }
  if (SUCCEEDED(hr)) hr = read_fence_->SetEventOnCompletion(read_value_, read_event_);
  if (FAILED(hr)) {
    XB_LOGE << "OUTPUT FAILED: the readback of a " << FormatName(p010) << " texture (hr " << Hr(hr) << ")";
    return false;
  }
  WaitForSingleObjectEx(read_event_, INFINITE, FALSE);
  if (buffer->buffer_size < chroma + (uint64_t)pitch * (desc.Height / 2)) {
    XB_LOGE << "OUTPUT FAILED: the readback buffer of the image is smaller than its texture";
    return false;
  }
  (void)frame;
  return true;
}

bool SemiPlanarOutput::ReadD3D11(const Rav1gFrame* frame, OutBuffer* buffer, Rav1gFrame* planar) {
  ComPtr<ID3D11Texture2D> texture;
  if (!buffer->d3d11texture || FAILED(buffer->d3d11texture.As(&texture))) {
    XB_LOGE << "INTEROP FAILED: the image of the frame has no D3D11 texture";
    return false;
  }
  D3D11_TEXTURE2D_DESC desc;
  texture->GetDesc(&desc);
  if (!staging_ || staging_desc_.Width != desc.Width || staging_desc_.Height != desc.Height ||
      staging_desc_.Format != desc.Format) {
    staging_.Reset();
    staging_desc_ = desc;
    staging_desc_.Usage = D3D11_USAGE_STAGING;
    staging_desc_.BindFlags = 0;
    staging_desc_.MiscFlags = 0;
    staging_desc_.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    const HRESULT hr = device11_->CreateTexture2D(&staging_desc_, NULL, &staging_);
    if (FAILED(hr)) {
      XB_LOGE << "INTEROP FAILED: the D3D11 staging texture the frames are read through (hr " << Hr(hr) << ")";
      return false;
    }
  }
  context11_->CopyResource(staging_.Get(), texture.Get());
  D3D11_MAPPED_SUBRESOURCE mapped = {};
  const HRESULT hr = context11_->Map(staging_.Get(), 0, D3D11_MAP_READ, 0, &mapped);
  if (FAILED(hr)) {
    XB_LOGE << "INTEROP FAILED: Map of the D3D11 staging texture (hr " << Hr(hr) << ")";
    return false;
  }
  // The planes of a mapped NV12 or P010 texture: the luma rows, then the rows of U and V.
  const uint8_t* luma = (const uint8_t*)mapped.pData;
  ToPlanes(frame, luma, mapped.RowPitch, luma + (size_t)mapped.RowPitch * desc.Height, mapped.RowPitch, planar);
  context11_->Unmap(staging_.Get(), 0);
  return true;
}

bool SemiPlanarOutput::Frame(const Rav1gFrame* frame, OutBuffer* buffer, bool read, Rav1gFrame* planar) {
  ++frames_;
  if (frame->format != RAV1G_IMAGE_FORMAT_NV12 && frame->format != RAV1G_IMAGE_FORMAT_P010) {
    XB_LOGE << "OUTPUT FAILED: the frame is not semi-planar (format " << (int)frame->format << ")";
    ++failed_;
    return false;
  }
  if (interop_) {
    // What the D3D11 side of an application does before it uses the texture: its commands wait for the
    // value the decoder's fence has when the frame is written.
    ID3D12Fence* fence = (ID3D12Fence*)frame->d3d12_fence;
    if (!fence11_ || !context11_ || !fence) {
      XB_LOGE << "INTEROP FAILED: no shared fence to wait on for frame " << (frames_ - 1);
      ++failed_;
      return false;
    }
    const HRESULT hr = context11_->Wait(fence11_.Get(), frame->fence_value);
    if (FAILED(hr)) {
      XB_LOGE << "INTEROP FAILED: ID3D11DeviceContext4::Wait for the value " << frame->fence_value
              << " of the output fence (hr " << Hr(hr) << ")";
      ++failed_;
      return false;
    }
    ++waited_;
    if (!read) {
      context11_->Flush();
      return true;
    }
  }
  if (!read) return true;
  bool ok;
  if (interop_) {
    ok = ReadD3D11(frame, buffer, planar);
  } else {
    ok = ReadD3D12(frame, buffer);
    if (ok) {
      const D3D12_RESOURCE_DESC desc = buffer->d3d12textures[0]->GetDesc();
      uint32_t pitch;
      uint64_t chroma;
      Layout((uint32_t)desc.Width, desc.Height, desc.Format == DXGI_FORMAT_P010, &pitch, &chroma);
      ToPlanes(frame, (const uint8_t*)buffer->pMem, pitch, (const uint8_t*)buffer->pMem + chroma, pitch, planar);
    }
  }
  if (!ok) {
    ++failed_;
    return false;
  }
  ++read_;
  return true;
}

void SemiPlanarOutput::Report() {
  if (interop_) {
    XB_LOGI << "INTEROP result: " << frames_ << " frames; " << waited_
            << " waited for on the shared fence by the D3D11 context; " << read_
            << " read from the D3D11 texture; " << failed_ << " failed"
            << ((failed_ == 0 && waited_ == frames_ && frames_ != 0) ? ": INTEROP is OK" : ": INTEROP FAILED");
  } else {
    XB_LOGI << "OUTPUT semi-planar result: " << frames_ << " frames; " << read_ << " read back from their texture; "
            << failed_ << " failed";
  }
  frames_ = waited_ = read_ = failed_ = 0;
}

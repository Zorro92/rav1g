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

#include "pch.h"
#include "DXEngine.h"
#include "SamplePixelShader.h"
#include "SampleVertexShader.h"
#include "ps_cbuf.h"

using namespace Windows::Graphics::Display;

DXEngine::DXEngine(Windows::UI::Core::CoreWindow ^ window) : m_frameIndex(0), m_rtvDescriptorSize(0) {
  DisplayInformation ^ currentDisplayInformation = DisplayInformation::GetForCurrentView();
  m_window = window;
  m_logicalSize = Windows::Foundation::Size(m_window->Bounds.Width, m_window->Bounds.Height);
  m_dpi = currentDisplayInformation->LogicalDpi;
  UpdateRenderTargetSize();
  m_viewport = CD3DX12_VIEWPORT(0.0f, 0.0f, static_cast<float>(m_width), static_cast<float>(m_height));
  m_scissorRect = CD3DX12_RECT(0, 0, static_cast<LONG>(m_width), static_cast<LONG>(m_height)), strides_[0] = m_width;
  strides_[1] = strides_[2] = strides_[0] / 2;
  upload_buffer_size_[0] = upload_buffer_size_[1] = upload_buffer_size_[2] = 0;
}

// Converts a length in device-independent pixels (DIPs) to a length in physical pixels.
static inline float ConvertDipsToPixels(float dips, float dpi) {
  static const float dipsPerInch = 96.0f;
  return floorf(dips * dpi / dipsPerInch + 0.5f);  // Round to nearest integer.
}

void DXEngine::UpdateRenderTargetSize() {
  m_effectiveDpi = m_dpi;

  // Calculate the necessary render target size in pixels.
  m_width = max(lround(ConvertDipsToPixels(m_logicalSize.Width, m_effectiveDpi)), 1);
  m_height = max(lround(ConvertDipsToPixels(m_logicalSize.Height, m_effectiveDpi)), 1);
}

int DXEngine::OnInit() {
  if (LoadPipeline() || LoadAssets()) return -1;
  ready_ = 1;
  return 0;
}

// Load the rendering pipeline dependencies.
int DXEngine::LoadPipeline() {
  UINT dxgiFactoryFlags = 0;
  D2D1_FACTORY_OPTIONS factory_options = {D2D1_DEBUG_LEVEL_NONE};
#if defined(_DEBUG)
  // Enable the debug layer (requires the Graphics Tools "optional feature").
  // NOTE: Enabling the debug layer after device creation will invalidate the active device.
  {
    ComPtr<ID3D12Debug> debugController;
    if (SUCCEEDED(D3D12GetDebugInterface(IID_PPV_ARGS(&debugController)))) {
      debugController->EnableDebugLayer();

      // Enable additional debug layers.
      dxgiFactoryFlags |= DXGI_CREATE_FACTORY_DEBUG;
      factory_options.debugLevel = D2D1_DEBUG_LEVEL_INFORMATION;
    }
  }
#endif

  ComPtr<IDXGIFactory4> factory;
  CHECK_HRESULT(CreateDXGIFactory2(dxgiFactoryFlags, IID_PPV_ARGS(&factory)));

  if (m_useWarpDevice) {
    ComPtr<IDXGIAdapter> warpAdapter;
    CHECK_HRESULT(factory->EnumWarpAdapter(IID_PPV_ARGS(&warpAdapter)));

    CHECK_HRESULT(D3D12CreateDevice(warpAdapter.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&m_device)));
  } else {
    ComPtr<IDXGIAdapter1> hardwareAdapter;
    GetHardwareAdapter(factory.Get(), &hardwareAdapter);

    CHECK_HRESULT(D3D12CreateDevice(hardwareAdapter.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&m_device)));
  }

  // Describe and create the command queue.
  D3D12_COMMAND_QUEUE_DESC queueDesc = {};
  queueDesc.Flags = D3D12_COMMAND_QUEUE_FLAG_NONE;
  queueDesc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;

  CHECK_HRESULT(m_device->CreateCommandQueue(&queueDesc, IID_PPV_ARGS(&m_commandQueue)));

  ComPtr<ID3D11Device> d3d11Device;
  CHECK_HRESULT(D3D11On12CreateDevice(m_device.Get(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                                      reinterpret_cast<IUnknown**>(m_commandQueue.GetAddressOf()), 1, 0, &d3d11Device,
                                      &d11on12device_context_, nullptr));

  // Query the 11On12 device from the 11 device.
  CHECK_HRESULT(d3d11Device.As(&d11on12device_));

  // Describe and create the swap chain.
  DXGI_SWAP_CHAIN_DESC1 swapChainDesc = {};
  swapChainDesc.BufferCount = FrameCount;
  swapChainDesc.Width = m_width;
  swapChainDesc.Height = m_height;
  swapChainDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  swapChainDesc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  swapChainDesc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  swapChainDesc.SampleDesc.Count = 1;
  swapChainDesc.Scaling = DXGI_SCALING_NONE;

  ComPtr<IDXGISwapChain1> swapChain;
  CHECK_HRESULT(factory->CreateSwapChainForCoreWindow(
      m_commandQueue.Get(),  // Swap chain needs the queue so that it can force a flush on it.
      reinterpret_cast<IUnknown*>(m_window.Get()), &swapChainDesc, nullptr, &swapChain));

  D2D1_DEVICE_CONTEXT_OPTIONS deviceOptions = D2D1_DEVICE_CONTEXT_OPTIONS_NONE;
  CHECK_HRESULT(
      D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory3), &factory_options, &d2d1factory_));
  ComPtr<IDXGIDevice> dxgiDevice;
  CHECK_HRESULT(d11on12device_.As(&dxgiDevice));
  CHECK_HRESULT(d2d1factory_->CreateDevice(dxgiDevice.Get(), &d2d1device2_));
  CHECK_HRESULT(d2d1device2_->CreateDeviceContext(deviceOptions, &d2d1deviceContext_));
  CHECK_HRESULT(DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory), &d2writeFactory_));

  float dpiX;
  float dpiY;
  d2d1factory_->GetDesktopDpi(&dpiX, &dpiY);
  D2D1_BITMAP_PROPERTIES1 bitmapProperties =
      D2D1::BitmapProperties1(D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                              D2D1::PixelFormat(DXGI_FORMAT_UNKNOWN, D2D1_ALPHA_MODE_PREMULTIPLIED), dpiX, dpiY);

  CHECK_HRESULT(swapChain.As(&m_swapChain));
  m_frameIndex = m_swapChain->GetCurrentBackBufferIndex();

  // Create descriptor heaps.
  {
    // Describe and create a render target view (RTV) descriptor heap.
    D3D12_DESCRIPTOR_HEAP_DESC rtvHeapDesc = {};
    rtvHeapDesc.NumDescriptors = FrameCount;
    rtvHeapDesc.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
    rtvHeapDesc.Flags = D3D12_DESCRIPTOR_HEAP_FLAG_NONE;
    CHECK_HRESULT(m_device->CreateDescriptorHeap(&rtvHeapDesc, IID_PPV_ARGS(&m_rtvHeap)));

    // Describe and create a shader resource view (SRV) heap for the texture.
    D3D12_DESCRIPTOR_HEAP_DESC srvHeapDesc = {};
    srvHeapDesc.NumDescriptors = 4;
    srvHeapDesc.Type = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
    srvHeapDesc.Flags = D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE;
    CHECK_HRESULT(m_device->CreateDescriptorHeap(&srvHeapDesc, IID_PPV_ARGS(&m_srvHeap)));

    m_rtvDescriptorSize = m_device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
    m_srvDescriptorSize = m_device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
  }

  // Create frame resources.
  {
    CD3DX12_CPU_DESCRIPTOR_HANDLE rtvHandle(m_rtvHeap->GetCPUDescriptorHandleForHeapStart());

    // Create a RTV for each frame.
    for (UINT n = 0; n < FrameCount; n++) {
      CHECK_HRESULT(m_swapChain->GetBuffer(n, IID_PPV_ARGS(&m_renderTargets[n])));
      m_device->CreateRenderTargetView(m_renderTargets[n].Get(), nullptr, rtvHandle);
      // Create a wrapped 11On12 resource of this back buffer. Since we are
      // rendering all D3D12 content first and then all D2D content, we specify
      // the In resource state as RENDER_TARGET - because D3D12 will have last
      // used it in this state - and the Out resource state as PRESENT. When
      // ReleaseWrappedResources() is called on the 11On12 device, the resource
      // will be transitioned to the PRESENT state.
      D3D11_RESOURCE_FLAGS d3d11Flags = {D3D11_BIND_RENDER_TARGET};
      CHECK_HRESULT(d11on12device_->CreateWrappedResource(
          m_renderTargets[n].Get(), &d3d11Flags, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT,
          IID_PPV_ARGS(&wrappedBackBuffers_[n])));

      // Create a render target for D2D to draw directly to this back buffer.
      ComPtr<IDXGISurface> surface;
      CHECK_HRESULT(wrappedBackBuffers_[n].As(&surface));
      CHECK_HRESULT(
          d2d1deviceContext_->CreateBitmapFromDxgiSurface(surface.Get(), &bitmapProperties, &d2d1renderTargets_[n]));

      rtvHandle.Offset(1, m_rtvDescriptorSize);
    }
  }

  CHECK_HRESULT(m_device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&m_commandAllocator)));

  CHECK_HRESULT(d2d1deviceContext_->CreateSolidColorBrush(D2D1::ColorF(D2D1::ColorF::LightGreen), &textBrush_));
  CHECK_HRESULT(d2writeFactory_->CreateTextFormat(L"Verdana", NULL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                                                  DWRITE_FONT_STRETCH_NORMAL, 24, L"en-us", &textFormat_));
  CHECK_HRESULT(textFormat_->SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING));
  CHECK_HRESULT(textFormat_->SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR));

  return S_OK;
}

#define TEXT_TOP 20
#define TEXT_LEFT 20
void DXEngine::RenderUI(std::shared_ptr<OneRenderTask> task) {
  D2D1_SIZE_F rtSize = d2d1renderTargets_[m_frameIndex]->GetSize();
  D2D1_RECT_F textRect = D2D1::RectF(TEXT_LEFT, TEXT_TOP, rtSize.width - TEXT_LEFT, 100);
  //    static const WCHAR text[] = L"11On12";

  // Acquire our wrapped render target resource for the current back buffer.
  d11on12device_->AcquireWrappedResources(wrappedBackBuffers_[m_frameIndex].GetAddressOf(), 1);

  // Render text directly to the back buffer.
  d2d1deviceContext_->SetTarget(d2d1renderTargets_[m_frameIndex].Get());
  d2d1deviceContext_->BeginDraw();
  d2d1deviceContext_->SetTransform(D2D1::Matrix3x2F::Identity());
  d2d1deviceContext_->DrawText(task->screen_msg.c_str(), (UINT32)task->screen_msg.length(), textFormat_.Get(),
                               &textRect, textBrush_.Get());
  CHECK_HRESULT_0(d2d1deviceContext_->EndDraw());

  // Release our wrapped render target resource. Releasing
  // transitions the back buffer resource to the state specified
  // as the OutState when the wrapped resource was created.
  d11on12device_->ReleaseWrappedResources(wrappedBackBuffers_[m_frameIndex].GetAddressOf(), 1);

  // Flush to submit the 11 command list to the shared command queue.
  d11on12device_context_->Flush();
}

// Load the sample assets.
int DXEngine::LoadAssets() {
  // Create the root signature.
  {
    HRESULT hr;
    D3D12_FEATURE_DATA_ROOT_SIGNATURE featureData = {};

    // This is the highest version the sample supports. If CheckFeatureSupport succeeds, the HighestVersion returned
    // will not be greater than this.
    featureData.HighestVersion = D3D_ROOT_SIGNATURE_VERSION_1_1;

    if (FAILED(m_device->CheckFeatureSupport(D3D12_FEATURE_ROOT_SIGNATURE, &featureData, sizeof(featureData)))) {
      featureData.HighestVersion = D3D_ROOT_SIGNATURE_VERSION_1_0;
    }

    CD3DX12_DESCRIPTOR_RANGE1 ranges[2];
    ranges[0].Init(D3D12_DESCRIPTOR_RANGE_TYPE_CBV, 1, 0, 0,
                   D3D12_DESCRIPTOR_RANGE_FLAG_NONE /*D3D12_DESCRIPTOR_RANGE_FLAG_DATA_STATIC*/);
    ranges[1].Init(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 3, 0, 0,
                   D3D12_DESCRIPTOR_RANGE_FLAG_NONE /*D3D12_DESCRIPTOR_RANGE_FLAG_DATA_STATIC*/);

    CD3DX12_ROOT_PARAMETER1 rootParameters[1];
    rootParameters[0].InitAsDescriptorTable(2, &ranges[0], D3D12_SHADER_VISIBILITY_PIXEL);

    D3D12_STATIC_SAMPLER_DESC sampler = {};
    sampler.Filter = D3D12_FILTER_MIN_MAG_MIP_POINT;
    sampler.AddressU = D3D12_TEXTURE_ADDRESS_MODE_BORDER;
    sampler.AddressV = D3D12_TEXTURE_ADDRESS_MODE_BORDER;
    sampler.AddressW = D3D12_TEXTURE_ADDRESS_MODE_BORDER;
    sampler.MipLODBias = 0;
    sampler.MaxAnisotropy = 0;
    sampler.ComparisonFunc = D3D12_COMPARISON_FUNC_NEVER;
    sampler.BorderColor = D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK;
    sampler.MinLOD = 0.0f;
    sampler.MaxLOD = D3D12_FLOAT32_MAX;
    sampler.ShaderRegister = 0;
    sampler.RegisterSpace = 0;
    sampler.ShaderVisibility = D3D12_SHADER_VISIBILITY_PIXEL;
    D3D12_STATIC_SAMPLER_DESC sampler1 = sampler;
    sampler1.ShaderRegister = 1;

    D3D12_STATIC_SAMPLER_DESC samplers[2] = {sampler, sampler1};

    CD3DX12_VERSIONED_ROOT_SIGNATURE_DESC rootSignatureDesc;
    rootSignatureDesc.Init_1_1(_countof(rootParameters), rootParameters, 2, samplers,
                               D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT);

    ComPtr<ID3DBlob> signature;
    ComPtr<ID3DBlob> error;
    hr = D3DX12SerializeVersionedRootSignature(&rootSignatureDesc, featureData.HighestVersion, &signature, &error);
    if (FAILED(hr)) {
      void* ss = error->GetBufferPointer();
      wprintf(L"%s\n", (wchar_t*)ss);
    }
    CHECK_HRESULT(m_device->CreateRootSignature(0, signature->GetBufferPointer(), signature->GetBufferSize(),
                                                IID_PPV_ARGS(&m_rootSignature)));

    // create CbPixShData buffer
    const D3D12_HEAP_TYPE heapType = D3D12_HEAP_TYPE_UPLOAD;
    const D3D12_RESOURCE_STATES state = D3D12_RESOURCE_STATE_GENERIC_READ;
    D3D12_HEAP_PROPERTIES heapProp;
    heapProp.Type = heapType;
    heapProp.CPUPageProperty = D3D12_CPU_PAGE_PROPERTY_UNKNOWN;  //
    heapProp.MemoryPoolPreference = D3D12_MEMORY_POOL_UNKNOWN;
    heapProp.CreationNodeMask = 0;
    heapProp.VisibleNodeMask = 0;

    const D3D12_RESOURCE_FLAGS flags = D3D12_RESOURCE_FLAG_NONE;
    hr = m_device->CreateCommittedResource(&heapProp, D3D12_HEAP_FLAG_NONE, &CD3DX12_RESOURCE_DESC::Buffer(256, flags),
                                           state, NULL, IID_PPV_ARGS(&hdr_buffer_));
    CHECK_HRESULT(hr);
    void* ptr = 0;
    hr = hdr_buffer_->Map(0, NULL, &ptr);
    if (FAILED(hr)) return 0;
    CbPixShData buffer = {};
    buffer.scale_factor = 1.0;
    buffer.hdr_10_10_10_2 = 0;
    memcpy(ptr, &buffer, sizeof(CbPixShData));
    hdr_buffer_->Unmap(0, 0);
  }

  // Create the pipeline state, which includes compiling and loading shaders.
  {
    // Define the vertex input layout.
    D3D12_INPUT_ELEMENT_DESC inputElementDescs[] = {
        {"POSITION", 0, DXGI_FORMAT_R32G32B32_FLOAT, 0, 0, D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA, 0},
        {"TEXCOORD", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 12, D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA, 0}};

    // Describe and create the graphics pipeline state object (PSO).
    D3D12_GRAPHICS_PIPELINE_STATE_DESC psoDesc = {};
    psoDesc.InputLayout = {inputElementDescs, _countof(inputElementDescs)};
    psoDesc.pRootSignature = m_rootSignature.Get();
    psoDesc.VS = CD3DX12_SHADER_BYTECODE(SampleVertexShader, sizeof(SampleVertexShader));
    psoDesc.PS = CD3DX12_SHADER_BYTECODE(SamplePixelShader, sizeof(SamplePixelShader));
    psoDesc.RasterizerState = CD3DX12_RASTERIZER_DESC(D3D12_DEFAULT);
    psoDesc.BlendState = CD3DX12_BLEND_DESC(D3D12_DEFAULT);
    psoDesc.DepthStencilState.DepthEnable = FALSE;
    psoDesc.DepthStencilState.StencilEnable = FALSE;
    psoDesc.SampleMask = UINT_MAX;
    psoDesc.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    psoDesc.NumRenderTargets = 1;
    psoDesc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    psoDesc.SampleDesc.Count = 1;
    HRESULT hr = m_device->CreateGraphicsPipelineState(&psoDesc, IID_PPV_ARGS(&m_pipelineState));
    CHECK_HRESULT(hr);
  }

  // Create the command list.
  CHECK_HRESULT(m_device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, m_commandAllocator.Get(),
                                            m_pipelineState.Get(), IID_PPV_ARGS(&m_commandList)));
  SET_D3D12_NAME(m_commandList);
  setVertexes();

  // Close the command list and execute it to begin the initial GPU setup.
  CHECK_HRESULT(m_commandList->Close());
  ID3D12CommandList* ppCommandLists[] = {m_commandList.Get()};
  m_commandQueue->ExecuteCommandLists(_countof(ppCommandLists), ppCommandLists);

  // Create synchronization objects and wait until assets have been uploaded to the GPU.
  {
    CHECK_HRESULT(m_device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&m_fence)));
    m_fenceValue = 1;

    // Create an event handle to use for frame synchronization.
    m_fenceEvent = CreateEvent(nullptr, FALSE, FALSE, nullptr);
    if (m_fenceEvent == nullptr) {
      CHECK_HRESULT(HRESULT_FROM_WIN32(GetLastError()));
    }

    // Wait for the command list to execute; we are reusing the same command
    // list in our main loop but for now, we just want to wait for setup to
    // complete before continuing.
    WaitForPreviousFrame();
  }
  return S_OK;
}

void DXEngine::setVertexes() {
  fRect startRect = {-1.0f, 1.0f, 1.0f, -1.0f};
  float texture_right = 1.0;
  Vertex triangleVertices[] = {{{startRect.left, startRect.top, 0.0f}, {0.0f, 0.0f}},
                               {{startRect.right, startRect.top, 0.0f}, {texture_right, 0.0f}},
                               {{startRect.left, startRect.bottom, 0.0f}, {0.0f, 1.0f}},
                               {{startRect.right, startRect.bottom, 0.0f}, {texture_right, 1.0f}}};
  const UINT vertexesNumber = sizeof(triangleVertices) / sizeof(Vertex);
  setVertexes(triangleVertices, vertexesNumber);
}

void DXEngine::OnResize() {
  std::unique_lock<std::mutex> lck(cs_render_);
  if (hasTexture_) {
    CHECK_HRESULT_0(m_commandAllocator->Reset());
    CHECK_HRESULT_0(m_commandList->Reset(m_commandAllocator.Get(), m_pipelineState.Get()));
    setVertexes();
    InternalRender(FALSE, 0);
  }
}

// Update frame-based values.
void DXEngine::OnUpdate() {}

// Render the scene.
void DXEngine::OnRender(std::shared_ptr<OneRenderTask> task) {
  if (ready_ && task && !task->shown) {
    InternalRender(TRUE, task);
  }
}

void DXEngine::InternalRender(BOOL reset, std::shared_ptr<OneRenderTask> task) {
  // Record all the commands we need to render the scene into the command list.
  PopulateCommandList(reset, task);

  // Execute the command list.
  ID3D12CommandList* ppCommandLists[] = {m_commandList.Get()};
  m_commandQueue->ExecuteCommandLists(_countof(ppCommandLists), ppCommandLists);

  RenderUI(task);

  // Present the frame.
  CHECK_HRESULT_0(m_swapChain->Present(1, 0));

  WaitForPreviousFrame();
}

void DXEngine::OnDestroy() {
  // Ensure that the GPU is no longer referencing resources that are about to be
  // cleaned up by the destructor.
  WaitForPreviousFrame();
  ready_ = 0;
  CloseHandle(m_fenceEvent);
}

void DXEngine::PopulateCommandList(BOOL reset, std::shared_ptr<OneRenderTask> task) {
  // Command list allocators can only be reset when the associated
  // command lists have finished execution on the GPU; apps should use
  // fences to determine GPU execution progress.
  if (reset) {
    CHECK_HRESULT_0(m_commandAllocator->Reset());

    // However, when ExecuteCommandList() is called on a particular command
    // list, that command list can then be reset at any time and must be before
    // re-recording.
    CHECK_HRESULT_0(m_commandList->Reset(m_commandAllocator.Get(), m_pipelineState.Get()));
  }

  CD3DX12_CPU_DESCRIPTOR_HANDLE rtvHandle(m_rtvHeap->GetCPUDescriptorHandleForHeapStart(), m_frameIndex,
                                          m_rtvDescriptorSize);
  m_commandList->OMSetRenderTargets(1, &rtvHandle, FALSE, nullptr);
  m_commandList->ResourceBarrier(
      1, &CD3DX12_RESOURCE_BARRIER::Transition(m_renderTargets[m_frameIndex].Get(), D3D12_RESOURCE_STATE_PRESENT,
                                               D3D12_RESOURCE_STATE_RENDER_TARGET));
  const float clearColor[] = {0.0f, 0.2f, 0.4f, 1.0f};
  m_commandList->ClearRenderTargetView(rtvHandle, clearColor, 0, nullptr);

  if (task) {
    if (task->type != renderType_) {
      void* ptr = 0;
      HRESULT hr = hdr_buffer_->Map(0, NULL, &ptr);
      CHECK_HRESULT_0(hr);
      renderType_ = task->type;
      CbPixShData* cbptr = (CbPixShData*)ptr;
      cbptr->scale_factor = renderType_ == ft10bit ? 64.0f : 1.0f;
      cbptr->hdr_10_10_10_2 = renderType_ == ft10x3 ? 1 : 0;
      if (cbptr->hdr_10_10_10_2) {
        D3D12_RESOURCE_DESC desc = task->textures[0]->GetDesc();
        cbptr->disp_w = lround(m_viewport.Width);
        cbptr->disp_h = lround(m_viewport.Height);
        cbptr->frame_w = task->render_width;
        cbptr->frame_h = task->render_height;
        cbptr->ptex_w = (int)desc.Width;
        cbptr->ptex_h = desc.Height;
      }
      hdr_buffer_->Unmap(0, 0);
    }
    const D3D12_RESOURCE_BARRIER barriers[3] = {
        CD3DX12_RESOURCE_BARRIER::Transition(task->textures[0].Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                             D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE),
        CD3DX12_RESOURCE_BARRIER::Transition(task->textures[1].Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                             D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE),
        CD3DX12_RESOURCE_BARRIER::Transition(task->textures[2].Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                                             D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE)};
    m_commandList->ResourceBarrier(3, barriers);
    CD3DX12_CPU_DESCRIPTOR_HANDLE srvHandle(m_srvHeap->GetCPUDescriptorHandleForHeapStart());

    m_commandList->SetGraphicsRootSignature(m_rootSignature.Get());

    D3D12_CONSTANT_BUFFER_VIEW_DESC cbvDesc{};
    D3D12_RESOURCE_DESC cbdesk = hdr_buffer_->GetDesc();
    cbvDesc.BufferLocation = hdr_buffer_->GetGPUVirtualAddress();
    cbvDesc.SizeInBytes = (UINT)cbdesk.Width;
    m_device->CreateConstantBufferView(&cbvDesc, srvHandle);
    srvHandle.Offset(1, m_srvDescriptorSize);

    for (int i = 0; i < 3; i++) {
      D3D12_RESOURCE_DESC textureDesc = task->textures[i]->GetDesc();
      D3D12_SHADER_RESOURCE_VIEW_DESC srvDesc = {};
      srvDesc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
      srvDesc.Format = textureDesc.Format;
      srvDesc.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
      srvDesc.Texture2D.MipLevels = 1;
      m_device->CreateShaderResourceView(task->textures[i].Get(), &srvDesc, srvHandle);
      srvHandle.Offset(1, m_srvDescriptorSize);
    }
    // Set necessary state.

    ID3D12DescriptorHeap* ppHeaps[] = {m_srvHeap.Get()};
    m_commandList->SetDescriptorHeaps(_countof(ppHeaps), ppHeaps);

    m_commandList->SetGraphicsRootDescriptorTable(0, m_srvHeap->GetGPUDescriptorHandleForHeapStart());
    m_commandList->RSSetViewports(1, &m_viewport);
    m_commandList->RSSetScissorRects(1, &m_scissorRect);

    // Record commands.
    m_commandList->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP /*D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST*/);
    m_commandList->IASetVertexBuffers(0, 1, &m_vertexBufferView);
    m_commandList->DrawInstanced(4, 1, 0, 0);
    {
      const D3D12_RESOURCE_BARRIER barriers[3] = {
          CD3DX12_RESOURCE_BARRIER::Transition(task->textures[0].Get(), D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                                               D3D12_RESOURCE_STATE_COPY_DEST),
          CD3DX12_RESOURCE_BARRIER::Transition(task->textures[1].Get(), D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                                               D3D12_RESOURCE_STATE_COPY_DEST),
          CD3DX12_RESOURCE_BARRIER::Transition(task->textures[2].Get(), D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                                               D3D12_RESOURCE_STATE_COPY_DEST)};
      m_commandList->ResourceBarrier(3, barriers);
    }
  }

  CHECK_HRESULT_0(m_commandList->Close());
}

void DXEngine::WaitForPreviousFrame() {
  // WAITING FOR THE FRAME TO COMPLETE BEFORE CONTINUING IS NOT BEST PRACTICE.
  // This is code implemented as such for simplicity. The D3D12HelloFrameBuffering
  // sample illustrates how to use fences for efficient resource usage and to
  // maximize GPU utilization.

  // Signal and increment the fence value.
  const UINT64 fence = m_fenceValue;
  CHECK_HRESULT_0(m_commandQueue->Signal(m_fence.Get(), fence));
  m_fenceValue++;

  // Wait until the previous frame is finished.
  if (m_fence->GetCompletedValue() < fence) {
    CHECK_HRESULT_0(m_fence->SetEventOnCompletion(fence, m_fenceEvent));
    WaitForSingleObject(m_fenceEvent, INFINITE);
  }

  m_frameIndex = m_swapChain->GetCurrentBackBufferIndex();
}

// Helper function for acquiring the first available hardware adapter that supports Direct3D 12.
// If no such adapter can be found, *ppAdapter will be set to nullptr.
_Use_decl_annotations_ void DXEngine::GetHardwareAdapter(IDXGIFactory2* pFactory, IDXGIAdapter1** ppAdapter) {
  ComPtr<IDXGIAdapter1> adapter;
  *ppAdapter = nullptr;

  for (UINT adapterIndex = 0; DXGI_ERROR_NOT_FOUND != pFactory->EnumAdapters1(adapterIndex, &adapter); ++adapterIndex) {
    DXGI_ADAPTER_DESC1 desc;
    adapter->GetDesc1(&desc);

    if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) {
      // Don't select the Basic Render Driver adapter.
      // If you want a software adapter, pass in "/warp" on the command line.
      continue;
    }

    // Check to see if the adapter supports Direct3D 12, but don't create the
    // actual device yet.
    if (SUCCEEDED(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0, _uuidof(ID3D12Device), nullptr))) {
      break;
    }
  }

  *ppAdapter = adapter.Detach();
}

void DXEngine::setVertexes(Vertex* vertexes, UINT size) {
  UINT vertexBufferSize = sizeof(Vertex) * size;
  CHECK_HRESULT_0(m_device->CreateCommittedResource(
      &CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_UPLOAD), D3D12_HEAP_FLAG_NONE,
      &CD3DX12_RESOURCE_DESC::Buffer(vertexBufferSize), D3D12_RESOURCE_STATE_GENERIC_READ, nullptr,
      IID_PPV_ARGS(&vertexUploadBuffer_)));
  SET_D3D12_NAME(vertexUploadBuffer_);

  // Copy the triangle data to the vertex buffer.
  UINT8* pVertexDataBegin;
  CD3DX12_RANGE readRange(0, 0);  // We do not intend to read from this resource on the CPU.
  CHECK_HRESULT_0(vertexUploadBuffer_->Map(0, &readRange, reinterpret_cast<void**>(&pVertexDataBegin)));
  memcpy(pVertexDataBegin, vertexes, vertexBufferSize);
  vertexUploadBuffer_->Unmap(0, nullptr);

  CHECK_HRESULT_0(
      m_device->CreateCommittedResource(&CD3DX12_HEAP_PROPERTIES(D3D12_HEAP_TYPE_DEFAULT), D3D12_HEAP_FLAG_NONE,
                                        &CD3DX12_RESOURCE_DESC::Buffer(vertexBufferSize),
                                        D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_PPV_ARGS(&m_vertexBuffer)));
  SET_D3D12_NAME(m_vertexBuffer);
  if (m_commandList) {
    m_commandList->CopyBufferRegion(m_vertexBuffer.Get(), 0, vertexUploadBuffer_.Get(), 0, vertexBufferSize);
    const D3D12_RESOURCE_BARRIER barriers[1] = {CD3DX12_RESOURCE_BARRIER::Transition(
        m_vertexBuffer.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER /*D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE*/)};
    m_commandList->ResourceBarrier(1, barriers);
  }

  // Initialize the vertex buffer view.
  m_vertexBufferView.BufferLocation = m_vertexBuffer->GetGPUVirtualAddress();
  m_vertexBufferView.StrideInBytes = sizeof(Vertex);
  m_vertexBufferView.SizeInBytes = vertexBufferSize;
}

/*
 * Shader Model 6 test: the capability report and the test run. See sm6test.h and docs/test-player.md,
 * "Shader Model 6 test".
 */

#ifndef NOMINMAX
#define NOMINMAX  // std::min and std::max, not the macros of windows.h
#endif
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <wrl/client.h>

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <algorithm>
#include <chrono>
#include <map>
#include <string>
#include <thread>
#include <vector>

#include "sm6test/sm6test.h"
#include "sm6test/sm6test_checks.h"

using Microsoft::WRL::ComPtr;
using namespace sm6check;

// One test shader: the table of them is written by testing/sm6test/compile.py.
struct Sm6ShaderBlob {
  const char* name;
  const char* profile;         // cs_6_0, ...
  const char* args;            // further DXC arguments
  const unsigned char* code;   // NULL: not compiled, |note| says why
  size_t size;
  const char* note;
};

#ifdef RAV1G_SM6TEST_SHADERS
#include "sm6test_generated/shaders.h"
static const int kShaderCount = (int)(sizeof(sm6test_shaders) / sizeof(sm6test_shaders[0]));
#else
#define SM6TEST_COMPILER ""
static const Sm6ShaderBlob sm6test_shaders[1] = {{"", "", "", NULL, 0, ""}};
static const int kShaderCount = 0;
#endif

int Sm6TestShadersBuiltIn(void) { return kShaderCount > 0 ? 1 : 0; }

namespace {

struct Logger {
  Sm6LogFn fn;
  void* ctx;
  void operator()(const char* fmt, ...) const {
    char line[4096];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    line[sizeof(line) - 1] = 0;
    if (fn) fn(ctx, line);
  }
};

double MsSince(std::chrono::steady_clock::time_point start) {
  return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count();
}

// ---------------------------------------------------------------------------------------------------------
// Capability report.
//
// The feature structures are asked for by number and read as 32-bit words: every one of them is a row of
// BOOL, UINT and enum members, and the numbers and layouts do not change, so the report does not depend on
// which of them the Windows SDK of the build knows (d3d12.h; the newest here are of SDK 10.0.26100).
// ---------------------------------------------------------------------------------------------------------

enum {
  kFeatureOptions = 0,
  kFeatureFeatureLevels = 2,
  kFeatureFormatSupport = 3,
  kFeatureGpuVirtualAddress = 6,
  kFeatureShaderModel = 7,
  kFeatureOptions1 = 8,
  kFeatureRootSignature = 12,
  kFeatureArchitecture1 = 16,
  kFeatureShaderCache = 19,
  kFeatureOptions3 = 21,
  kFeatureOptions4 = 23,
  kFeatureOptions9 = 37,
  kFeatureOptions11 = 40,
  kFeatureOptions12 = 41,
  kFeatureOptions13 = 42,
  kFeatureOptions16 = 45,
  kFeatureOptions21 = 53
};

HRESULT QueryWords(ID3D12Device* device, int feature, uint32_t* words, int count) {
  return device->CheckFeatureSupport((D3D12_FEATURE)feature, words, (UINT)(count * sizeof(uint32_t)));
}

// "CAPS <label>: <name> <value>, ..." or the HRESULT of the query. A name that starts with '#' is a set
// of flags, printed in hexadecimal; a name that starts with '-' is not printed. |words| keeps what was
// read (zero when the query failed) and may carry input members.
HRESULT CapsWords(const Logger& log, ID3D12Device* device, const char* label, int feature, const char* const* names,
                  int count, uint32_t* words) {
  const HRESULT hr = QueryWords(device, feature, words, count);
  if (FAILED(hr)) {
    log("CAPS %s: query failed hr=0x%08X", label, (unsigned)hr);
    memset(words, 0, count * sizeof(uint32_t));
    return hr;
  }
  std::string text;
  for (int i = 0; i < count; ++i) {
    if (names[i][0] == '-') continue;
    if (!text.empty()) text += ", ";
    if (names[i][0] == '#')
      text += Fmt("%s 0x%X", names[i] + 1, (unsigned)words[i]);
    else
      text += Fmt("%s %d", names[i], (int)words[i]);
  }
  log("CAPS %s: %s", label, text.c_str());
  return hr;
}

std::string ShaderModelText(uint32_t model) { return Fmt("%u.%u", (unsigned)(model >> 4), (unsigned)(model & 15)); }

// The highest shader model the device reports: asked from 6.9 down, because a runtime refuses a request
// that names a shader model it does not know. 0 when every request failed.
uint32_t HighestShaderModel(ID3D12Device* device, uint32_t* asked, HRESULT* last_hr) {
  *asked = 0;
  *last_hr = S_OK;
  for (uint32_t request = 0x69; request >= 0x60; --request) {
    uint32_t word = request;
    const HRESULT hr = QueryWords(device, kFeatureShaderModel, &word, 1);
    if (SUCCEEDED(hr)) {
      *asked = request;
      return word;
    }
    *last_hr = hr;
  }
  return 0;
}

}  // namespace

void Sm6LogCaps(ID3D12Device* device, Sm6LogFn fn, void* ctx) {
  const Logger log = {fn, ctx};
  if (!device) {
    log("CAPS no D3D12 device");
    return;
  }

  // The adapter of the device, and the version of its user mode driver.
  {
    ComPtr<IDXGIFactory4> factory;
    ComPtr<IDXGIAdapter1> adapter;
    HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&factory));
    if (SUCCEEDED(hr)) hr = factory->EnumAdapterByLuid(device->GetAdapterLuid(), IID_PPV_ARGS(&adapter));
    DXGI_ADAPTER_DESC1 desc = {};
    if (SUCCEEDED(hr)) hr = adapter->GetDesc1(&desc);
    if (FAILED(hr)) {
      log("CAPS adapter: not available hr=0x%08X", (unsigned)hr);
    } else {
      std::string name;
      for (const wchar_t* w = desc.Description; *w; ++w) name += (*w >= 32 && *w < 127) ? (char)*w : '?';
      log("CAPS adapter: %s (%s), vendor 0x%04X device 0x%04X subsystem 0x%08X revision 0x%X; dedicated video "
          "memory %.0f MiB, dedicated system memory %.0f MiB, shared system memory %.0f MiB",
          name.c_str(), (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) ? "software" : "hardware", desc.VendorId,
          desc.DeviceId, desc.SubSysId, desc.Revision, (double)desc.DedicatedVideoMemory / 1048576.0,
          (double)desc.DedicatedSystemMemory / 1048576.0, (double)desc.SharedSystemMemory / 1048576.0);
      LARGE_INTEGER version = {};
      hr = adapter->CheckInterfaceSupport(__uuidof(IDXGIDevice), &version);
      if (SUCCEEDED(hr)) {
        log("CAPS driver: user mode driver version %u.%u.%u.%u", (unsigned)HIWORD(version.HighPart),
            (unsigned)LOWORD(version.HighPart), (unsigned)HIWORD(version.LowPart), (unsigned)LOWORD(version.LowPart));
      } else {
        log("CAPS driver: version not available hr=0x%08X", (unsigned)hr);
      }
    }
  }

  // Feature level.
  {
    static const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
                                               D3D_FEATURE_LEVEL_12_1, (D3D_FEATURE_LEVEL)0xc200 /* 12_2 */};
    D3D12_FEATURE_DATA_FEATURE_LEVELS data = {};
    HRESULT hr = E_FAIL;
    for (int count = 5; count >= 4 && FAILED(hr); --count) {  // an older runtime does not know 12_2
      data.NumFeatureLevels = (UINT)count;
      data.pFeatureLevelsRequested = levels;
      hr = device->CheckFeatureSupport(D3D12_FEATURE_FEATURE_LEVELS, &data, sizeof(data));
    }
    if (SUCCEEDED(hr))
      log("CAPS feature level: %d_%d", ((int)data.MaxSupportedFeatureLevel >> 12) & 15,
          ((int)data.MaxSupportedFeatureLevel >> 8) & 15);
    else
      log("CAPS feature level: query failed hr=0x%08X", (unsigned)hr);
  }

  // Shader model.
  uint32_t asked = 0;
  HRESULT model_hr = S_OK;
  const uint32_t model = HighestShaderModel(device, &asked, &model_hr);
  if (!asked) {
    log("CAPS shader model: query failed for every request from 6.9 down to 6.0, last hr=0x%08X", (unsigned)model_hr);
  } else if (asked == 0x69) {
    log("CAPS shader model: %s (the device's answer to a request for 6.9)", ShaderModelText(model).c_str());
  } else {
    log("CAPS shader model: %s (the device's answer to a request for %s; requests for more were refused, last "
        "hr=0x%08X)",
        ShaderModelText(model).c_str(), ShaderModelText(asked).c_str(), (unsigned)model_hr);
  }

  uint32_t words[16];
  static const char* const kOptions[] = {"DoublePrecisionFloatShaderOps",
                                         "-OutputMergerLogicOp",
                                         "#MinPrecisionSupport",
                                         "TiledResourcesTier",
                                         "ResourceBindingTier",
                                         "-PSSpecifiedStencilRefSupported",
                                         "TypedUAVLoadAdditionalFormats",
                                         "-ROVsSupported",
                                         "-ConservativeRasterizationTier",
                                         "MaxGPUVirtualAddressBitsPerResource",
                                         "StandardSwizzle64KBSupported",
                                         "-CrossNodeSharingTier",
                                         "-CrossAdapterRowMajorTextureSupported",
                                         "-VPAndRTArrayIndexFromAnyShaderFeedingRasterizerSupportedWithoutGSEmulation",
                                         "ResourceHeapTier"};
  CapsWords(log, device, "options", kFeatureOptions, kOptions, 15, words);

  // Wave operations.
  static const char* const kOptions1[] = {"WaveOps",        "WaveLaneCountMin",              "WaveLaneCountMax",
                                          "TotalLaneCount", "ExpandedComputeResourceStates", "Int64ShaderOps"};
  uint32_t options1[6];
  const HRESULT options1_hr = CapsWords(log, device, "options1", kFeatureOptions1, kOptions1, 6, options1);

  static const char* const kOptions3[] = {"CopyQueueTimestampQueriesSupported", "CastingFullyTypedFormatSupported",
                                          "#WriteBufferImmediateSupportFlags", "-ViewInstancingTier",
                                          "-BarycentricsSupported"};
  CapsWords(log, device, "options3", kFeatureOptions3, kOptions3, 5, words);

  // Native 16-bit types.
  static const char* const kOptions4[] = {"-MSAA64KBAlignedTextureSupported", "SharedResourceCompatibilityTier",
                                          "Native16BitShaderOpsSupported"};
  CapsWords(log, device, "options4", kFeatureOptions4, kOptions4, 3, words);

  static const char* const kOptions9[] = {"-MeshShaderPipelineStatsSupported",
                                          "-MeshShaderSupportsFullRangeRenderTargetArrayIndex",
                                          "AtomicInt64OnTypedResourceSupported",
                                          "AtomicInt64OnGroupSharedSupported",
                                          "-DerivativesInMeshAndAmplificationShadersSupported",
                                          "WaveMMATier"};
  CapsWords(log, device, "options9", kFeatureOptions9, kOptions9, 6, words);
  static const char* const kOptions11[] = {"AtomicInt64OnDescriptorHeapResourceSupported"};
  CapsWords(log, device, "options11", kFeatureOptions11, kOptions11, 1, words);
  static const char* const kOptions12[] = {"-MSPrimitivesPipelineStatisticIncludesCulledPrimitives",
                                           "EnhancedBarriersSupported", "RelaxedFormatCastingSupported"};
  CapsWords(log, device, "options12", kFeatureOptions12, kOptions12, 3, words);
  static const char* const kOptions13[] = {"UnrestrictedBufferTextureCopyPitchSupported",
                                           "-UnrestrictedVertexElementAlignmentSupported",
                                           "-InvertedViewportHeightFlipsYSupported",
                                           "-InvertedViewportDepthFlipsZSupported",
                                           "TextureCopyBetweenDimensionsSupported",
                                           "-AlphaBlendFactorSupported"};
  CapsWords(log, device, "options13", kFeatureOptions13, kOptions13, 6, words);
  static const char* const kOptions16[] = {"-DynamicDepthBiasSupported", "GPUUploadHeapSupported"};
  CapsWords(log, device, "options16", kFeatureOptions16, kOptions16, 2, words);
  // What Shader Model 6.8 brought to the options (Windows SDK 10.0.26100).
  static const char* const kOptions21[] = {"WorkGraphsTier", "ExecuteIndirectTier",
                                           "SampleCmpGradientAndBiasSupported", "ExtendedCommandInfoSupported"};
  CapsWords(log, device, "options21", kFeatureOptions21, kOptions21, 4, words);

  // A wave size chosen by the shader has no member of its own in the options: [WaveSize(n)] is part of
  // Shader Model 6.6 and the range form [WaveSize(min, max, preferred)] of 6.8, and the size has to be one
  // the device has (WaveLaneCountMin..WaveLaneCountMax). The wavesize tests of -sm6test try it.
  {
    std::string lanes = FAILED(options1_hr)
                            ? std::string("lane counts not known")
                            : Fmt("the device reports waves of %u to %u lanes", (unsigned)options1[1],
                                  (unsigned)options1[2]);
    log("CAPS wave size: [WaveSize(n)] needs shader model 6.6: %s; [WaveSize(min, max, preferred)] needs 6.8: %s; "
        "%s; wave operations %s",
        !asked ? "not known" : model >= 0x66 ? "reported" : "not reported",
        !asked ? "not known" : model >= 0x68 ? "reported" : "not reported", lanes.c_str(),
        FAILED(options1_hr) ? "not known" : options1[0] ? "reported" : "not reported");
  }

  static const char* const kArchitecture1[] = {"-NodeIndex", "TileBasedRenderer", "UMA", "CacheCoherentUMA",
                                               "IsolatedMMU"};
  memset(words, 0, sizeof(words));
  CapsWords(log, device, "architecture", kFeatureArchitecture1, kArchitecture1, 5, words);
  static const char* const kAddress[] = {"MaxGPUVirtualAddressBitsPerResource", "MaxGPUVirtualAddressBitsPerProcess"};
  CapsWords(log, device, "gpu virtual address", kFeatureGpuVirtualAddress, kAddress, 2, words);
  static const char* const kShaderCache[] = {"#SupportFlags"};
  CapsWords(log, device, "shader cache", kFeatureShaderCache, kShaderCache, 1, words);

  // Root signature version: asked from 1.2 down, as the shader model is.
  {
    HRESULT hr = E_FAIL;
    uint32_t version = 0;
    for (uint32_t request = 3; request >= 1 && FAILED(hr); --request) {
      version = request;
      hr = QueryWords(device, kFeatureRootSignature, &version, 1);
    }
    if (SUCCEEDED(hr))
      log("CAPS root signature: highest version 1.%u", (unsigned)(version - 1));
    else
      log("CAPS root signature: query failed hr=0x%08X", (unsigned)hr);
  }

  // Loads and stores through typed UAVs, for the formats a decoder has samples in.
  {
    static const struct {
      const char* name;
      DXGI_FORMAT format;
    } kFormats[] = {{"R8_UNORM", DXGI_FORMAT_R8_UNORM},
                    {"R8_UINT", DXGI_FORMAT_R8_UINT},
                    {"R16_UNORM", DXGI_FORMAT_R16_UNORM},
                    {"R16_UINT", DXGI_FORMAT_R16_UINT},
                    {"R16_SINT", DXGI_FORMAT_R16_SINT},
                    {"R32_UINT", DXGI_FORMAT_R32_UINT},
                    {"R32_SINT", DXGI_FORMAT_R32_SINT},
                    {"R32_FLOAT", DXGI_FORMAT_R32_FLOAT},
                    {"R8G8_UINT", DXGI_FORMAT_R8G8_UINT},
                    {"R16G16_UINT", DXGI_FORMAT_R16G16_UINT},
                    {"R8G8B8A8_UNORM", DXGI_FORMAT_R8G8B8A8_UNORM},
                    {"R8G8B8A8_UINT", DXGI_FORMAT_R8G8B8A8_UINT},
                    {"R16G16B16A16_UINT", DXGI_FORMAT_R16G16B16A16_UINT},
                    {"R10G10B10A2_UNORM", DXGI_FORMAT_R10G10B10A2_UNORM},
                    {"R10G10B10A2_UINT", DXGI_FORMAT_R10G10B10A2_UINT}};
    std::string text;
    for (size_t i = 0; i < sizeof(kFormats) / sizeof(kFormats[0]); ++i) {
      uint32_t support[3] = {(uint32_t)kFormats[i].format, 0, 0};
      const HRESULT hr = QueryWords(device, kFeatureFormatSupport, support, 3);
      if (!text.empty()) text += ", ";
      if (FAILED(hr))
        text += Fmt("%s hr=0x%08X", kFormats[i].name, (unsigned)hr);
      else  // D3D12_FORMAT_SUPPORT2_UAV_TYPED_LOAD 0x40, _STORE 0x80
        text += Fmt("%s %d/%d", kFormats[i].name, (support[2] & 0x40) ? 1 : 0, (support[2] & 0x80) ? 1 : 0);
    }
    log("CAPS typed UAV load/store: %s", text.c_str());
  }

  if (kShaderCount)
    log("CAPS sm6test: %d test shaders built in, compiled with %s", kShaderCount, SM6TEST_COMPILER);
  else
    log("CAPS sm6test: the test shaders are not built in (the build was not given /p:Rav1gSm6TestShaders)");
}

// ---------------------------------------------------------------------------------------------------------
// The test run.
// ---------------------------------------------------------------------------------------------------------

namespace {

enum {
  kInputBytes = 16 * 1024,  // the largest input is 1024 words
  kOutputBytes = 32 * 1024, // the largest output is 256 records of 24 words
  kWaitSeconds = 60
};

// What a word of the output holds that no thread wrote.
const uint32_t kSentinel = 0xDEADBEEFu;

class Runner {
 public:
  Runner(ID3D12Device* device, ID3D12CommandQueue* queue) : device_(device), queue_(queue) {}

  // Root signature, queue (when none was given), allocator, command list, fence and the four buffers.
  // Empty when all of it exists, else what failed.
  std::string Init() {
    HRESULT hr;
    {
      // The root signature of the decoder's shaders, in small: buffers as root descriptors.
      D3D12_ROOT_PARAMETER params[2] = {};
      params[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_SRV;
      params[0].Descriptor.ShaderRegister = 0;
      params[0].ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
      params[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_UAV;
      params[1].Descriptor.ShaderRegister = 0;
      params[1].ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
      D3D12_ROOT_SIGNATURE_DESC desc = {};
      desc.NumParameters = 2;
      desc.pParameters = params;
      ComPtr<ID3DBlob> blob;
      ComPtr<ID3DBlob> errors;
      hr = D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &blob, &errors);
      if (FAILED(hr)) return Fmt("D3D12SerializeRootSignature hr=0x%08X", (unsigned)hr);
      hr = device_->CreateRootSignature(0, blob->GetBufferPointer(), blob->GetBufferSize(), IID_PPV_ARGS(&root_));
      if (FAILED(hr)) return Fmt("CreateRootSignature hr=0x%08X", (unsigned)hr);
    }
    if (!queue_) {
      D3D12_COMMAND_QUEUE_DESC desc = {};
      desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;  // what the decoder creates for itself
      hr = device_->CreateCommandQueue(&desc, IID_PPV_ARGS(&queue_));
      if (FAILED(hr)) return Fmt("CreateCommandQueue hr=0x%08X", (unsigned)hr);
    }
    const D3D12_COMMAND_LIST_TYPE type = queue_->GetDesc().Type;
    hr = device_->CreateCommandAllocator(type, IID_PPV_ARGS(&allocator_));
    if (FAILED(hr)) return Fmt("CreateCommandAllocator hr=0x%08X", (unsigned)hr);
    hr = device_->CreateCommandList(0, type, allocator_.Get(), NULL, IID_PPV_ARGS(&list_));
    if (FAILED(hr)) return Fmt("CreateCommandList hr=0x%08X", (unsigned)hr);
    list_->Close();
    hr = device_->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence_));
    if (FAILED(hr)) return Fmt("CreateFence hr=0x%08X", (unsigned)hr);

    // upload: the input of a test, and behind it the sentinel words the output is filled with before a
    // dispatch. input and output: what the shader reads and writes. readback: the output for the host.
    hr = CreateBuffer(D3D12_HEAP_TYPE_UPLOAD, kInputBytes + kOutputBytes, D3D12_RESOURCE_FLAG_NONE,
                      D3D12_RESOURCE_STATE_GENERIC_READ, &upload_);
    if (FAILED(hr)) return Fmt("upload buffer hr=0x%08X", (unsigned)hr);
    hr = CreateBuffer(D3D12_HEAP_TYPE_DEFAULT, kInputBytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST,
                      &input_);
    if (FAILED(hr)) return Fmt("input buffer hr=0x%08X", (unsigned)hr);
    hr = CreateBuffer(D3D12_HEAP_TYPE_DEFAULT, kOutputBytes, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                      D3D12_RESOURCE_STATE_COPY_DEST, &output_);
    if (FAILED(hr)) return Fmt("output buffer hr=0x%08X", (unsigned)hr);
    hr = CreateBuffer(D3D12_HEAP_TYPE_READBACK, kOutputBytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST,
                      &readback_);
    if (FAILED(hr)) return Fmt("readback buffer hr=0x%08X", (unsigned)hr);
    hr = upload_->Map(0, NULL, &upload_mem_);
    if (FAILED(hr)) return Fmt("Map of the upload buffer hr=0x%08X", (unsigned)hr);
    uint32_t* sentinel = reinterpret_cast<uint32_t*>(static_cast<uint8_t*>(upload_mem_) + kInputBytes);
    for (int i = 0; i < kOutputBytes / 4; ++i) sentinel[i] = kSentinel;
    return std::string();
  }

  ~Runner() {
    if (upload_ && upload_mem_) upload_->Unmap(0, NULL);
  }

  enum Outcome { kRan, kPsoFailed, kBroken };

  // The pipeline state of |shader|, one dispatch of 4 thread groups on |input|, and the output read back.
  // kPsoFailed: the device refused the pipeline state, |hr| says how. kBroken: |error| says what.
  Outcome Run(const Sm6ShaderBlob& shader, const std::vector<uint32_t>& input, std::vector<uint32_t>* output,
              HRESULT* hr, std::string* error, double* pso_ms, double* run_ms) {
    *pso_ms = 0.0;
    *run_ms = 0.0;
    ComPtr<ID3D12PipelineState> pso;
    D3D12_COMPUTE_PIPELINE_STATE_DESC desc = {};
    desc.pRootSignature = root_.Get();
    desc.CS.pShaderBytecode = shader.code;
    desc.CS.BytecodeLength = shader.size;
    const auto pso_start = std::chrono::steady_clock::now();
    *hr = device_->CreateComputePipelineState(&desc, IID_PPV_ARGS(&pso));
    *pso_ms = MsSince(pso_start);
    if (FAILED(*hr)) {
      const HRESULT removed = device_->GetDeviceRemovedReason();
      if (FAILED(removed)) {
        *error = Fmt("the device was removed by the pipeline state creation (hr=0x%08X), reason 0x%08X",
                     (unsigned)*hr, (unsigned)removed);
        return kBroken;
      }
      return kPsoFailed;
    }

    memset(upload_mem_, 0, kInputBytes);
    memcpy(upload_mem_, input.data(), (std::min)(input.size() * sizeof(uint32_t), (size_t)kInputBytes));

    const auto run_start = std::chrono::steady_clock::now();
    *hr = allocator_->Reset();
    if (SUCCEEDED(*hr)) *hr = list_->Reset(allocator_.Get(), NULL);
    if (FAILED(*hr)) {
      *error = Fmt("command list reset hr=0x%08X", (unsigned)*hr);
      return kBroken;
    }
    list_->CopyBufferRegion(input_.Get(), 0, upload_.Get(), 0, kInputBytes);
    list_->CopyBufferRegion(output_.Get(), 0, upload_.Get(), kInputBytes, kOutputBytes);
    Transition(input_.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    Transition(output_.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    list_->SetComputeRootSignature(root_.Get());
    list_->SetPipelineState(pso.Get());
    list_->SetComputeRootShaderResourceView(0, input_->GetGPUVirtualAddress());
    list_->SetComputeRootUnorderedAccessView(1, output_->GetGPUVirtualAddress());
    list_->Dispatch(kGroups, 1, 1);
    Transition(input_.Get(), D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    Transition(output_.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
    list_->CopyBufferRegion(readback_.Get(), 0, output_.Get(), 0, kOutputBytes);
    Transition(output_.Get(), D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    *hr = list_->Close();
    if (FAILED(*hr)) {
      *error = Fmt("command list close hr=0x%08X", (unsigned)*hr);
      return kBroken;
    }
    ID3D12CommandList* lists[] = {list_.Get()};
    queue_->ExecuteCommandLists(1, lists);
    const UINT64 value = ++fence_value_;
    *hr = queue_->Signal(fence_.Get(), value);
    if (FAILED(*hr)) {
      *error = Fmt("queue signal hr=0x%08X", (unsigned)*hr);
      return kBroken;
    }
    // Polled: the test has no use for an event, and a device that was removed completes every fence.
    while (fence_->GetCompletedValue() < value) {
      if (MsSince(run_start) > kWaitSeconds * 1000.0) {
        *error = Fmt("the dispatch did not finish within %d s", (int)kWaitSeconds);
        return kBroken;
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    const HRESULT removed = device_->GetDeviceRemovedReason();
    if (FAILED(removed)) {
      *error = Fmt("the device was removed, reason 0x%08X", (unsigned)removed);
      return kBroken;
    }
    *run_ms = MsSince(run_start);

    void* mem = NULL;
    const D3D12_RANGE read = {0, (SIZE_T)kOutputBytes};
    *hr = readback_->Map(0, &read, &mem);
    if (FAILED(*hr)) {
      *error = Fmt("Map of the readback buffer hr=0x%08X", (unsigned)*hr);
      return kBroken;
    }
    output->resize(kOutputBytes / 4);
    memcpy(output->data(), mem, kOutputBytes);
    const D3D12_RANGE none = {0, 0};
    readback_->Unmap(0, &none);
    return kRan;
  }

 private:
  HRESULT CreateBuffer(D3D12_HEAP_TYPE heap_type, UINT64 size, D3D12_RESOURCE_FLAGS flags, D3D12_RESOURCE_STATES state,
                       ComPtr<ID3D12Resource>* out) {
    D3D12_HEAP_PROPERTIES heap = {};
    heap.Type = heap_type;
    D3D12_RESOURCE_DESC desc = {};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    desc.Width = size;
    desc.Height = 1;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.Format = DXGI_FORMAT_UNKNOWN;
    desc.SampleDesc.Count = 1;
    desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    desc.Flags = flags;
    return device_->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, NULL,
                                            IID_PPV_ARGS(out->ReleaseAndGetAddressOf()));
  }

  void Transition(ID3D12Resource* resource, D3D12_RESOURCE_STATES before, D3D12_RESOURCE_STATES after) {
    D3D12_RESOURCE_BARRIER barrier = {};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition.pResource = resource;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = before;
    barrier.Transition.StateAfter = after;
    list_->ResourceBarrier(1, &barrier);
  }

  ComPtr<ID3D12Device> device_;
  ComPtr<ID3D12CommandQueue> queue_;
  ComPtr<ID3D12RootSignature> root_;
  ComPtr<ID3D12CommandAllocator> allocator_;
  ComPtr<ID3D12GraphicsCommandList> list_;
  ComPtr<ID3D12Fence> fence_;
  ComPtr<ID3D12Resource> upload_;
  ComPtr<ID3D12Resource> input_;
  ComPtr<ID3D12Resource> output_;
  ComPtr<ID3D12Resource> readback_;
  void* upload_mem_ = NULL;
  UINT64 fence_value_ = 0;
};

enum Kind { kBasic, kWave, kReduce, kReduceNoLanes, kNative16Math, kNative16Load };

struct TestDef {
  const char* name;  // of the shader in the table
  Kind kind;
  int lanes_min;     // [WaveSize]: the lane counts the shader asked for; 0: it did not ask
  int lanes_max;
};

const TestDef kTests[] = {
    {"basic", kBasic, 0, 0},
    {"wave_ops", kWave, 0, 0},
    {"reduce_groupshared", kReduceNoLanes, 0, 0},
    {"reduce_wave", kReduce, 0, 0},
    {"native16_math", kNative16Math, 0, 0},
    {"native16_load", kNative16Load, 0, 0},
    {"wavesize4", kWave, 4, 4},
    {"wavesize8", kWave, 8, 8},
    {"wavesize16", kWave, 16, 16},
    {"wavesize32", kWave, 32, 32},
    {"wavesize64", kWave, 64, 64},
    {"reduce_wave64", kReduce, 0, 0},
    {"wavesize_range_pref32", kWave, 4, 128},
    {"wavesize_range_pref64", kWave, 4, 128},
};

const Sm6ShaderBlob* FindShader(const char* name) {
  for (int i = 0; i < kShaderCount; ++i)
    if (!strcmp(sm6test_shaders[i].name, name)) return &sm6test_shaders[i];
  return NULL;
}

// The validator's hash, bytes 4..19 of the container: zero in bytecode that was not signed.
bool HasValidationHash(const Sm6ShaderBlob& shader) {
  if (shader.size < 20) return false;
  for (int i = 4; i < 20; ++i)
    if (shader.code[i]) return true;
  return false;
}

}  // namespace

int Sm6RunTests(ID3D12Device* device, ID3D12CommandQueue* queue, Sm6LogFn fn, void* ctx) {
  const Logger log = {fn, ctx};
  const int test_count = (int)(sizeof(kTests) / sizeof(kTests[0]));
  if (!device) {
    log("SM6TEST not run: no D3D12 device");
    return 1;
  }
  if (!kShaderCount) {
    log("SM6TEST not run: the test shaders are not built in. Compile them with testing/sm6test/compile.py and "
        "build with /p:Rav1gSm6TestShaders=<its output directory>");
    return 1;
  }
  uint32_t asked = 0;
  HRESULT model_hr = S_OK;
  const uint32_t model = HighestShaderModel(device, &asked, &model_hr);
  log("SM6TEST start: %d tests, shaders compiled with %s; the device reports shader model %s", test_count,
      SM6TEST_COMPILER, asked ? ShaderModelText(model).c_str() : "(query failed)");

  Runner runner(device, queue);
  const std::string init_error = runner.Init();
  if (!init_error.empty()) {
    log("SM6TEST not run: %s", init_error.c_str());
    return 1;
  }
  log("SM6TEST root signature, command list and buffers created");

  int passed = 0, failed = 0, refused = 0, not_built = 0, broken = 0;
  std::string refused_names, failed_names;
  for (int i = 0; i < test_count; ++i) {
    const TestDef& test = kTests[i];
    const Sm6ShaderBlob* shader = FindShader(test.name);
    if (!shader) {
      log("SM6TEST %s: NOT BUILT | the shader is not in the table of this build", test.name);
      ++broken;
      continue;
    }
    if (!shader->code) {
      // A shader the DXC of the build could not compile (compile.py lets that pass for the optional ones).
      log("SM6TEST %s: NOT BUILT | %s; %s", test.name, shader->profile, shader->note);
      ++not_built;
      continue;
    }
    const std::string what = Fmt("%s%s%s, %u bytes, validation hash %s", shader->profile, shader->args[0] ? " " : "",
                                 shader->args, (unsigned)shader->size, HasValidationHash(*shader) ? "present" : "ABSENT");
    std::vector<uint32_t> input;
    switch (test.kind) {
      case kBasic: input = BasicInput(); break;
      case kWave: input = WaveInput(); break;
      case kReduce:
      case kReduceNoLanes: input = ReduceInput(); break;
      case kNative16Math: input = MathInput(); break;
      case kNative16Load: input = LoadInput(); break;
    }
    std::vector<uint32_t> output;
    HRESULT hr = S_OK;
    std::string error;
    double pso_ms = 0.0, run_ms = 0.0;
    const Runner::Outcome outcome = runner.Run(*shader, input, &output, &hr, &error, &pso_ms, &run_ms);
    if (outcome == Runner::kPsoFailed) {
      log("SM6TEST %s: PSO FAILED hr=0x%08X | %s; CreateComputePipelineState refused the shader after %.0f ms",
          test.name, (unsigned)hr, what.c_str(), pso_ms);
      ++refused;
      refused_names += std::string(refused_names.empty() ? "" : ", ") + test.name;
      continue;
    }
    if (outcome == Runner::kBroken) {
      log("SM6TEST %s: BROKEN | %s; %s", test.name, what.c_str(), error.c_str());
      ++broken;
      // A device that is gone, or a queue that does not answer, takes no further test.
      log("SM6TEST stopped: %d tests not run", test_count - i - 1);
      broken += test_count - i - 1;
      break;
    }
    Verdict verdict;
    switch (test.kind) {
      case kBasic: CheckBasic(input, output, &verdict); break;
      case kWave: CheckWave(input, output, test.lanes_min, test.lanes_max, &verdict); break;
      case kReduce: CheckReduce(input, output, true, &verdict); break;
      case kReduceNoLanes: CheckReduce(input, output, false, &verdict); break;
      case kNative16Math: CheckNative16Math(input, output, &verdict); break;
      case kNative16Load: CheckNative16Load(input, output, &verdict); break;
    }
    log("SM6TEST %s: %s | %s; %s; %s; pipeline state %.0f ms, dispatch and readback %.0f ms", test.name,
        verdict.mismatches ? "FAIL" : "PASS", what.c_str(), verdict.details.c_str(), verdict.MismatchText().c_str(),
        pso_ms, run_ms);
    if (verdict.mismatches) {
      ++failed;
      failed_names += std::string(failed_names.empty() ? "" : ", ") + test.name;
    } else {
      ++passed;
    }
  }
  log("SM6TEST summary: %d tests: %d PASS, %d FAIL%s%s%s, %d PSO FAILED%s%s%s, %d not built, %d broken", test_count,
      passed, failed, failed ? " (" : "", failed_names.c_str(), failed ? ")" : "", refused, refused ? " (" : "",
      refused_names.c_str(), refused ? ")" : "", not_built, broken);
  return broken ? 1 : 0;
}

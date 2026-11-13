/*
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */

// The functions of rav1g.h on the C++ library (libav1, the host of libav1/dx), for the test player
// that is built with it: the reference the player with the Rust library is timed against
// (docs/rust-port.md, step 5). The player's own code is the same in both; this file is the
// difference, and it does what the player did itself before: the same calls of aom_codec_* and
// av1_*, in the same order. It goes when the C++ host leaves the tree.
//
// Not compiled into a player that links the Rust library, which has these functions itself.

#include <stdint.h>
#include <string.h>
#include <string>
#include <vector>

#include <d3d12.h>

#include "rav1g.h"
#include "aom/aom_codec.h"
#include "aom/aom_decoder.h"
#include "aom/aomdx.h"
#include "config/aom_config.h"
#include "dx/av1_perf.h"

static_assert(RAV1G_STATUS_NEEDS_RECONFIGURE == (int)AOM_CODEC_NEEDS_RECONFIGURE, "the status values are aom_codec_err_t");
static_assert(RAV1G_STATUS_INVALID_PARAM == (int)AOM_CODEC_INVALID_PARAM, "the status values are aom_codec_err_t");
static_assert(RAV1G_IMAGE_FORMAT_PACKED10X3 == (int)fbt10x3, "the image formats are frame_buffer_type");
static_assert(RAV1G_LOW_MEMORY_ALL == AV1_LOW_MEMORY_ALL, "the low memory parts are AV1_LOW_MEMORY_*");
static_assert(RAV1G_INTER_FAST_LISTS_ONLY == AV1_INTER_FAST_LISTS_ONLY, "the inter modes are AV1_INTER_FAST_*");
static_assert(RAV1G_POISON_BUFFERS == AV1_POISON_BUFFERS, "the fill levels are AV1_POISON_*");
static_assert(RAV1G_MEMORY_GUARD_MARGIN == AV1_MEMORY_GUARD_MARGIN, "the guard margin is AV1_MEMORY_GUARD_MARGIN");

struct Rav1gDevice {
  ID3D12Device* device;
  // Null when the application gave none: every decoder then makes a direct queue of its own, as the
  // library always did for the player, which gives none.
  ID3D12CommandQueue* queue;
  // av1_create_pipeline_cache_handle
  void* pipelines;
};

struct Rav1gDecoder {
  aom_codec_ctx_t ctx;
  std::vector<uint8_t> host_memory;
  Rav1gOutput output;
  int64_t (*memory_available)(void* context);
  void* memory_available_context;
  std::string message;
  std::string detail;
};

static thread_local std::string last_error;

static Rav1gStatus status_of(aom_codec_err_t code) { return (Rav1gStatus)(int)code; }

const char* rav1g_last_error(void) { return last_error.c_str(); }

const char* rav1g_version(void) { return "libav1, the C++ host (the reference build), behind rav1g.h"; }

Rav1gStatus rav1g_peek_stream_info(const uint8_t* data, size_t size, Rav1gStreamInfo* info) {
  if (!data || !info) return RAV1G_STATUS_INVALID_PARAM;
  aom_codec_stream_info_t si;
  memset(&si, 0, sizeof(si));
  const aom_codec_err_t code = aom_codec_peek_stream_info(aom_codec_av1_dx(), data, size, &si);
  if (code != AOM_CODEC_OK) return status_of(code);
  info->width = si.w;
  info->height = si.h;
  info->bit_depth = si.bit_depth;
  info->is_key_frame = si.is_kf != 0;
  return RAV1G_STATUS_OK;
}

Rav1gDevice* rav1g_device_create(void* d3d12_device, void* d3d12_queue, uint32_t pipeline_threads) {
  if (!d3d12_device) {
    last_error = "the reference build needs the application's D3D12 device";
    return NULL;
  }
  void* pipelines = av1_create_pipeline_cache_handle(d3d12_device, (int)pipeline_threads);
  if (!pipelines) {
    last_error = "the compute pipeline states could not be created";
    return NULL;
  }
  Rav1gDevice* device = new Rav1gDevice;
  device->device = static_cast<ID3D12Device*>(d3d12_device);
  device->queue = static_cast<ID3D12CommandQueue*>(d3d12_queue);
  device->device->AddRef();
  if (device->queue) device->queue->AddRef();
  device->pipelines = pipelines;
  return device;
}

Rav1gStatus rav1g_device_wait_pipelines(const Rav1gDevice* device) {
  if (!device) return RAV1G_STATUS_INVALID_PARAM;
  if (av1_wait_pipeline_cache_handle(device->pipelines) == 0) return RAV1G_STATUS_OK;
  last_error = "a compute pipeline state could not be created";
  return RAV1G_STATUS_ERROR;
}

void* rav1g_device_d3d12_device(const Rav1gDevice* device) { return device ? device->device : NULL; }

void* rav1g_device_d3d12_queue(const Rav1gDevice* device) { return device ? device->queue : NULL; }

// The player destroys its decoders before the device, so the pipeline states go here.
void rav1g_device_destroy(Rav1gDevice* device) {
  if (!device) return;
  av1_destroy_pipeline_cache_handle(device->pipelines);
  if (device->queue) device->queue->Release();
  device->device->Release();
  delete device;
}

void rav1g_log_video_memory(void* d3d12_device, const char* label) { av1_log_video_memory_now(d3d12_device, label); }

void rav1g_settings_default(Rav1gSettings* settings) {
  if (!settings) return;
  memset(settings, 0, sizeof(*settings));
  settings->width = 1920;
  settings->height = 1088;
  settings->bit_depth = 8;
  settings->threads = 1;
  settings->low_memory = RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER;
}

// The configuration the player filled in itself before.
static void config_of(const Rav1gSettings* settings, aom_codec_dec_cfg_t* cfg) {
  memset(cfg, 0, sizeof(*cfg));
  cfg->width = settings->width;
  cfg->height = settings->height;
  cfg->bitdepth = settings->bit_depth;
  cfg->threads = settings->threads;
  cfg->cfg.ext_partition = 1;
  cfg->allow_lowbitdepth = CONFIG_LOWBITDEPTH;
  cfg->host_size = 0;
  cfg->tryHDR10x3 = settings->packed_10x3 ? 1 : 0;
  cfg->frame_slots = (int)settings->frame_slots;
  cfg->intra_v2 = settings->intra_v2 ? 1 : 0;
  cfg->inter_fast = settings->inter_fast ? 1 : 0;
  // 0 in the configuration is the library's default, not "none".
  const uint32_t parts = settings->low_memory & RAV1G_LOW_MEMORY_ALL;
  cfg->low_memory = parts ? (int)parts : AV1_LOW_MEMORY_NONE;
}

void rav1g_memory_requirements(const Rav1gSettings* settings, Rav1gMemoryRequirements* requirements) {
  if (!settings || !requirements) return;
  aom_codec_dec_cfg_t cfg;
  config_of(settings, &cfg);
  av1_codec_query_memory_requirements(&cfg);
  requirements->host_size = cfg.host_size;
  requirements->device_size = cfg.device_size;
  requirements->device_size_deferred = cfg.device_size_deferred;
}

// The callbacks of the library; the context is the decoder.

static int get_out_buffer(void* priv, size_t buf_size, uint16_t w, uint16_t h, frame_buffer_type fb_type,
                          av1_decoded_frame_buffer_t* fb) {
  Rav1gDecoder* decoder = static_cast<Rav1gDecoder*>(priv);
  Rav1gImageRequest request;
  request.buffer_size = buf_size;
  request.width = w;
  request.height = h;
  request.format = (Rav1gImageFormat)(int)fb_type;
  Rav1gImage* image = new Rav1gImage;
  memset(image, 0, sizeof(*image));
  if (decoder->output.get_image(decoder->output.context, &request, image) != 0) {
    delete image;
    return -1;
  }
  fb->dx12_buffer = image->d3d12_buffer;
  fb->buffer_host_ptr = image->memory;
  fb->buffer_size = image->buffer_size;
  fb->dx12_texture[0] = image->d3d12_textures[0];
  fb->dx12_texture[1] = image->d3d12_textures[1];
  fb->dx12_texture[2] = image->d3d12_textures[2];
  // The image travels with the frame and comes back in notify_frame_ready or release_out_buffer.
  fb->priv = image;
  return 0;
}

static int release_out_buffer(void* priv, void* buffer_priv) {
  Rav1gDecoder* decoder = static_cast<Rav1gDecoder*>(priv);
  Rav1gImage* image = static_cast<Rav1gImage*>(buffer_priv);
  if (!image) return -1;
  if (decoder->output.unused) decoder->output.unused(decoder->output.context, image->token);
  delete image;
  return 0;
}

static int notify_frame_ready(void* priv, void* img_ptr) {
  Rav1gDecoder* decoder = static_cast<Rav1gDecoder*>(priv);
  if (!img_ptr) {
    if (decoder->output.end_of_frames) decoder->output.end_of_frames(decoder->output.context);
    return 0;
  }
  const aom_image_t* img = static_cast<const aom_image_t*>(img_ptr);
  Rav1gImage* image = static_cast<Rav1gImage*>(img->fb2_priv);
  Rav1gFrame frame;
  memset(&frame, 0, sizeof(frame));
  frame.image = *image;
  frame.width = img->d_w;
  frame.height = img->d_h;
  frame.bit_depth = img->bit_depth;
  frame.monochrome = img->monochrome != 0;
  frame.packed_10x3 = img->is_hdr10x3 != 0 && img->bit_depth > 8;
  frame.user_data = (uint64_t)(uintptr_t)img->user_priv;
  for (int plane = 0; plane < 3; plane++) {
    const bool has_plane = image->memory && (plane == 0 || !img->monochrome);
    frame.planes[plane] = has_plane ? img->planes[plane] : NULL;
    frame.strides[plane] = (uint32_t)img->stride[plane];
  }
  // The C++ library does not hand its fences out.
  frame.d3d12_fence = NULL;
  frame.fence_value = 0;
  delete image;
  decoder->output.frame(decoder->output.context, &frame);
  return 0;
}

static int64_t memory_available(void* priv) {
  Rav1gDecoder* decoder = static_cast<Rav1gDecoder*>(priv);
  return decoder->memory_available(decoder->memory_available_context);
}

Rav1gStatus rav1g_decoder_create(const Rav1gDevice* device, const Rav1gSettings* settings, Rav1gDecoder** out) {
  if (!out) return RAV1G_STATUS_INVALID_PARAM;
  *out = NULL;
  if (!device || !settings) return RAV1G_STATUS_INVALID_PARAM;
  if (!settings->output.get_image || !settings->output.frame) {
    last_error = "the output has no get_image or no frame callback";
    return RAV1G_STATUS_INVALID_PARAM;
  }
  Rav1gDecoder* decoder = new Rav1gDecoder;
  memset(&decoder->ctx, 0, sizeof(decoder->ctx));
  decoder->output = settings->output;
  decoder->memory_available = settings->memory_available;
  decoder->memory_available_context = settings->memory_available_context;
  aom_codec_dec_cfg_t cfg;
  config_of(settings, &cfg);
  cfg.out_buffers_cb.get_out_buffer_cb = get_out_buffer;
  cfg.out_buffers_cb.release_out_buffer_cb = release_out_buffer;
  cfg.out_buffers_cb.notify_frame_ready_cb = notify_frame_ready;
  cfg.out_buffers_cb.out_buffers_priv = decoder;
  // Without a callback there is no budget guard.
  cfg.memory_available_cb = settings->memory_available ? memory_available : NULL;
  cfg.memory_available_priv = decoder;
  av1_codec_query_memory_requirements(&cfg);
  decoder->host_memory.resize(cfg.host_size, 0);
  cfg.host_memory = decoder->host_memory.data();
  cfg.host_size = decoder->host_memory.size();
  cfg.dx12device = device->device;
  cfg.dx12command_queue = device->queue;
  cfg.dxPsos = device->pipelines;
  const aom_codec_err_t code = aom_codec_dec_init(&decoder->ctx, aom_codec_av1_dx(), &cfg, 0);
  if (code != AOM_CODEC_OK) {
    const char* message = aom_codec_error(&decoder->ctx);
    const char* detail = aom_codec_error_detail(&decoder->ctx);
    last_error = std::string("decoder creation failed: ") + (message ? message : "?");
    if (detail && detail[0]) last_error += std::string(": ") + detail;
    delete decoder;
    return status_of(code);
  }
  aom_codec_control(&decoder->ctx, AV1D_SET_OUTPUT_ALL_LAYERS, settings->output_all_layers ? 1 : 0);
  aom_codec_control(&decoder->ctx, AV1D_SET_OPERATING_POINT, (int)settings->operating_point);
  *out = decoder;
  return RAV1G_STATUS_OK;
}

static Rav1gStatus keep(Rav1gDecoder* decoder, aom_codec_err_t code) {
  if (code != AOM_CODEC_OK) {
    const char* message = aom_codec_error(&decoder->ctx);
    const char* detail = aom_codec_error_detail(&decoder->ctx);
    decoder->message = message ? message : "";
    decoder->detail = detail ? detail : "";
  }
  return status_of(code);
}

Rav1gStatus rav1g_decoder_decode(Rav1gDecoder* decoder, const uint8_t* data, size_t size, uint64_t user_data) {
  if (!decoder || !data) return RAV1G_STATUS_INVALID_PARAM;
  return keep(decoder, aom_codec_decode(&decoder->ctx, data, size, (void*)(uintptr_t)user_data));
}

const char* rav1g_decoder_error(const Rav1gDecoder* decoder) { return decoder ? decoder->message.c_str() : ""; }

const char* rav1g_decoder_error_detail(const Rav1gDecoder* decoder) { return decoder ? decoder->detail.c_str() : ""; }

void rav1g_decoder_drain(Rav1gDecoder* decoder) {
  if (decoder) aom_codec_decode(&decoder->ctx, NULL, 0, NULL);
}

Rav1gStatus rav1g_decoder_set_output_all_layers(Rav1gDecoder* decoder, bool enable) {
  if (!decoder) return RAV1G_STATUS_INVALID_PARAM;
  return keep(decoder, aom_codec_control(&decoder->ctx, AV1D_SET_OUTPUT_ALL_LAYERS, enable ? 1 : 0));
}

Rav1gStatus rav1g_decoder_set_operating_point(Rav1gDecoder* decoder, uint32_t operating_point) {
  if (!decoder) return RAV1G_STATUS_INVALID_PARAM;
  return keep(decoder, aom_codec_control(&decoder->ctx, AV1D_SET_OPERATING_POINT, (int)operating_point));
}

void rav1g_decoder_destroy(Rav1gDecoder* decoder) {
  if (!decoder) return;
  aom_codec_destroy(&decoder->ctx);
  delete decoder;
}

void rav1g_set_cdef_v2(bool enable) { av1_set_cdef_v2(enable ? 1 : 0); }
void rav1g_set_intra_v2(bool enable) { av1_set_intra_v2(enable ? 1 : 0); }
void rav1g_set_intra_opt(bool enable) { av1_set_intra_opt(enable ? 1 : 0); }
void rav1g_set_async_clear(bool enable) { av1_set_async_clear(enable ? 1 : 0); }
void rav1g_set_inter_fast(uint32_t mode) { av1_set_inter_fast((int)mode); }
void rav1g_set_lf_h_split(uint32_t mode) { av1_set_lf_h_split((int)mode); }
void rav1g_set_intra_edge_fix(bool enable) { av1_set_intra_edge_fix(enable ? 1 : 0); }
void rav1g_set_poison_buffers(uint32_t level) { av1_set_poison_buffers((int)level); }
void rav1g_set_perf_detail(uint32_t mode) { av1_set_perf_detail((int)mode); }
const char* rav1g_shader_variant(void) { return av1_shader_variant(); }

void rav1g_set_perf_callback(Rav1gPerfCallback callback, void* context) { av1_set_perf_callback(callback, context); }

void rav1g_set_stage_dump(Rav1gStageDumpCallback callback, void* context, const int32_t* frames, size_t count) {
  av1_set_stage_dump(callback, context, frames, (int)count);
}

// The library's log callback takes an int where rav1g.h has a bool.
static Rav1gLogCallback log_callback = NULL;
static void* log_context = NULL;

static void log_line(void* priv, int is_error, const char* line) {
  (void)priv;
  if (log_callback) log_callback(log_context, is_error != 0, line);
}

void rav1g_set_log_callback(Rav1gLogCallback callback, void* context) {
  if (callback) {
    log_context = context;
    log_callback = callback;
    av1_set_log_callback(log_line, NULL);
  } else {
    av1_set_log_callback(NULL, NULL);
    log_callback = NULL;
    log_context = NULL;
  }
}

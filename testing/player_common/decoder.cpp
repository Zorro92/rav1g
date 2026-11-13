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

#include "decoder.h"
#include "decoder_creator.h"
#include <direct.h>

#define PLANE_WIDTH_ALIGMENT 256
#define PLANE_WIDTH_ALIGMENT1 (PLANE_WIDTH_ALIGMENT - 1)

using sys_clock = std::chrono::system_clock;
using time_point = sys_clock::time_point;
// using duration_uint64 = std::chrono::duration<uint64_t, std::micro>;

static const int direct_mem_align = 16 * 1024;

static std::string MibString(uint64_t bytes) {
  char ss[48];
  sprintf_s(ss, sizeof(ss), "%.1f MiB", (double)bytes / (1024.0 * 1024.0));
  return ss;
}

// "MEMORY app" line: the memory of the whole app and the limit the system gives it. In app mode on the
// console that limit is what a decoder creation runs into; the decoder's buffers count against it.
static void LogAppMemory(const char* label, bool is_error) {
#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
  std::string line = std::string("MEMORY app ") + label + ": ";
  try {
    const uint64_t usage = Windows::System::MemoryManager::AppMemoryUsage;
    const uint64_t limit = Windows::System::MemoryManager::AppMemoryUsageLimit;
    line += "usage " + MibString(usage) + " of limit " + MibString(limit) + ", " +
            MibString(limit > usage ? limit - usage : 0) + " left";
  } catch (Platform::Exception ^) {
    line += "not available (Windows::System::MemoryManager failed)";
  }
  if (is_error)
    XB_LOGE << line;
  else
    XB_LOGI << line;
#else
  (void)label;
  (void)is_error;
#endif
}

// What the app has left of its memory limit (Windows::System::MemoryManager), negative where that is not
// known. On a console the D3D12 buffers of the decoder count against that limit, and an app that goes over
// it loses its D3D12 device for the rest of the process.
static int64_t AppMemoryLeft() {
#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
  try {
    const uint64_t usage = Windows::System::MemoryManager::AppMemoryUsage;
    const uint64_t limit = Windows::System::MemoryManager::AppMemoryUsageLimit;
    return limit > usage ? (int64_t)(limit - usage) : 0;
  } catch (Platform::Exception ^) {
    return -1;
  }
#else
  return -1;
#endif
}

// The app's memory in use, 0 where that is not known.
static uint64_t AppMemoryUsageNow() {
#if (WINAPI_FAMILY == WINAPI_FAMILY_APP)
  try {
    return Windows::System::MemoryManager::AppMemoryUsage;
  } catch (Platform::Exception ^) {
    return 0;
  }
#else
  return 0;
#endif
}

// One step of the launch or of a task: "MEMORY app <where>: ..." and "MEMORY dxgi <where>: ...". The steps
// are listed in docs/test-player.md; the difference between two lines is what the step in between took or
// gave back.
void AV1Decoder::LogMemory(ID3D12Device* device, const char* where) {
  LogAppMemory(where, false);
  if (device) av1_log_video_memory_now(device, where);
}

void AV1Decoder::DestroyDecoder() {
  XB_LOGI << "Destroy decoder";
  LogMemory("before the decoder is destroyed");
  // The decoder logs its own release step by step ("MEMORY destroy ...").
  aom_codec_destroy(&decctx_);
  memset(&decctx_, 0, sizeof(decctx_));
  initialized_ = 0;
  settle_pending_ = true;
  LogMemory("after the decoder was destroyed");
}

// The system gives the memory of released D3D12 buffers back to the app some time after the release
// (D3D12 only queues the destruction), and the budget guard of the next creation looks at what the app has
// left. So after a decoder and its output pool were released the player waits here until the app's memory
// is back near what it was without a decoder, at most kSettleMaxMs, and logs what it saw: how long it took,
// or how much was still charged at the end of the wait.
void AV1Decoder::SettleAppMemory() {
  static const int kSettlePollMs = 20;
  static const int kSettleMaxMs = 2000;
  static const uint64_t kSettleNear = (uint64_t)16 << 20;
  const uint64_t start = AppMemoryUsageNow();
  if (!start || !app_baseline_) {
    XB_LOGI << "MEMORY app settle after the release: not done, the app's memory is not known here";
    return;
  }
  uint64_t now = start;
  int waited_ms = 0;
  while (now > app_baseline_ + kSettleNear && waited_ms < kSettleMaxMs) {
    std::this_thread::sleep_for(std::chrono::milliseconds(kSettlePollMs));
    waited_ms += kSettlePollMs;
    now = AppMemoryUsageNow();
  }
  const uint64_t above = now > app_baseline_ ? now - app_baseline_ : 0;
  reusable_after_release_ = above < largest_device_size_ ? above : largest_device_size_;
  XB_LOGI << "MEMORY app settle after the release: usage " << MibString(start) << " -> " << MibString(now) << " in "
          << waited_ms << " ms; without a decoder the app used " << MibString(app_baseline_)
          << " at the first task of the launch, " << MibString(above) << " more are charged now"
          << (above > kSettleNear ? " (not returned within the wait)" : " (returned)")
          << "; counted as available to the next decoder: " << MibString(reusable_after_release_);
  LogMemory("after the settle wait");
}

// The textures the output pool creates when the first frame arrives (all its images at once): the samples of
// a 4:2:0 image and a sixteenth for alignment. An estimate: 4 images of 3840x2160 10-bit come to 100.9 MiB,
// the console reported 100.5 MiB. The readback buffers of an md5 or file run are not counted: with them the
// runs that were measured added up to more than the app's limit and still ran, so counting them would refuse
// runs that work.
static uint64_t OutputPoolEstimate(const CParameters::Parameters& params, unsigned int width, unsigned int height,
                                   unsigned int bit_depth) {
  if (params.needReadBack_ && params.noninterop_) return 0;  // readback buffers only
  const uint64_t samples = (uint64_t)width * height + 2 * (uint64_t)((width + 1) / 2) * ((height + 1) / 2);
  // 16 bits a sample, or three samples in 32 bits with -try10x3.
  const uint64_t image = bit_depth > 8 ? (params.hdr10x3_ ? samples * 4 / 3 : samples * 2) : samples;
  return (uint64_t)params.player_queue_size_ * image * 17 / 16;
}

// The decoder's budget guard (aom_codec_dec_cfg_t::memory_available_cb): what the decoder may still take is
// what the app has left, less the part of the output pool that does not exist yet, plus what an earlier
// decoder left charged to the app and the next one gets back. Measured on the console (2026-11-12): a 4K
// 10-bit decoder peaks at 912-916 MiB of app memory whether it starts from 148, 223 or 288 MiB, because the
// memory a destroyed decoder leaves charged (up to ~250 MiB) is reused; without this the guard refused
// creations that fit.
int64_t AV1Decoder::MemoryAvailable(void* priv) {
  AV1Decoder* This = static_cast<AV1Decoder*>(priv);
  int64_t left = AppMemoryLeft();
  if (left < 0 || !This) return left;
  left += (int64_t)This->reusable_after_release_;
  const uint64_t created = This->storage_ ? This->storage_->TextureBytes() : 0;
  const uint64_t pending = This->output_pool_estimate_ > created ? This->output_pool_estimate_ - created : 0;
  return (uint64_t)left > pending ? (int64_t)((uint64_t)left - pending) : 0;
}

char* pdummy = 0;
int AV1Decoder::DecodeLoop() {
  char dummy;
  pdummy = &dummy;
  is_running_ = 1;
  for (int i = 0; i < params_.loops_; i++) {
    XB_LOGI << " ";
    XB_LOGI << "start loop " << i << " for " << params_.source_file_;
    if (!app_baseline_taken_ && !initialized_ && host_mem_.empty()) {
      // First task of the launch: the pipeline states exist, nothing of a decoder or a stream does.
      app_baseline_ = AppMemoryUsageNow();
      app_baseline_taken_ = true;
    }
    LogMemory("task start");
    DecodeOne();
    // The stream reader of the task and its buffers are locals of DecodeOne: gone here.
    LogMemory("task end, stream reader closed");
  }
  XB_LOGI << " ";
  XB_LOGI << "All works're done. Bye";
  is_running_ = 0;
  return 0;
}

int BufferLocker::GetBufferInt(size_t buf_size, uint16_t w, uint16_t h, frame_buffer_type fb_type,
                               av1_decoded_frame_buffer_t* fb) {
  uint32_t flags = params_->needReadBack_ ? (params_->noninterop_ ? CBUFREADBACK : CBUFALL)
                                          : CBUFTEXTURE;
  std::shared_ptr<OutBufferWrapper> buffer = buffers_pool_->GetFreeBuffer(w, h, fb_type, buf_size, flags);
  if (!buffer) return -1;
  OutBuffer* buf = buffer->Buffer();
  fb->dx12_buffer = buf->d3d12buffer.Get();
  fb->buffer_host_ptr = buf->pMem;
  fb->dx12_texture[0] = buf->d3d12textures[0].Get();
  fb->dx12_texture[1] = buf->d3d12textures[1].Get();
  fb->dx12_texture[2] = buf->d3d12textures[2].Get();
  fb->priv = buf;
  return 0;
}
int BufferLocker::ReleaseBufferInt(void* buffer_priv) {
  buffers_pool_->ReleaseRentedBuffer(buffer_priv);
  return 0;
}

int AV1Decoder::DecodeOne() {
  // A completed -r task released all its pools below. Measure its settle before the
  // next reader starts allocating compressed read-ahead buffers.
  if (settle_pending_ && !initialized_ && !storage_ && host_mem_.empty()) {
    SettleAppMemory();
    settle_pending_ = false;
  }
  // int res;
  int dec_flags = 0;
  StreamReader reader(srmodeAsync);
  if (params_.source_file_.empty() || params_.source_file_.length() < 4) {
    XB_LOGE << "Invalid parameters. File path is empty";
    return -1;
  }
  std::string source_path(params_.source_file_);
  if (reader.Start(source_path)) {
    XB_LOGE << "Unable to start file reader for " << source_path;
    return -1;
  }
  ReadBufferWrapper buffer;

  // skip a few first frames if required
  for (int i = 0; i < params_.start_frame_; i++) {
    buffer = reader.GetData();
    if (!buffer) {
      XB_LOGE << "end of stream reached before start decoding";
      return 0;
    }
  }
  int cur_frame = params_.start_frame_;

  // skip non-key frames (if start_frame_ != 0)
  int is_key = 0;
  aom_codec_stream_info_t si;
  memset(&si, 0, sizeof(si));
  // Reads on to the first key frame; its sequence header has the largest frame size of the stream.
  auto find_key_frame = [&]() -> int {
    buffer.Reset();
    while (1) {
      buffer = reader.GetData();
      if (!buffer) {
        XB_LOGE << "End of Stream reached looking for key frame ";
        return -1;
      }
      aom_codec_err_t err = aom_codec_peek_stream_info(aom_codec_av1_dx(), buffer->Data(), buffer->Size(), &si);
      if (AOM_CODEC_OK != err) {
        XB_LOGE << "Failed to get stream info";
        return -1;
      }
      is_key = si.is_kf;
      if (is_key) break;
      cur_frame++;
    }
    return 0;
  };

  // The key frame is looked for before the decoder exists: its sequence header says how large the frames
  // and the output images of the stream are (max_frame_width and max_frame_height bound every frame of the
  // sequence, with superres and reference scaling) and how many bits a sample has.
  if (find_key_frame()) return -1;
  // The reader thread is open and reads ahead (up to 20 frames); the first key frame is in memory.
  LogMemory("stream reader at the first key frame");

  // The size the decoder is created for: the stream's largest frame size, rounded up to 128 as the decoder
  // does, and 8-bit buffers for an 8-bit stream. A stream that is larger than 4096x2176 in either direction
  // gets that size, and so does every stream with -nofitsize.
  unsigned int dec_width = PLAYER_DECODER_WIDTH;
  unsigned int dec_height = PLAYER_DECODER_HEIGHT;
  unsigned int dec_bitdepth = PLAYER_DECODER_BITDEPTH;
  auto size_for_stream = [&](const aom_codec_stream_info_t& info, unsigned int* width, unsigned int* height,
                             unsigned int* bitdepth) {
    *width = PLAYER_DECODER_WIDTH;
    *height = PLAYER_DECODER_HEIGHT;
    *bitdepth = PLAYER_DECODER_BITDEPTH;
    if (params_.fitsize_) {
      const unsigned int fit_width = (info.w + 127) & ~127u;
      const unsigned int fit_height = (info.h + 127) & ~127u;
      if (info.w && info.h && fit_width <= PLAYER_DECODER_WIDTH && fit_height <= PLAYER_DECODER_HEIGHT) {
        *width = fit_width;
        *height = fit_height;
      }
      if (info.bit_depth == 8) *bitdepth = 8;
    }
  };
  size_for_stream(si, &dec_width, &dec_height, &dec_bitdepth);

  // A decoder that was created with something else than this task asks for is replaced. need_reinit_ says
  // so for the task before; the comparison also covers a task before that ended ahead of this point.
  const bool other_decoder =
      initialized_ && (created_threads_ != params_.threads_ || created_queue_size_ != params_.player_queue_size_ ||
                       created_slots_ != params_.slots_ || created_width_ != dec_width ||
                       created_height_ != dec_height || created_bitdepth_ != dec_bitdepth ||
                       created_intrav2_ != params_.intrav2_ ||
                       created_interfast_ != (params_.interfast_ || params_.interfastlists_) ||
                       created_lowmem_ != params_.lowmem_parts_);
  if (initialized_ && (need_reinit_ || other_decoder)) DestroyDecoder();
  // Creates the decoder for dec_width x dec_height, dec_bitdepth and the stream info in si. |new_sequence|:
  // the decoder replaces the one of this task in the middle of the stream (a new sequence that did not fit
  // it, in the decode loop below); the output queue and the output pool of the task stay, with the frames
  // of the sequence before still in them. Returns 0 when the decoder exists.
  auto create_decoder = [&](bool new_sequence) -> int {
    XB_LOGI << "(Re)create decoder";
    try {
      // No decoder exists here. The output pool of the one before goes first: its images are memory the new
      // decoder may need, and the old output queue would keep them until after the creation.
      if (!new_sequence && (output_queue_ || storage_)) {
        LogMemory("before the output pool is released");
        output_queue_.reset();
        storage_.reset();
        LogMemory("after the output pool was released");
      }
      if (!host_mem_.empty()) {
        host_mem_.clear();
        host_mem_.shrink_to_fit();
        LogMemory("after the host pool was released");
      }
      // Everything of the decoder before is released. Give the system the time to hand it back before the
      // host pool of the new decoder is allocated and its budget guard looks at what is left.
      // (Not in the middle of a stream: the output pool stays, so the app's memory does not come back to
      // what it was without a decoder. The budget guard of the creation waits for what it needs.)
      if (settle_pending_) {
        if (!new_sequence) SettleAppMemory();
        settle_pending_ = false;
      }
      aom_codec_dec_cfg_t cfg;
      memset(&cfg, 0, sizeof(aom_codec_dec_cfg_t));
      cfg.width = dec_width;
      cfg.height = dec_height;
      cfg.bitdepth = dec_bitdepth;
      cfg.threads = params_.threads_;
      cfg.cfg.ext_partition = 1;  //?
      cfg.allow_lowbitdepth = CONFIG_LOWBITDEPTH;
      cfg.out_buffers_cb.get_out_buffer_cb = GetBuffer;
      cfg.out_buffers_cb.release_out_buffer_cb = ReleaseBuffer;
#if CB_OUTPUT
      cfg.out_buffers_cb.notify_frame_ready_cb = NotifyFrameReady;
#endif

      cfg.out_buffers_cb.out_buffers_priv = this;
      cfg.host_size = 0;
      cfg.tryHDR10x3 = params_.hdr10x3_;
      // -slots: set before the memory query, each slot has host memory of its own.
      cfg.frame_slots = params_.slots_;
      // The same for what else decides which buffers exist: -intrav2 needs buffers of its own, -lowmem and
      // -lowmemparts leave some out or make them smaller. The player always says which parts it wants: 0 in
      // the configuration would be the library's default (the shared back buffer), not "none".
      cfg.intra_v2 = params_.intrav2_ ? 1 : 0;
      // -interfast: room in the block maps for the lists by motion vector class.
      cfg.inter_fast = (params_.interfast_ || params_.interfastlists_) ? 1 : 0;
      cfg.low_memory = params_.lowmem_parts_ ? params_.lowmem_parts_ : AV1_LOW_MEMORY_NONE;
      // Budget guard: the decoder refuses a creation, and later a palette buffer, that does not fit what
      // the app has left. The output pool takes its textures on the first frame; until then they are held
      // back from what the decoder may take (MemoryAvailable).
      const unsigned int out_width = si.w ? si.w : dec_width;
      const unsigned int out_height = si.h ? si.h : dec_height;
      const unsigned int out_bitdepth = si.bit_depth ? si.bit_depth : dec_bitdepth;
      const uint64_t output_pool = OutputPoolEstimate(params_, out_width, out_height, out_bitdepth);
      // A pool that stays (new_sequence) gets its images of the new size as the frames arrive, in place of
      // the ones it has: the whole estimate is still to come, on top of what the pool has created so far.
      output_pool_estimate_ = output_pool + ((new_sequence && storage_) ? storage_->TextureBytes() : 0);
      // -noguard: no callback, the creation is not checked. For finding out whether memory that stays
      // charged after a decoder was destroyed is reused by the next one; a creation that does not fit loses
      // the D3D12 device for the rest of the launch.
      cfg.memory_available_cb = params_.noguard_ ? NULL : MemoryAvailable;
      cfg.memory_available_priv = this;
      av1_codec_query_memory_requirements(&cfg);
      host_mem_.resize(cfg.host_size, 0);
      cfg.host_memory = &host_mem_[0];  // onion_mem.Data();
      cfg.host_size = host_mem_.size();
      cfg.dx12device = dx_desources_.dx12_device.Get();
      cfg.dx12command_queue = dx_desources_.dx12_command_queue.Get();
      cfg.dxPsos = dx_desources_.dx12_psos;
      if (!cfg.dxPsos) {
        XB_LOGE << "Unable to create compute shaders";
        return 1;
      }

      XB_LOGI << "MEMORY decoder size: " << dec_width << "x" << dec_height << " " << dec_bitdepth << "-bit ("
              << (params_.fitsize_ ? "fit to the stream" : "-nofitsize") << ", the stream's largest frame is "
              << si.w << "x" << si.h << " " << si.bit_depth << "-bit), slots " << params_.slots_
              << ", intra_v2 buffers "
              << (params_.intrav2_ ? "yes (-intrav2)" : "no") << ", low memory parts " << params_.lowmem_parts_
              << "; device buffers " << MibString(cfg.device_size) << " and up to "
              << MibString(cfg.device_size_deferred) << " for palette buffers on first use, host pool "
              << MibString(cfg.host_size);
      XB_LOGI << "MEMORY output pool estimate: textures of " << params_.player_queue_size_ << " images of "
              << out_width << "x" << out_height << " " << out_bitdepth << "-bit, " << MibString(output_pool)
              << (params_.needReadBack_ ? " (the readback buffers of this run are not counted)" : "")
              << "; held back from what the decoder may take until the pool has them. The decoder's budget "
              << "guard keeps a margin of " << MibString(AV1_MEMORY_GUARD_MARGIN) << " at its creation"
              << (params_.noguard_ ? "; -noguard: the guard is off for this creation" : "");
      // The host pool of the new decoder is allocated; the decoder's own lines give the video memory.
      LogAppMemory("before the decoder", false);
      const int init_failed = aom_codec_dec_init(&decctx_, aom_codec_av1_dx(), &cfg, dec_flags);
      // The memory an earlier decoder left is this decoder's now. Only when it exists: a creation that
      // failed or was refused has allocated nothing, the memory is still there for the next creation, and
      // the budget guard of a smaller decoder has to count it (without it a refused creation made the
      // guard refuse the next one as well, which would have fit).
      if (!init_failed) reusable_after_release_ = 0;
      if (!init_failed && cfg.device_size > largest_device_size_) largest_device_size_ = cfg.device_size;
      if (init_failed) {
        host_mem_.clear();
        host_mem_.shrink_to_fit();
        // The detail is the decoder's MEMORY FAILED line for what could not be allocated. The decoder has
        // released everything it had allocated up to there.
        const char* detail = aom_codec_error_detail(&decctx_);
        XB_LOGE << "Failed to initialize decoder: " << aom_codec_error(&decctx_)
                << ((detail && detail[0]) ? ": " : "") << ((detail && detail[0]) ? detail : "");
        LogAppMemory("after the failed decoder creation", true);
        throw 2;
      }
      LogAppMemory("after the decoder", false);
      if (!new_sequence)
        storage_ = std::unique_ptr<OutBufferStorage>(
            new OutBufferStorage(params_.player_queue_size_, dx_desources_.dx12_device.Get()));
      initialized_ = 1;
      created_threads_ = params_.threads_;
      created_queue_size_ = params_.player_queue_size_;
      created_slots_ = params_.slots_;
      created_width_ = dec_width;
      created_height_ = dec_height;
      created_bitdepth_ = dec_bitdepth;
      created_intrav2_ = params_.intrav2_;
      created_interfast_ = params_.interfast_ || params_.interfastlists_;
      created_lowmem_ = params_.lowmem_parts_;
    } catch (int error) {
      XB_LOGE << "Unable to create decoder. Error " << error;
      return -1;
    } catch (std::bad_alloc error) {
      XB_LOGE << "Unable to create decoder. Error " << error.what();
      return -1;
    } catch (...) {
      XB_LOGE << "Unable to create decoder. Unknown error";
      return -1;
    }
    return 0;
  };
  if (params_.recreate_ || !initialized_) {
    const int create_error = create_decoder(false);
    if (create_error) return create_error;
  }
  // The layers of a stream that has them: one frame per temporal unit unless -alllayers, the first operating
  // point unless -oppoint. Set for every task (a decoder serves several) and for a decoder that replaces
  // the task's in the middle of the stream.
  auto apply_layer_options = [&]() {
    if (aom_codec_control(&decctx_, AV1D_SET_OUTPUT_ALL_LAYERS, params_.all_layers_ ? 1 : 0))
      XB_LOGE << "AV1D_SET_OUTPUT_ALL_LAYERS failed: " << aom_codec_error(&decctx_);
    if (aom_codec_control(&decctx_, AV1D_SET_OPERATING_POINT, params_.oppoint_))
      XB_LOGE << "AV1D_SET_OPERATING_POINT failed: " << aom_codec_error(&decctx_);
  };
  apply_layer_options();
  if (!buffer) {
    buffer = reader.GetData();
    if (!buffer) {
      XB_LOGE << "End of stream ";
      return -1;
    }
  }
  double framedur = 16667.0;
  double orig_frate = reader.FrameRate();
  if (params_.frate_ > 0)
    framedur = (double)1000000.0 / (double)params_.frate_;
  else if (params_.frate_ == 0)
    framedur = 1000000.0 / orig_frate;
  if (params_.frate_ < 0)
    XB_LOGI << "Frame rate = unlimited (-1)"
            << " (" << params_.threads_ << " threads, player_queue_size " << params_.player_queue_size_ << ")";
  else
    XB_LOGI << "Frame rate = " << (1000000.0 / framedur) << " (" << params_.threads_ << " threads, player_queue_size "
            << params_.player_queue_size_ << ")";
  need_preload_ = params_.frate_ >= 0;
  preload_num_ = params_.player_queue_size_ < PRELOAD_SIZE ? params_.player_queue_size_ : PRELOAD_SIZE;

  // Timing run (-gputime): the previous task has drained, so no frame is in flight here.
  {
    std::lock_guard<std::mutex> lock(perf_cs_);
    perf_stats_.Reset();
  }
  av1_set_perf_callback(params_.gputime_ ? PerfFrame : NULL, this);
  perf_installed_ = params_.gputime_ ? 1 : 0;
  // -gputimedetail, -gputimeisolate: the inter breakdown, set like the callback while no frame is in flight.
  av1_set_perf_detail(params_.gputime_ ? params_.gputime_detail_ : 0);
  storage_->SetWaitLogging(params_.gputime_);
  storage_->ResetWaitStats();
  if (params_.gputime_)
    XB_LOGI << "GPUTIME timing run: per-stage GPU timestamps and CPU timers on, summary follows the last frame";
  if (params_.gputime_detail_)
    XB_LOGI << "GPUTIME inter breakdown: "
            << (params_.gputime_detail_ == 2
                    ? "a UAV barrier and a timestamp behind every dispatch of the inter stage and of the "
                      "reconstruction (-gputimeisolate; same output, the stage is slower by what they cost)"
                    : "a timestamp behind every section of the inter stage, work counters (-gputimedetail)");
  if (params_.nopresent_)
    XB_LOGI << "GPUTIME -nopresent: decoded frames go straight back to the pool, nothing is presented";
  // CDEF shader: also set at a point where no frame is in flight. cdef_filter_v2 unless the task has -cdefv1.
  av1_set_cdef_v2(params_.cdefv2_ ? 1 : 0);
  XB_LOGI << "CDEF shader: " << (params_.cdefv2_ ? "cdef_filter_v2 (default)" : "cdef_filter (original, -cdefv1)");
  // -intrav2: the same rules, independent of the CDEF shader.
  av1_set_intra_v2(params_.intrav2_ ? 1 : 0);
  XB_LOGI << "Intra shaders: "
          << (params_.intrav2_ ? "intra_v2 (-intrav2; frames with intra block copy keep the original shaders)"
                               : "intra_main + intra_filter (original)");
  // -interfast: the same rules. The decoder was created with room for the lists (cfg.inter_fast above).
  // -interfastlists: the same lists, the original shaders on every one of them (what the lists alone cost).
  av1_set_inter_fast(params_.interfastlists_ ? AV1_INTER_FAST_LISTS_ONLY
                                              : params_.interfast_ ? AV1_INTER_FAST_ON : 0);
  XB_LOGI << "Inter shaders: "
          << (params_.interfastlists_
                  ? "lists by motion vector class, inter_main and inter_compound on them (-interfastlists; "
                    "frames with a scaled reference keep the original lists)"
                  : params_.interfast_ ? "by motion vector class: inter_fast, inter_compound_fast (-interfast; "
                                         "frames with a scaled reference keep the original shaders)"
                                       : "inter_main, inter_compound (original)");
  // -intraopt: which build of the original intra_main shader runs; independent of the two flags above.
  av1_set_intra_opt(params_.intraopt_ ? 1 : 0);
  XB_LOGI << "Intra main shader: "
          << (params_.intraopt_ ? "intra_main_opt (FXC optimised, -intraopt)" : "intra_main (/Od, original)");

  // -slots: fixed when the decoder was created; Start() asks for a new decoder when the value changes.
  XB_LOGI << "Frame slots: " << params_.slots_;
  // The decoder's size: also fixed at creation; a decoder of another size is replaced.
  XB_LOGI << "Decoder size: " << created_width_ << "x" << created_height_ << " " << created_bitdepth_ << "-bit"
          << (params_.fitsize_ ? " (fit to the stream, default)" : " (fixed, -nofitsize)");
  // -lowmem, -lowmemparts: fixed at creation too.
  XB_LOGI << "Low memory parts: " << created_lowmem_
          << (created_lowmem_ == PLAYER_LOWMEM_DEFAULT
                  ? " (default: 1 shared back buffer)"
                  : created_lowmem_ ? " (1 shared back buffer, 2 mode info shares, 4 coefficient shares)"
                                    : " (none, -lowmemparts 0)");
  // -asyncclear: the same rules as the shader flags above, independent of them and of -slots.
  av1_set_async_clear(params_.asyncclear_ ? 1 : 0);
  XB_LOGI << "Clears: " << (params_.asyncclear_ ? "off the decode thread (-asyncclear)" : "decode thread (original)");

  // The experiments: set like the switches above. The shader build is fixed when the package is built.
  av1_set_lf_h_split(params_.lfhsplit_);
  av1_set_intra_edge_fix(params_.intraedgefix_ ? 1 : 0);
  XB_LOGI << "Experiments: shaders " << (av1_shader_variant()[0] ? av1_shader_variant() : "normal build")
          << "; horizontal loop filter "
          << (params_.lfhsplit_ == 1 ? "even rows of blocks, then odd (-lfhsplit 1)"
                                     : params_.lfhsplit_ == 2 ? "row of blocks by row (-lfhsplit 2)"
                                                              : "one dispatch per plane (default)")
          << "; 8-bit above edge filter bound " << (params_.intraedgefix_ ? "filter_count (-intraedgefix)" : "above_count (default)");
  // -dumpstages: like the switches above, set while no frame is in flight.
  if (!params_.dump_frames_.empty()) {
    _mkdir(params_.dumpdir_.c_str());
    const size_t name_pos = params_.source_file_.find_last_of("\\/");
    stage_dumps_.Start(params_.dumpdir_,
                       name_pos == std::string::npos ? params_.source_file_ : params_.source_file_.substr(name_pos + 1));
    av1_set_stage_dump(StageDumpWriter::Callback, &stage_dumps_, params_.dump_frames_.data(),
                       (int)params_.dump_frames_.size());
    dumps_installed_ = 1;
    XB_LOGI << "DUMPSTAGES frames " << params_.dumpstages_ << " (decoding order) to " << params_.dumpdir_;
  } else {
    av1_set_stage_dump(NULL, NULL, NULL, 0);
    dumps_installed_ = 0;
  }

  output_queue_ = std::unique_ptr<BufferLocker>(new BufferLocker(&params_, cur_frame, storage_));
  if (display_) display_->AddSource(this);

#if (ASYNC_OUTPUT)
  StartFetch();
#elif !CB_OUTPUT
  int frame_out = 0;
  aom_image_t* img;
#endif  // ASYNC_OUTPUT

  int frame_in = 0;
  int cnt = 0;
  int limit = params_.limit_;
  int has_error = 0;
  int at_least_one = 0;
  // MEMORY lines of the decode loop: once the first frame is out (the output pool has its images then) and
  // once 30 are out, to tell memory that is taken once from memory that grows with the frames.
  static const int kMemoryMarks[] = {1, 30};
  int memory_mark = 0;
  while (buffer && limit-- && !(*stop_) && !output_queue_->hasFatalError_) {
    XB_LOGD << cnt++ << " - " << buffer->Size();
    uint64_t user_priv = static_cast<uint64_t>(frame_in++ * framedur);
    const std::chrono::steady_clock::time_point perf_call_start = std::chrono::steady_clock::now();
    aom_codec_err_t decode_err = aom_codec_decode(&decctx_, buffer->Data(), buffer->Size(), (void*)user_priv);
    if (decode_err == AOM_CODEC_NEEDS_RECONFIGURE) {
      // A new sequence whose frames are larger, or have more bits, than the decoder was created for
      // (aom/aom_codec.h). Nothing of this temporal unit was decoded. The frames of the sequence before
      // are drained into the output queue, which stays, the decoder is replaced by one for the new
      // sequence, sized from this temporal unit as the first one was from the first key frame and never
      // smaller than the one before, and the temporal unit is passed again.
      const std::string why = aom_codec_error_detail(&decctx_) ? aom_codec_error_detail(&decctx_) : "";
      aom_codec_stream_info_t new_si;
      memset(&new_si, 0, sizeof(new_si));
      unsigned int new_width = 0, new_height = 0, new_bitdepth = 0;
      const bool new_key = aom_codec_peek_stream_info(aom_codec_av1_dx(), buffer->Data(), buffer->Size(), &new_si) ==
                               AOM_CODEC_OK && new_si.is_kf;
      if (new_key) {
        size_for_stream(new_si, &new_width, &new_height, &new_bitdepth);
        if (new_width < created_width_) new_width = created_width_;
        if (new_height < created_height_) new_height = created_height_;
        if (new_bitdepth < created_bitdepth_) new_bitdepth = created_bitdepth_;
      }
      if (!new_key || (new_width == created_width_ && new_height == created_height_ &&
                       new_bitdepth == created_bitdepth_)) {
        XB_LOGE << "aom_codec_decode: the temporal unit needs another decoder (" << why << ") "
                << (new_key ? "and the player creates none that is larger" : "and does not start with a key frame");
      } else {
        XB_LOGI << "RECONFIGURE: a new sequence of " << new_si.w << "x" << new_si.h << " " << new_si.bit_depth
                << "-bit frames (" << why << "); the decoder is replaced by one of " << new_width << "x"
                << new_height << " " << new_bitdepth << "-bit";
        // The drain joins the decoder's completion thread, which ends with the notice that no more frames
        // come; the output queue must not take that for the end of the stream.
        reconfiguring_ = 1;
        aom_codec_decode(&decctx_, 0, 0, 0);
        reconfiguring_ = 0;
        DestroyDecoder();
        // -dumpstages: the drain has handed over the dumps of the sequence before; the frame numbers of the
        // new decoder start at 0 again, and its dumps go to files of their own.
        if (dumps_installed_) stage_dumps_.NextSequence();
        si = new_si;
        dec_width = new_width;
        dec_height = new_height;
        dec_bitdepth = new_bitdepth;
        if (create_decoder(true)) {
          // No decoder is left whose drain would tell the output queue that the stream has ended.
          has_error = 1;
          output_queue_->hasFatalError_ = 1;
          output_queue_->Push(0);
          break;
        }
        apply_layer_options();
        decode_err = aom_codec_decode(&decctx_, buffer->Data(), buffer->Size(), (void*)user_priv);
      }
    }
    if (decode_err) {
      XB_LOGE << "aom_codec_decode error " << aom_codec_error(&decctx_);
      has_error = 1;
      output_queue_->hasFatalError_ = 1;
      break;
    }
    if (params_.gputime_) {
      const double call_ms =
          std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - perf_call_start).count();
      std::lock_guard<std::mutex> lock(perf_cs_);
      perf_stats_.AddDecodeCall(call_ms);
    }
    at_least_one = 1;
#if (!ASYNC_OUTPUT && !CB_OUTPUT)
    aom_codec_iter_t iter = NULL;
    while ((img = aom_codec_get_frame(&decctx_, &iter))) {
      XB_LOGD << "got " << ++frame_out;
      if (output_queue_->Push(img)) {
        output_queue_->hasFatalError_ = 1;
        break;
      }
    }
#endif  // ASYNC_OUTPUT
    if (memory_mark < 2 && output_queue_->pushed_.load() >= kMemoryMarks[memory_mark]) {
      LogMemory(memory_mark == 0 ? "decoding, first frame out" : "decoding, 30 frames out");
      ++memory_mark;
    }
    buffer = reader.GetData();
  }
  LogMemory("before the drain");
  // drain
  aom_codec_decode(&decctx_, 0, 0, 0);
  LogMemory("after the drain");
  // The drain joins the decoder's GPU completion thread: every dump of the task is written.
  if (dumps_installed_) {
    av1_set_stage_dump(NULL, NULL, NULL, 0);
    dumps_installed_ = 0;
    const std::vector<std::string> lines = stage_dumps_.TakeLines();
    for (size_t i = 0; i < lines.size(); ++i) XB_LOGI << lines[i];
    XB_LOGI << "DUMPSTAGES " << stage_dumps_.Dumps() << " dumps, " << MibString(stage_dumps_.Bytes()) << ", in "
            << params_.dumpdir_;
    if (!stage_dumps_.Error().empty()) XB_LOGE << "DUMPSTAGES " << stage_dumps_.Error();
  }
  // The drain joins the decoder's GPU completion thread, so every frame record is in.
  if (params_.gputime_ || params_.nopresent_) PerfReport();
#if (!ASYNC_OUTPUT && !CB_OUTPUT)
  if (!output_queue_->hasFatalError_) {
    aom_codec_iter_t iter = NULL;
    while ((img = aom_codec_get_frame(&decctx_, &iter))) {
      XB_LOGD << "got " << ++frame_out;
      output_queue_->Push(img);
    }
  }
  output_queue_->Push(0);
#elif ASYNC_OUTPUT
  StopFetch();
#endif  // ASYNC_OUTPUT
  need_preload_ = 0;
  if (*stop_) {
    if (display_) display_->RemoveSource(this);
    output_queue_->Flush();
    XB_LOGI << "Decoding process was terminated by user";
  } else {
    if (at_least_one) output_queue_->WaitLast();
    if (display_) display_->RemoveSource(this);
  }
  LogMemory("after the last frame left the output queue");
  if (initialized_ && (has_error || params_.recreate_)) DestroyDecoder();
  // The task's own records: the md5 reference list and the md5 file being saved (Finalize), and the
  // per-frame timing records, which the report above has printed.
  output_queue_->Finalize();
  {
    std::lock_guard<std::mutex> lock(perf_cs_);
    perf_stats_.Release();
  }
  LogMemory("after the task's md5 and timing records were released");
  uint32_t has_md5_errors = output_queue_->has_md5_errors_;
  if (!initialized_) {
    // DestroyDecoder has joined the completion thread; RemoveSource and Finalize have
    // released the displayed/last frame. A destroyed decoder no longer needs these.
    output_queue_.reset();
    storage_.reset();
    std::vector<uint8_t>().swap(host_mem_);
    LogMemory("after the destroyed task's host and output pools were released");
  }
  if (*stop_ == taskNext) *stop_ = TaskNone;
  if (params_.conformance_) {
    if (has_md5_errors) {
      if (!params_.bad_conform_file_.empty()) {
        rename(params_.source_file_.c_str(), params_.bad_conform_file_.c_str());
        rename((params_.source_file_ + ".md5").c_str(), (params_.bad_conform_file_ + ".md5").c_str());
      }
    } else {
      remove(params_.source_file_.c_str());
      remove((params_.source_file_ + ".md5").c_str());
    }
  }

  return 0;
}

void AV1Decoder::PerfReport() {
  if (params_.nopresent_ && output_queue_) {
    const int released = output_queue_->released_;
    const double span_ms = std::chrono::duration<double, std::milli>(output_queue_->released_last_ -
                                                                     output_queue_->released_first_)
                               .count();
    XB_LOGI << "GPUTIME -nopresent: " << released << " frames released without presenting, "
            << ((released > 1 && span_ms > 0.0) ? (double)(released - 1) * 1000.0 / span_ms : 0.0)
            << " fps between the first and the last";
  }
  if (!params_.gputime_) return;
  perfstats::OutputWaits waits = {0, 0.0, 0.0};
  if (storage_) storage_->GetWaitStats(&waits.count, &waits.total_ms, &waits.max_ms);
  std::vector<std::string> summary;
  std::vector<std::string> frames;
  {
    std::lock_guard<std::mutex> lock(perf_cs_);
    summary = perf_stats_.Summary(waits);
    // Per-frame lines are kept in memory during the run and written only now,
    // so the timing run adds no log I/O to the decode loop.
    if (params_.progress_) frames = perf_stats_.FrameLines();
  }
  for (size_t i = 0; i < summary.size(); ++i) XB_LOGI << "GPUTIME " << summary[i];
  for (size_t i = 0; i < frames.size(); ++i) XB_LOGI << "GPUTIME " << frames[i];
}

int AV1Decoder::FetchLoop() {
  aom_image_t* img;
  aom_codec_iter_t iter = NULL;
  while (!stop_fetch_) {
    while ((img = aom_codec_get_frame(&decctx_, &iter))) {
      output_queue_->Push(img);
    }
  }
  while ((img = aom_codec_get_frame(&decctx_, &iter))) {
    output_queue_->Push(img);
  }
  output_queue_->Push(0);
  return 0;
}

void AV1Decoder::FlushOutput() {
  if (output_queue_.get()) {
    output_queue_->Flush();
  }
}

std::shared_ptr<OutBufferWrapper> AV1Decoder::GetFrame(StreamStats* stats) {
  if (!output_queue_.get()) return 0;
  if (params_.nopresent_) {
    // Nothing to show. Sleep so the render loop does not spin a core next to the tile threads.
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
    return 0;
  }
  if (need_preload_) {
    if (output_queue_->Size() < preload_num_ && output_queue_->hasFreeBuffers())
      return 0;
    else
      need_preload_ = 0;
  }

  if (stats) {
    stats->frate = output_queue_->FRate();
    stats->dropped = output_queue_->Drops();
    stats->fname = params_.source_file_;
  }
  return output_queue_->Pop();
}

void BufferLocker::Flush() {
  while (!queue_.empty()) {
    Pop();
  }
}

int BufferLocker::Push(aom_image_t* img) {
  if (!img) {
    std::lock_guard<std::mutex> lock(cs_);
    if (params_->nopresent_) {
      // Nothing is popped in this mode, so the end of the stream is seen here.
      got_last_frame_ = 1;
      cv_.notify_one();
      return 0;
    }
    queue_.push(0);
    return 0;
  }
  if (got_last_frame_) got_last_frame_ = 0;
  ++pushed_;
  if (img && (img->d_w != width_ || img->d_h != height_ || img->bit_depth != bpp_)) {
    width_ = img->d_w;
    height_ = img->d_h;
    bpp_ = img->bit_depth;
    XB_LOGI << "Got first frame (" << (uint64_t)img->user_priv << ") " << width_ << " x " << height_ << " x " << bpp_;
  }
  if (!params_->target_file_.empty() || params_->md5_frame_check_ || params_->md5_ || md5_saver_) {
    if (!params_->target_file_.empty() && img && params_->save_wrong_yuv_.empty()) {
      SaveToFile(img, false);
    }
    if (md5_saver_) {
      int ret = md5_saver_->Put(img);
      if (ret) md5_saver_.reset(0);
    }
    if ((params_->md5_frame_check_ || params_->md5_) && img && md5_checker_) {
      int err = md5_checker_->Put(img);
      if (err && !params_->save_wrong_yuv_.empty()) {
        SaveToFile(img, true);
      }
    }
  }
  if (params_->rawDecode_ || params_->progress_) {
    uint64_t duration;
    uint64_t stop_decode_time = timer_.GetCurrent(5, &duration);
    if (frame_counter_) {
      double current_frate = (double)frame_counter_ * 1000000.0 / (double)stop_decode_time;
      XB_LOGI << "Current frate " << frame_counter_ << "  " << current_frate;
      XB_LOGI << "## " << frame_counter_ << " " << duration << "(" << stop_decode_time << ")";
    }
    frame_counter_++;
    if (params_->rawDecode_) return 0;
  }
  if (params_->nopresent_) {
    // Hand the buffer straight back to the pool: no display queue, no Present.
    buffers_pool_->ReleaseRentedBuffer(img->fb2_priv);
    released_last_ = std::chrono::steady_clock::now();
    if (!released_++) released_first_ = released_last_;
    frame_no_++;
    return 0;
  }
  std::shared_ptr<OutBufferWrapper> buffer = buffers_pool_->ReleaseRentedBuffer(img->fb2_priv);  //!!!!
  assert(buffer);
  OutBuffer* buf = buffer->Buffer();
  buf->frame_no_offset = start_frame_;
  buf->frameWidth = img->w;
  buf->frameHeight = img->h;
  buf->renderWidth = img->d_w;
  buf->renderHeight = img->d_h;
  buf->fb_type = (img->bit_depth == 10) ? (img->is_hdr10x3 ? fbt10x3 : fbt10bit) : fbt8bit;
  buf->frame_no = frame_no_++;
  buf->pts = (uint64_t)img->user_priv;
  memcpy(buffer->Buffer()->planes, img->planes, sizeof(img->planes));
  memcpy(buffer->Buffer()->strides, img->stride, sizeof(img->stride));
  std::lock_guard<std::mutex> lock(cs_);
  buffer->SetShown(0);
  queue_.push(buffer);
  return 0;
}

int BufferLocker::SaveToFile(aom_image_t* img, bool one_frame) {
  std::string target_file;
  if (one_frame) {
    size_t pos = params_->source_file_.rfind("/");
    if (pos != std::string::npos) {
      char ss[32];
      sprintf_s(ss, 32, "%d", (int)(frame_no_ + start_frame_));
      std::string fn = params_->source_file_.substr(pos);
      target_file = params_->save_wrong_yuv_ + "/" + fn + "_" + ss + ".yuv";
    }
  } else
    target_file = params_->target_file_;
  if (target_file.empty()) {
    XB_LOGE << "Target file path isn't defined";
    return -1;
  }
  if (!out_file_) {
    out_file_ = fopen(target_file.c_str(), "wb");
    if (!out_file_) {
      XB_LOGE << "Unable to open target file " << target_file;
      return -1;
    }
  }
  uint8_t unpack_buff[4096 * 4];
  uint8_t* ptr = img->planes[0];
  uint8_t* wr_ptr;
  uint32_t width_in_byte = img->d_w * (img->bit_depth == 8 ? 1 : 2);
  for (uint32_t i = 0; i < img->d_h; i++) {
    if (img->is_hdr10x3) {
      uint8_t* targ = unpack_buff;
      Md5Checker::Unpack10x3(ptr, targ, img->d_w);
      wr_ptr = targ;
    } else
      wr_ptr = ptr;
    size_t written = fwrite(wr_ptr, sizeof(uint8_t), width_in_byte, out_file_);
    if (written != width_in_byte) {
      XB_LOGE << "Error in target file " << target_file;
      return -1;
    }
    ptr += img->stride[0];
  }
  if (!img->monochrome) {
    width_in_byte /= 2;
    uint32_t uv_width = (img->d_w + 1) / 2;
    uint32_t uv_width_in_byte = (img->d_w + 1) / 2 * (img->bit_depth == 8 ? 1 : 2);
    uint32_t uv_height = (img->d_h + 1) / 2;
    for (int k = 1; k < 3; k++) {
      ptr = img->planes[k];
      for (uint32_t i = 0; i < uv_height; i++) {
        if (img->is_hdr10x3) {
          uint8_t* targ = unpack_buff;
          Md5Checker::Unpack10x3(ptr, targ, uv_width);
          wr_ptr = targ;
        } else
          wr_ptr = ptr;
        size_t written = fwrite(wr_ptr, sizeof(uint8_t), uv_width_in_byte, out_file_);
        if (written != uv_width_in_byte) {
          XB_LOGE << "Error in target file " << target_file;
          return -1;
        }
        ptr += img->stride[k];
      }
    }
  }
  if (one_frame && out_file_) {
    fclose(out_file_);
    out_file_ = 0;
    XB_LOGI << "Decoded frame No " << (frame_no_ + start_frame_) << " was saved to " << target_file;
  }
  return 0;
}

std::shared_ptr<OutBufferWrapper> BufferLocker::Pop() {
  std::lock_guard<std::mutex> lock(cs_);
  std::shared_ptr<OutBufferWrapper> ret(0);
  if (!started_ && queue_.empty()) return ret;
  started_ = 1;
  int64_t delay = 0;
  uint64_t duration;
  uint64_t current = timer_.GetCurrent(0, &duration);
  int finished = 0;
  int local_drops = 0;
  // clean dropped
  if (params_->frate_ >= 0) {
    while (!queue_.empty()) {
      ret = queue_.front();
      if (ret) {
        uint64_t pts = ret->Buffer()->pts;
        delay = current - pts;
        if (delay < DISPLAY_MIN_TIME) {
          XB_LOGD << "Too early " << ret->Buffer()->frame_no << " " << delay;
          ret = 0;
          break;
        }
        queue_.pop();
        if (delay <= DROP_TIME) {
          local_drops = 0;
          break;
        }
        local_drops++;
      } else {
        finished = 1;
        queue_.pop();
      }
    }
  } else if (queue_.size() > 0) {
    while (queue_.size() > 1) {
      queue_.pop();
    }
    ret = queue_.front();
    if (!ret) finished = 1;
    queue_.pop();
  } else {
  }
  if (ret) {
    last_frame_ = ret;
    int frame_no = last_frame_->Buffer()->frame_no;
    received_ = frame_no + 1;
    current_frate_ = (double)frame_no * 1000000.0 / (double)current;
    frame_dropped_ += local_drops;
    if (params_->show_drops_ && local_drops) {
      XB_LOGW << ret->Buffer()->frame_no << " - drop " << frame_dropped_ << " " << (delay / 1000) << "ms";
    }
  }
  if (finished) {
    XB_LOGI << "The average fps is " << current_frate_ << " (" << frame_dropped_ << " drops) in " << received_
            << " frames";
  }

  if (finished && queue_.empty()) {
    got_last_frame_ = 1;
    cv_.notify_one();
  }
  return last_frame_;
}

int Md5Checker::Initialize(const char* md5_file, bool for_stream, uint32_t start_frame) {
  std::string md5file(md5_file);
  md5file += ".md5";
  if (ParseMd5File(md5file.c_str())) return -1;
  current_ = start_frame;
  for_stream_ = for_stream;
  if (for_stream_) MD5Init(&md5_ctx_);
  return 0;
}

int Md5Saver::Put(aom_image_t* img) {
  char ss_md5[3];
  std::string frame_md5;
  if (!file_) {
    file_ = fopen(md5_file_.c_str(), "w");
    if (!file_) {
      return -1;
    }
  }
  MD5Init(&md5_ctx_);
  uint8_t* ptr = img->planes[0];
  uint32_t bpp = img->bit_depth == 8 ? 1 : 2;
  uint32_t w_c = (img->d_w + 1) / 2;
  uint32_t h_c = (img->d_h + 1) / 2;
  for (uint32_t i = 0; i < img->d_h; i++) {
    MD5Update(&md5_ctx_, ptr, img->d_w * bpp);
    ptr += img->stride[0];
  }
  if (!img->monochrome) {
    for (int k = 1; k < 3; k++) {
      uint8_t* ptr = img->planes[k];
      for (uint32_t i = 0; i < h_c; i++) {
        MD5Update(&md5_ctx_, ptr, w_c * bpp);
        ptr += img->stride[k];
      }
    }
  }
  MD5Final(md5_digest_, &md5_ctx_);
  for (int i = 0; i < 16; ++i) {
    sprintf_s(ss_md5, 3, "%02x", md5_digest_[i]);
    frame_md5.append(ss_md5, 2);
  }
  fprintf(file_, "%s   %d\n", frame_md5.c_str(), current_);
  current_++;
  return 0;
}

void Md5Checker::Unpack10x3(uint8_t* ptr_src, uint8_t* ptr_trg, int width_in_pix) {
  uint32_t triads = (width_in_pix + 5) / 6;
  uint32_t* col_ptr = (uint32_t*)ptr_trg;
  uint32_t* src_ptr = (uint32_t*)ptr_src;
  for (uint32_t i = 0; i < triads; i++) {
    uint32_t val = (src_ptr[0] & 0x000003FF) | ((src_ptr[0] & 0x000FFC00) << 6);
    col_ptr[0] = val;
    val = (src_ptr[0] & 0x3FF00000) >> 20 | ((src_ptr[1] & 0x000003FF) << 16);
    col_ptr[1] = val;
    val = (src_ptr[1] & 0x000FFC00) >> 10 | ((src_ptr[1] & 0x3FF00000) >> 4);
    col_ptr[2] = val;

    src_ptr += 2;
    col_ptr += 3;
  }
}

int Md5Checker::Put(aom_image_t* img) {
  char ss_md5[3];
  std::string frame_md5;
  int error = 0;
  uint8_t unpack_buff[4096 * 4];
  if (img->is_hdr10x3 && img->d_w > 9182) return 1;
  if (!for_stream_) MD5Init(&md5_ctx_);
  uint8_t* ptr = img->planes[0];
  uint32_t bpp = img->bit_depth == 8 ? 1 : 2;
  uint32_t w_c = (img->d_w + 1) / 2;
  uint32_t h_c = (img->d_h + 1) / 2;
  for (uint32_t i = 0; i < img->d_h; i++) {
    if (img->is_hdr10x3) {
      uint8_t* targ = unpack_buff;
      Unpack10x3(ptr, targ, img->d_w);
      MD5Update(&md5_ctx_, targ, img->d_w * bpp);
    } else {
      MD5Update(&md5_ctx_, ptr, img->d_w * bpp);
    }
    ptr += img->stride[0];
  }
  if (!img->monochrome) {
    for (int k = 1; k < 3; k++) {
      uint8_t* ptr = img->planes[k];
      for (uint32_t i = 0; i < h_c; i++) {
        if (img->is_hdr10x3) {
          uint8_t* targ = unpack_buff;
          Unpack10x3(ptr, targ, w_c);
          MD5Update(&md5_ctx_, targ, w_c * bpp);
        } else {
          MD5Update(&md5_ctx_, ptr, w_c * bpp);
        }
        ptr += img->stride[k];
      }
    }
  }
  if (!for_stream_) {
    MD5Final(md5_digest_, &md5_ctx_);
    for (int i = 0; i < 16; ++i) {
      sprintf_s(ss_md5, 3, "%02x", md5_digest_[i]);
      frame_md5.append(ss_md5, 2);
    }
    if (md5_strings_.empty()) {
      XB_LOGE << "MD5 checker doesn't have referenced md5 values";
    } else if (frame_md5.compare(md5_strings_[current_]) != 0) {
      XB_LOGE << "MD5 checksum error in " << current_ << " frame. Avaiting " << md5_strings_[current_] << " In real "
              << frame_md5;
      hasErrors_++;
      error = 1;
    }
  }
  current_++;
  return error;
}
void Md5Checker::Finalize() {
  char ss_md5[3];
  std::string frame_md5;

  MD5Final(md5_digest_, &md5_ctx_);
  for (int i = 0; i < 16; ++i) {
    sprintf_s(ss_md5, 3, "%02x", md5_digest_[i]);
    frame_md5.append(ss_md5, 2);
  }
  if (md5_strings_.empty()) {
    XB_LOGE << "MD5 checker doesn't have referenced md5 values";
  } else if (frame_md5.compare(md5_strings_[0]) != 0) {
    hasErrors_++;
  }
}

uint32_t Md5Checker::ShowResult(uint32_t fatal_error) {
  if (for_stream_) Finalize();
  if (hasErrors_) {
    if (for_stream_)
      XB_LOGE << "MD5 checksum doesn't match";
    else
      XB_LOGE << "MD5 checksum errors in " << hasErrors_ << " frames";
  } else if (!fatal_error)
    XB_LOGI << "MD5 is OK";
  if (fatal_error) XB_LOGE << "Unable to decode whole stream die to fatal error";
  return hasErrors_;
}

int Md5Checker::ParseMd5File(const char* md5_file) {
  md5_strings_.clear();
  std::ifstream input(md5_file);
  if (input.is_open()) {
    std::string line;

    while (std::getline(input, line)) {
      md5_strings_.push_back(line.substr(0, 32));
    }
  } else {
    XB_LOGE << "Invalid md5 file path '" << md5_file << "'";
    return -1;
  }

  return 0;
}

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

#include <stdint.h>
#include <memory>
#include <string>
#include <thread>
#include <mutex>
#include <queue>
#include <assert.h>
#include <fstream>
#include <chrono>
#include <condition_variable>
#include <atomic>

#include <wrl.h>
#include <wrl/client.h>
#include <d3d11.h>

#include "common/log.h"
#include "cmd_parser.h"
#include "common/utils.h"
#include "common/stream_reader.h"
#include "aom/aom_codec.h"
#include "aom/aom_decoder.h"
#include "aom/aomdx.h"
#include "common/frame_outs.h"

#include "common/md5_utils.h"
#include "dx/av1_perf.h"
#include "perf_stats.h"
#include "common/stage_dump.h"

#define DROP_TIME 8000
#define DISPLAY_MIN_TIME (-10000)
#define DEFAULT_FPS 60
#define MAGIG_IDX 0xFFFF
#define PRELOAD_SIZE 10
#define MAX_PLAYER_QUEUE_SIZE 16

#define STREAM_READER_CACHE_SIZE 20

#define CB_OUTPUT 1
#if CB_OUTPUT
#define ASYNC_OUTPUT 0
#else
#define ASYNC_OUTPUT 1
#endif

// using namespace DirectX;
using namespace Microsoft::WRL;

class DisplayImpl;
class AV1Decoder;
interface DisplayCallbacks;

#define AVAR_SIZE 8

typedef struct d3d_resources {
  ComPtr<ID3D12Device> dx12_device;
  ComPtr<ID3D12CommandQueue> dx12_command_queue;
  void* dx12_psos;
  d3d_resources(d3d_resources& other) {
    dx12_device = other.dx12_device;
    dx12_command_queue = other.dx12_command_queue;
    dx12_psos = other.dx12_psos;
  };
  d3d_resources(ID3D12Device* dx12_device, ID3D12CommandQueue* dx12_command_queue) {
    this->dx12_device = dx12_device;
    this->dx12_command_queue = dx12_command_queue;
  }
} d3d_resources;

class Md5Checker {
 public:
  int Initialize(const char* md5_file, bool for_stream, uint32_t start_frame);
  int Put(aom_image_t* img);
  void Finalize();
  uint32_t ShowResult(uint32_t fatal_error);
  static void Unpack10x3(uint8_t* ptr_src, uint8_t* ptr_trg, int width_in_pix);

 private:
  int ParseMd5File(const char* md5_file);
  std::string md5_file_;
  std::vector<std::string> md5_strings_;
  MD5Context md5_ctx_;
  unsigned char md5_digest_[16];
  uint32_t current_ = 0;
  uint32_t hasErrors_ = 0;
  bool for_stream_ = 0;
  //  bool initialized_ = false;
};

class Md5Saver {
 public:
  Md5Saver(const char* md5_file, uint32_t start_frame) : md5_file_(md5_file), current_(start_frame) {}
  ~Md5Saver() {
    if (file_) fclose(file_);
  }
  int Put(aom_image_t* img);

 private:
  std::string md5_file_;
  MD5Context md5_ctx_;
  unsigned char md5_digest_[16];
  uint32_t current_ = 0;
  FILE* file_ = 0;
};

struct StreamStats {
  double frate;
  uint32_t dropped;
  std::string fname;
};

class BufferLocker {
 public:
  BufferLocker(const CParameters::Parameters* params, uint32_t start_frame, std::shared_ptr<OutBufferStorage> storage)
      : params_(params), start_frame_(start_frame), buffers_pool_(storage) {
    if (params_->md5_frame_check_ || params_->md5_) {
      md5_checker_.reset(new Md5Checker());
      if (md5_checker_) {
        md5_checker_->Initialize(params_->source_file_.c_str(), params_->md5_, start_frame);
      }
    }
    if (!params_->save_md5_.empty()) {
      md5_saver_.reset(new Md5Saver(params_->save_md5_.c_str(), start_frame));
    }
  }
  ~BufferLocker() { Finalize(); }
  bool IsEmpty() {
    std::lock_guard<std::mutex> lock(cs_);
    return queue_.empty();
  }
  void WaitEmpty() {
    std::unique_lock<std::mutex> lock(cs_);
    cv_.wait(lock, [this] { return !this->queue_.empty(); });
  }
  void WaitLast() {
    std::unique_lock<std::mutex> lock(cs_);
    cv_.wait(lock, [this] { return this->got_last_frame_; });
  }

  int Push(aom_image_t* img);
  std::shared_ptr<OutBufferWrapper> Pop();
  int Drops() const { return frame_dropped_; }
  double FRate() { return current_frate_; }
  size_t Size() {
    std::lock_guard<std::mutex> lock(cs_);
    return queue_.size();
  }
  int hasFreeBuffers() { return buffers_pool_->GetSize() > 0; }

  int SaveToFile(aom_image_t* img, bool one_frame);
  int GetBufferInt(size_t buf_size, uint16_t w, uint16_t h, frame_buffer_type fb_type, av1_decoded_frame_buffer_t* fb);
  int ReleaseBufferInt(void* buffer_priv);
  void Flush();
  void Finalize() {
    if (!finalized_) {
      if (out_file_) fclose(out_file_);
      out_file_ = 0;
      if (md5_checker_) has_md5_errors_ = md5_checker_->ShowResult(hasFatalError_);
      // The task's md5 state goes with the task: the reference list of the checker, and the saver, whose
      // file is closed (and so complete on disk) here instead of when the next task replaces this queue.
      md5_checker_.reset();
      md5_saver_.reset();
      last_frame_.reset();
      // buffers_pool_.reset();
      finalized_ = true;
      started_ = 0;
    }
  }

 private:
  std::queue<std::shared_ptr<OutBufferWrapper> > queue_;
  std::shared_ptr<OutBufferWrapper> last_frame_;
  std::unique_ptr<Md5Checker> md5_checker_;
  std::unique_ptr<Md5Saver> md5_saver_;
  std::mutex cs_;
  StreamTimer timer_;
  int frame_dropped_ = 0;
  FILE* out_file_ = 0;
  std::condition_variable cv_;
  uint32_t received_ = 0;
  double current_frate_ = 0.0;
  std::shared_ptr<OutBufferStorage> buffers_pool_;
  int frame_no_ = 0;
  int frame_counter_ = 0;
  int got_last_frame_ = 0;
  // Frames the decoder has handed over so far, for the MEMORY lines of the decode loop.
  std::atomic<int> pushed_{0};
  // -nopresent: frames handed straight back to the pool
  int released_ = 0;
  std::chrono::steady_clock::time_point released_first_;
  std::chrono::steady_clock::time_point released_last_;
  const CParameters::Parameters* params_;
  uint32_t hasFatalError_ = 0;
  uint32_t width_ = 0;
  uint32_t height_ = 0;
  uint32_t bpp_ = 0;
  uint32_t start_frame_ = 0;
  ComPtr<ID3D12Device> dx12_device_;
  bool has_md5_errors_ = false;
  bool finalized_ = false;
  int started_ = 0;
  friend class AV1Decoder;
};

class AV1Decoder {
 public:
  AV1Decoder(d3d_resources dx_resources, DisplayCallbacks* display, uint32_t* stop = 0)
      : dx_desources_(dx_resources), display_(display), stop_(stop) {}
  ~AV1Decoder() {
    if (initialized_) {
      aom_codec_destroy(&decctx_);
    }
    if (perf_installed_) av1_set_perf_callback(NULL, NULL);
    if (dumps_installed_) av1_set_stage_dump(NULL, NULL, NULL, 0);
  }
  // "MEMORY app <where>" and "MEMORY dxgi <where>": the memory of the app (Windows::System::MemoryManager)
  // and the video memory of the device's adapter, at a step of the launch or of a task (decoder.cpp).
  static void LogMemory(ID3D12Device* device, const char* where);
  void Start(CParameters* params) {
    need_reinit_ = params && (params->GetParams().threads_ != params_.threads_ ||
                              params->GetParams().player_queue_size_ != params_.player_queue_size_ ||
                              params->GetParams().slots_ != params_.slots_ ||  // a decoder creation parameter
                              params->GetParams().recreate_);
    params_ = params->GetParams();
    stop_decode_ = 0;
    decode_thread_ = std::thread(RunDecoderLoop, this);
    //        DecodeLoop();
  }

  void Stop() {
    stop_decode_ = 1;
    WaitForFinish();
  }

  void WaitForFinish() { decode_thread_.join(); }

  std::shared_ptr<OutBufferWrapper> GetFrame(StreamStats* stats = 0);
  void FlushOutput();

  static int GetBuffer(void* priv, size_t buf_size, uint16_t w, uint16_t h, frame_buffer_type fb_type,
                       av1_decoded_frame_buffer_t* fb) {
    if (!priv) return -1;
    AV1Decoder* This = static_cast<AV1Decoder*>(priv);
    return This->output_queue_->GetBufferInt(buf_size, w, h, fb_type, fb);
  }
  static int ReleaseBuffer(void* priv, void* buffer_priv) {
    if (!priv) return -1;
    AV1Decoder* This = static_cast<AV1Decoder*>(priv);
    return This->output_queue_->ReleaseBufferInt(buffer_priv);
  }
  static int NotifyFrameReady(void* priv, void* img) {
    if (!priv) return -1;
    AV1Decoder* This = static_cast<AV1Decoder*>(priv);
    if (img && This->params_.gputime_) This->PerfOutput();
    // The drain of a decoder that is replaced in the middle of a stream: more frames follow.
    if (!img && This->reconfiguring_) return 0;
    return This->output_queue_->Push((aom_image_t*)img);
  }
  // The decoder's budget guard: the bytes the decoder may still allocate (decoder.cpp).
  static int64_t MemoryAvailable(void* priv);
  // The decoder's log lines (av1_set_log_callback), from whatever thread the decoder is on.
  static void LogLine(void* priv, int is_error, const char* line) {
    (void)priv;
    if (is_error)
      XB_LOGE << line;
    else
      XB_LOGI << line;
  }
  // -gputime: one record per decoded frame, from the decoder's GPU completion thread.
  static void PerfFrame(void* priv, const av1_perf_frame* frame) {
    if (!priv || !frame) return;
    AV1Decoder* This = static_cast<AV1Decoder*>(priv);
    std::lock_guard<std::mutex> lock(This->perf_cs_);
    This->perf_stats_.AddFrame(*frame);
  }

 protected:
  static void RunDecoderLoop(AV1Decoder* ptr) { ptr->DecodeLoop(); }
  static void RunFetchLoop(AV1Decoder* ptr) { ptr->FetchLoop(); }
  int DecodeLoop();
  int DecodeOne();
  void LogMemory(const char* where) { LogMemory(dx_desources_.dx12_device.Get(), where); }
  // aom_codec_destroy with the MEMORY lines before and after it.
  void DestroyDecoder();
  // After a decoder and its output pool were released: waits, bounded, until the app's memory is back at
  // what it was without a decoder, and logs how long that took and what stays charged.
  void SettleAppMemory();
  void PerfOutput() {
    const double now_ms =
        std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now().time_since_epoch()).count();
    std::lock_guard<std::mutex> lock(perf_cs_);
    perf_stats_.AddOutput(now_ms);
  }
  void PerfReport();
  int FetchLoop();
  void StartFetch() {
    stop_fetch_ = 0;
    fetch_thread_ = std::thread(RunFetchLoop, this);
  }
  void StopFetch() {
    stop_fetch_ = 1;
    fetch_thread_.join();
  }

 private:
  std::thread fetch_thread_;
  std::thread decode_thread_;
  CParameters::Parameters params_;
  volatile int is_running_ = 0;
  aom_codec_ctx_t decctx_;
  volatile int stop_decode_ = 0;
  volatile int stop_fetch_ = 0;
  d3d_resources dx_desources_;
  uint32_t* stop_;
  std::unique_ptr<BufferLocker> output_queue_;
  int need_preload_ = 1;
  uint32_t preload_num_ = PRELOAD_SIZE;
  DisplayCallbacks* display_ = 0;
  std::vector<uint8_t> host_mem_;
  std::shared_ptr<OutBufferStorage> storage_;
  int initialized_ = 0;
  int need_reinit_ = 0;
  // Set around the drain of a decoder that is replaced for a new, larger sequence of the same stream.
  volatile int reconfiguring_ = 0;
  // What the decoder in decctx_ was created with (valid while initialized_). A task that needs anything else
  // gets a new decoder, also when the task before it ended before it reached the decoder.
  int created_threads_ = 0;
  int created_queue_size_ = 0;
  int created_slots_ = 0;
  unsigned int created_width_ = 0;
  unsigned int created_height_ = 0;
  unsigned int created_bitdepth_ = 0;
  // Estimate of the output pool's textures for the stream, for MemoryAvailable.
  uint64_t output_pool_estimate_ = 0;
  // The app's memory at the first task of the launch: pipeline states created, no decoder, no host pool, no
  // output pool, no stream open. What a release should get back to. 0: not known.
  uint64_t app_baseline_ = 0;
  bool app_baseline_taken_ = false;
  // A decoder was destroyed since the last creation: the next creation settles first (SettleAppMemory).
  bool settle_pending_ = false;
  bool created_intrav2_ = false;
  bool created_interfast_ = false;
  int created_lowmem_ = 0;
  perfstats::PerfStats perf_stats_;
  std::mutex perf_cs_;
  int perf_installed_ = 0;
  // -dumpstages: the files of the task's stage dumps.
  StageDumpWriter stage_dumps_;
  int dumps_installed_ = 0;
};
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

#define PLANE_WIDTH_ALIGMENT 256
#define PLANE_WIDTH_ALIGMENT1 (PLANE_WIDTH_ALIGMENT - 1)

using sys_clock = std::chrono::system_clock;
using time_point = sys_clock::time_point;
// using duration_uint64 = std::chrono::duration<uint64_t, std::micro>;

static const int direct_mem_align = 16 * 1024;

char* pdummy = 0;
int AV1Decoder::DecodeLoop() {
  char dummy;
  pdummy = &dummy;
  is_running_ = 1;
  for (int i = 0; i < params_.loops_; i++) {
    XB_LOGI << " ";
    XB_LOGI << "start loop " << i << " for " << params_.source_file_;
    DecodeOne();
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
  if (initialized_ && need_reinit_) {
    XB_LOGI << "Destroy decoder";
    aom_codec_destroy(&decctx_);
    memset(&decctx_, 0, sizeof(decctx_));
    initialized_ = 0;
  }
  if (params_.recreate_ || !initialized_) {
    XB_LOGI << "(Re)create decoder";
    try {
      storage_.reset();
      host_mem_.clear();
      host_mem_.shrink_to_fit();
      aom_codec_dec_cfg_t cfg;
      memset(&cfg, 0, sizeof(aom_codec_dec_cfg_t));
      cfg.width = 4096;
      cfg.height = 2176;
      cfg.bitdepth = 10;
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

      if (aom_codec_dec_init(&decctx_, aom_codec_av1_dx(), &cfg, dec_flags)) {
        host_mem_.clear();
        host_mem_.shrink_to_fit();
        XB_LOGE << "Failed to initialize decoder: " << aom_codec_error(&decctx_);
        throw 2;
      }
      storage_ = std::unique_ptr<OutBufferStorage>(
          new OutBufferStorage(params_.player_queue_size_, dx_desources_.dx12_device.Get()));
      initialized_ = 1;
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
  }
  // skip non-key frames (if start_frame_ != 0)
  int is_key = 0;
  buffer.Reset();
  aom_codec_stream_info_t si;
  memset(&si, 0, sizeof(si));
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
  while (buffer && limit-- && !(*stop_) && !output_queue_->hasFatalError_) {
    XB_LOGD << cnt++ << " - " << buffer->Size();
    uint64_t user_priv = static_cast<uint64_t>(frame_in++ * framedur);
    if (aom_codec_decode(&decctx_, buffer->Data(), buffer->Size(), (void*)user_priv)) {
      XB_LOGE << "aom_codec_decode error " << aom_codec_error(&decctx_);
      has_error = 1;
      output_queue_->hasFatalError_ = 1;
      break;
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
    buffer = reader.GetData();
  }
  // drain
  aom_codec_decode(&decctx_, 0, 0, 0);
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
  if (has_error || params_.recreate_) {
    XB_LOGI << "Destroy decoder";
    aom_codec_destroy(&decctx_);
    memset(&decctx_, 0, sizeof(decctx_));
    initialized_ = 0;
  }
  output_queue_->Finalize();
  uint32_t has_md5_errors = output_queue_->has_md5_errors_;
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
    queue_.push(0);
    return 0;
  }
  if (got_last_frame_) got_last_frame_ = 0;
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
    if ((params_->md5_frame_check_ || params_->md5_) && img) {
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

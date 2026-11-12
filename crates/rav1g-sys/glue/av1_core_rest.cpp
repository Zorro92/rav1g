/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// What the cargo build still compiles of libav1/dx/av1_core.cpp (step 4 of docs/rust-port.md): the text of
// that file without the parts Rust has (crates/rav1g/src/host), which are called through the extern "C"
// functions declared below. libav1/dx/av1_core.cpp itself stays whole for the MSBuild projects. This file
// shrinks with every part of step 4 and goes with the last.
//
// In Rust so far: the creation and the release of a decoder with the buffer plan and the budget guard, the
// memory query, av1_allocate_pbi, and the pool of frame buffers (create.rs); the timers and timestamps of
// the timing run (perf.rs); the stage dumps (stage_dump.rs); the setup of a frame and the other functions
// the entropy decoder calls, the command buffer of a frame and its submission (frame.rs).
//
// Left here: the completion thread, the queue of output images, the drain, and show_existing_frame.

#include "dx/av1_core.h"
#include "dx/av1_memory.h"
#include "dx/av1_compute.h"
#include <assert.h>
#include <stdio.h>
#include <new>
#include "dx/types.h"
#include "av1/decoder/decoder.h"
#include "av1\common\scan.h"
#include "av1\common\idct.h"
#include "av1\common\filter.h"
#include "av1/common/reconinter.h"
#include "av1/common/warped_motion.h"
#include "av1/common/reconintra.h"
#include "aom_dsp/intrapred_common.h"


int av1_postprocess_copy_output(Av1Core *dec, AV1_COMMON *cm);

static THREADFN gpu_thread_hook(void *pdata);

extern "C" {
void rav1g_frame_buffer_acquire(Av1Core *dec, HwFrameBuffer *fb);
int rav1g_release_fb(Av1Core *dec, HwFrameBuffer *fb);
void rav1g_release_unsubmitted_frame(Av1Core *dec);
void rav1g_prepare_command_buffer(Av1Core *dec);
void rav1g_commit_command_buffer(Av1Core *dec);

int64_t rav1g_perf_now(void);
void rav1g_perf_add(Av1Core *dec, int id, int64_t start_ticks);
void rav1g_perf_collect(Av1Core *dec, av1_frame_thread_data *td, int64_t wait_start, av1_perf_frame *rec);
void rav1g_perf_deliver(const av1_perf_frame *rec);
void rav1g_stage_dump_deliver(av1_frame_thread_data *td);

// The frame setup (Rust) starts the completion thread when it does not run.
void rav1g_cpp_start_gpu_thread(Av1Core *dec) {
  if (!dec->gpu_thread) pthread_create(&dec->gpu_thread, 0, (LPTHREAD_START_ROUTINE)gpu_thread_hook, dec);
}
}

// What this file calls of the Rust, under the names it had in av1_core.cpp.
static inline int64_t perf_now() { return rav1g_perf_now(); }
void av1_perf_add(Av1Core *dec, int id, int64_t start_ticks) { rav1g_perf_add(dec, id, start_ticks); }
static void perf_collect(Av1Core *dec, av1_frame_thread_data *td, int64_t wait_start, av1_perf_frame *rec) {
  rav1g_perf_collect(dec, td, wait_start, rec);
}
static void stage_dump_deliver(av1_frame_thread_data *td) { rav1g_stage_dump_deliver(td); }
static void frame_buffer_acquire(Av1Core *dec, HwFrameBuffer *fb) { rav1g_frame_buffer_acquire(dec, fb); }
static int release_fb(Av1Core *dec, HwFrameBuffer *fb) { return rav1g_release_fb(dec, fb); }
static void release_unsubmitted_frame(Av1Core *dec) { rav1g_release_unsubmitted_frame(dec); }
static void av1_prepare_command_buffer(Av1Core *dec) { rav1g_prepare_command_buffer(dec); }
static void av1_commit_command_buffer(Av1Core *dec) { rav1g_commit_command_buffer(dec); }

// GPU completion thread, -asyncclear: the frame that used |td| has completed on the GPU, so nothing reads the
// slot's grids any more, and the slot is not in the pool, so the decode thread cannot have it. Clear what the
// frame cleared at its setup; the next frame of the same size then starts on a slot that is already clean.
// Returns the bytes cleared.
static size_t async_clear_slot(av1_frame_thread_data *td) {
  size_t bytes = 0;
  if (td->clear_pending && td->async_clear) {
    memset(td->mode_info_grid->host_ptr, 0, td->clear_mi_bytes);
    // The luma grid and the chroma grid behind it, as one range (the two memsets of av1_setup_frame).
    if (td->clear_iter_bytes) memset(td->gen_intra_inter_grid->host_ptr, -1, td->clear_iter_bytes);
    td->clean_mi_bytes = td->clear_mi_bytes;
    td->clean_iter_bytes = td->clear_iter_bytes;
    bytes = td->clear_mi_bytes + td->clear_iter_bytes;
  }
  td->clear_pending = 0;
  return bytes;
}

void copy_to_img(Av1Core *dec, HwOutputImage *hw_img, aom_image_t *dst) {
  dst->bit_depth = hw_img->hbd ? 10 : 8;
  dst->w = hw_img->y_crop_width;
  dst->h = hw_img->y_crop_height;
  dst->d_w = hw_img->y_crop_width;
  dst->d_h = hw_img->y_crop_height;
  dst->r_w = hw_img->y_crop_width;
  dst->r_h = hw_img->y_crop_height;
  dst->x_chroma_shift = 1;
  dst->y_chroma_shift = 1;
  dst->planes[AOM_PLANE_Y] = hw_img->fb_ptr + hw_img->planes[0].offset;
  dst->planes[AOM_PLANE_U] = hw_img->fb_ptr + hw_img->planes[1].offset;
  dst->planes[AOM_PLANE_V] = hw_img->fb_ptr + hw_img->planes[2].offset;
  dst->stride[AOM_PLANE_Y] = hw_img->planes[0].stride;
  dst->stride[AOM_PLANE_U] = hw_img->planes[1].stride;
  dst->stride[AOM_PLANE_V] = hw_img->planes[2].stride;
  dst->user_priv = hw_img->user_priv;
  dst->fb2_priv = hw_img->alloc_priv;
  dst->monochrome = hw_img->monochrome;
  if (hw_img->monochrome) {
    dst->planes[AOM_PLANE_U] = NULL;
    dst->planes[AOM_PLANE_V] = NULL;
  }
  dst->is_hdr10x3 = dec->tryhdr10x3;
  hw_img->fb_ptr = NULL;
  hw_img->alloc_priv = NULL;
  hw_img->user_priv = NULL;
  hw_img->is_valid = 0;
}

extern "C" int get_output_frame(AV1Decoder *pbi, aom_image_t *dst) {
  Av1Core *dec = pbi->gpu_decoder;
  if (MTQueueIsEmpty(&dec->output_queue)) return 1;
  HwOutputImage *hw_img = (HwOutputImage *)MTQueueGet(&dec->output_queue);
  copy_to_img(dec, hw_img, dst);
  MTQueuePush(&dec->image_pool, hw_img);
  return 0;
}

void av1_sync_gpu(av1_frame_thread_data *td) {
  ComputeCommandBuffer *cb = &td->command_buffer;
  WaitForSingleObject(cb->event, INFINITE);
}

THREADFN gpu_thread_hook(void *data) {
  Av1Core *dec = (Av1Core *)data;
  while (1) {
    GpuWorkItem *item = (GpuWorkItem *)MTQueueGet(&dec->gpu_waiting_queue);
    if (!item) {
      if (dec->cb_notify_frame_ready && dec->image_alloc_priv) dec->cb_notify_frame_ready(dec->image_alloc_priv, 0);
      break;
    }

    av1_frame_thread_data *td = item->data;
    av1_perf_frame perf_rec = {};
    int perf_have_rec = 0;
    if (td) {
      // sync gpu
      const int64_t perf_wait_start = td->perf.collect ? perf_now() : 0;
      av1_sync_gpu(td);
      // -dumpstages: the copies of the frame's stages, while the slot is still this frame's.
      if (td->dump_count) stage_dump_deliver(td);
      if (perf_wait_start) {
        // Read the frame's timestamps now: its fence is done and the slot has
        // not gone back to the pool yet.
        perf_collect(dec, td, perf_wait_start, &perf_rec);
        perf_have_rec = 1;
      }
      pthread_mutex_lock(&dec->fb_pool_mutex);
      int signal = 0;
      if (td->back_buffer0) signal |= release_fb(dec, td->back_buffer0);
      signal |= release_fb(dec, td->dst_frame_buffer);
      for (int r = 0; r < 7; ++r) {
        if (td->refs[r]) signal |= release_fb(dec, td->refs[r]);
        td->refs[r] = 0;
      }
      td->dst_frame_buffer = 0;
      td->back_buffer0 = 0;
      if (signal) {
        pthread_cond_signal(&dec->fb_pool_empty_cond);
      }
      if (td->sec_thread_data) {
        MTQueuePush(&dec->frame_data_pool, td->sec_thread_data);
      }
      pthread_mutex_unlock(&dec->fb_pool_mutex);
      td->sec_thread_data = NULL;
      // -asyncclear: clear the slot's grids here instead of on the decode thread. A second slot went back
      // above as it is: a frame uses only the coefficient and mode info shares of its second slot, not the
      // grids, so that slot is as clean as it was.
      const int64_t perf_clear_start = perf_have_rec ? perf_now() : 0;
      const size_t cleared = async_clear_slot(td);
      if (perf_clear_start && cleared) {
        LARGE_INTEGER qpf;
        QueryPerformanceFrequency(&qpf);
        perf_rec.cpu_ms[AV1_PERF_CPU_ASYNC_CLEAR] =
            (double)(perf_now() - perf_clear_start) * 1000.0 / (double)qpf.QuadPart;
        perf_rec.async_clear_bytes = (int64_t)cleared;
      }
      // A pipeline slot, or the output context of a show_existing_frame copy.
      MTQueuePush(td->output_only ? &dec->output_data_pool : &dec->frame_data_pool, td);
    }

    HwOutputImage *hw_img = item->image;
    if (hw_img) {
      if (dec->cb_notify_frame_ready && dec->image_alloc_priv) {
        aom_image_t img = {};
        aom_image_t *dst = &img;
        copy_to_img(dec, hw_img, dst);
        MTQueuePush(&dec->image_pool, hw_img);
        const int64_t perf_notify_start = perf_have_rec ? perf_now() : 0;
        dec->cb_notify_frame_ready(dec->image_alloc_priv, dst);
        if (perf_notify_start) {
          LARGE_INTEGER qpf;
          QueryPerformanceFrequency(&qpf);
          perf_rec.cpu_ms[AV1_PERF_CPU_NOTIFY] = (double)(perf_now() - perf_notify_start) * 1000.0 / (double)qpf.QuadPart;
        }
      } else {
        MTQueuePush(&dec->output_queue, hw_img);
      }
    }
    if (perf_have_rec) {
      rav1g_perf_deliver(&perf_rec);
    }
    MTQueuePush(&dec->gpu_item_pool, item);
  }

  return THREAD_RETURN(0);
}

void av1_drain_gpu_decoder(Av1Core *dec) {
  if (dec && dec->gpu_thread) {
    MTQueuePush(&dec->gpu_waiting_queue, NULL);
    pthread_join(dec->gpu_thread, 0);
    dec->gpu_thread = NULL;
  }
}

// show_existing_frame: the frame in cm->cur_frame (the frame to show; the header reader has put it there,
// set cm->show_frame and loaded the frame's film grain parameters into cm->film_grain_params, the
// load_grain_params of the specification) is output now. The frame was decoded earlier; it took no output
// image then unless it was shown then, and that image went to the application. So the output is made here:
// an output image from the application, and a command list of its own that copies the frame buffer into it,
// with film grain.
//
// Order on the GPU: the command list of the frame's decode was submitted by this thread before this header
// was read, and the lists run on the one queue in the order of their submission, so the copy reads the
// finished frame. Nothing writes a frame buffer after the frame's own list.
// Lifetime: the copy holds a reference to the frame buffer (frame_buffer_acquire, taken while the reference
// map still holds the frame) until the completion thread has seen the fence of the copy, so the buffer
// cannot go back to the pool and be written by a later frame before the copy has run, whatever the stream
// does to the reference map in between.
// Output order: the work item is queued behind those of the frames before it, and the completion thread
// hands the images over in that order.
//
// The copy takes an output context, not a pipeline slot: a slot would be one frame less in flight for as
// long as the copy waits behind the frames in front of it on the queue.
extern "C" void av1_decode_sef(AV1Decoder *pbi) {
  Av1Core *dec = pbi->gpu_decoder;
  AV1_COMMON *cm = &pbi->common;
  YV12_BUFFER_CONFIG *buf = &cm->cur_frame->buf;
  // A frame before this one that failed after its setup.
  release_unsubmitted_frame(dec);
  buf->hw_show_image = NULL;
  // Nothing to show: no frame was ever decoded into this entry.
  if (!buf->hw_buffer) return;
  // Not the frame of its temporal unit that is output: a frame of a higher spatial layer follows.
  if (!av1_frame_is_output(pbi)) return;

  int64_t perf_t = av1_perf_ticks();
  if (perf_t) {
    dec->perf_cur.t_frame_start = perf_t;
    dec->perf_cur.is_sef = 1;
    dec->perf_cur.show_frame = 1;
    dec->perf_cur.intra_iters = -1;
    dec->perf_cur.frame_number = buf->hw_buffer->frame_number;
  }
  // Waits when both contexts are in flight: until the copy of an earlier show_existing_frame has completed.
  av1_frame_thread_data *td = (av1_frame_thread_data *)MTQueueGet(&dec->output_data_pool);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_SLOT, perf_t);
  td->sec_thread_data = NULL;
  td->back_buffer0 = NULL;
  memset(td->refs, 0, sizeof(td->refs));
  td->clear_pending = 0;
  td->frame_number = buf->hw_buffer->frame_number;
  td->dst_frame_buffer = buf->hw_buffer;
  td->frame_buffer = buf->hw_buffer;
  td->is_hbd = buf->bit_depth > 8;
  td->bitdepth = buf->bit_depth;
  td->shaders = td->is_hbd ? &dec->shader_lib->shaders_hbd : &dec->shader_lib->shaders_8bit;
  td->do_cdef = 0;
  td->do_loop_rest = 0;
  td->do_superres = 0;
  td->do_filmgrain = cm->film_grain_params.apply_grain;
  td->output_frame = 1;
  frame_buffer_acquire(dec, td->dst_frame_buffer);
  // From here on release_unsubmitted_frame() can take the context back if the output image cannot be had.
  dec->curr_frame_data = td;
  // The completion thread ends with a drain; a show_existing_frame may be the first thing after one.
  if (!dec->gpu_thread) pthread_create(&dec->gpu_thread, 0, (LPTHREAD_START_ROUTINE)gpu_thread_hook, dec);

  av1_prepare_command_buffer(dec);
  if (td->dump_active)
    snprintf(td->dump_info, sizeof(td->dump_info), "show_existing_frame film_grain=%d", td->do_filmgrain);
  av1_postprocess_copy_output(dec, cm);
  av1_commit_command_buffer(dec);

  perf_t = av1_perf_ticks();
  GpuWorkItem *item = (GpuWorkItem *)MTQueueGet(&dec->gpu_item_pool);
  av1_perf_add(dec, AV1_PERF_CPU_WAIT_ITEM, perf_t);
  item->data = td;
  item->image = buf->hw_show_image;
  buf->hw_show_image = NULL;
  dec->curr_frame_data = NULL;
  if (item->image) item->image->user_priv = pbi->user_priv;
  MTQueuePush(&dec->gpu_waiting_queue, item);
}


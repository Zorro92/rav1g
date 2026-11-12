/*
 * Copyright 2020 Google LLC
 *
 */

 /*
 * Copyright (c) 2016, Alliance for Open Media. All rights reserved
 *
 * This source code is subject to the terms of the BSD 2 Clause License and
 * the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
 * was not distributed with this source code in the LICENSE file, you can
 * obtain it at www.aomedia.org/license/software. If the Alliance for Open
 * Media Patent License 1.0 was not distributed with this source code in the
 * PATENTS file, you can obtain it at www.aomedia.org/license/patent.
 */


#include <stdlib.h>
#include <string.h>

#include "config/aom_config.h"
#include "config/aom_version.h"

#include "aom/internal/aom_codec_internal.h"
#include "aom/aomdx.h"
#include "aom/aom_decoder.h"
#include "aom_dsp/bitreader_buffer.h"
#include "aom_dsp/aom_dsp_common.h"
#include "aom_ports/mem_ops.h"
#include "aom_util/aom_thread.h"

#include "av1/common/alloccommon.h"
#include "av1/common/frame_buffers.h"
#include "av1/common/enums.h"
#include "av1/common/obu_util.h"
#include "av1/common/seq_header_peek.h"

#include "av1/decoder/decoder.h"
#include "av1/decoder/decodeframe.h"
#include "av1/decoder/obu.h"

#include "av1/av1_iface_common.h"
#include "dx/av1_core.h"

struct aom_codec_alg_priv {
  aom_codec_priv_t base;
  aom_codec_dec_cfg_t cfg;
  aom_codec_stream_info_t si;
  int postproc_cfg_set;
  aom_postproc_cfg_t postproc_cfg;
  aom_image_t img;
  int img_avail;
  int flushed;
  int invert_tile_order;
  RefCntBuffer *last_show_frame;  // Last output frame buffer
  int byte_alignment;
  int skip_loop_filter;
  int skip_film_grain;
  int decode_tile_row;
  int decode_tile_col;
  unsigned int tile_mode;
  unsigned int ext_tile_debug;
  unsigned int row_mt;
  EXTERNAL_REFERENCES ext_refs;
  unsigned int is_annexb;
  int operating_point;
  int output_all_layers;

  struct Av1Core * gpu_decoder;

  AV1Decoder * pbi;

  int need_resync;  // wait for key/intra-only frame
  // BufferPool that holds all reference frames. Shared by all the FrameWorkers.
  BufferPool *buffer_pool;

  // External frame buffer info to save for AV1 common.
  void *ext_priv;  // Private data associated with the external frame buffers.
  aom_get_frame_buffer_cb_fn_t get_ext_fb_cb;
  aom_release_frame_buffer_cb_fn_t release_ext_fb_cb;

#if CONFIG_INSPECTION
  aom_inspect_cb inspect_cb;
  void *inspect_ctx;
#endif
};


static void set_error_detail(aom_codec_alg_priv_t *ctx,
    const char *const error) {
    ctx->base.err_detail = error;
}


static void init_buffer_callbacks(aom_codec_alg_priv_t *ctx) {
    AV1_COMMON *const cm = &ctx->pbi->common;
    BufferPool *const pool = cm->buffer_pool;

    cm->cur_frame = NULL;
    cm->byte_alignment = ctx->byte_alignment;
    cm->skip_loop_filter = ctx->skip_loop_filter;
    cm->skip_film_grain = ctx->skip_film_grain;

    pool->get_fb_cb = NULL;
    pool->release_fb_cb = NULL;
    pool->release_fb_cb_int = av1_release_fb_callback;
    pool->cb_priv = ctx->pbi->gpu_decoder;
}

static void set_default_ppflags(aom_postproc_cfg_t *cfg) {
    cfg->post_proc_flag = AOM_DEBLOCK | AOM_DEMACROBLOCK;
    cfg->deblocking_level = 4;
    cfg->noise_level = 0;
}

aom_codec_err_t av1_codec_query_memory_requirements(aom_codec_dec_cfg_t *cfg)
{
    if (!cfg)
        return AOM_CODEC_INVALID_PARAM;
    if (av1_query_memory_requirements(cfg))
        return AOM_CODEC_UNSUP_FEATURE;
    return AOM_CODEC_OK;
}

static aom_codec_err_t decoder_init(aom_codec_ctx_t *ctx,
                                    aom_codec_priv_enc_mr_cfg_t *data) {
  // This function only allocates space for the aom_codec_alg_priv_t
  // structure. More memory may be required at the time the stream
  // information becomes known.
  (void)data;

  if (!ctx->priv) {
    const int alg_priv_size = (sizeof(aom_codec_alg_priv_t) + 31) & ~31;
    const aom_codec_dec_cfg_t * cfg = ctx->cfg;
    if (!cfg || !cfg->host_memory || cfg->host_size < alg_priv_size)
        return AOM_CODEC_MEM_ERROR;
    aom_codec_alg_priv_t *const priv = (aom_codec_alg_priv_t *)cfg->host_memory;
    priv->cfg = *cfg;
    priv->cfg.allow_lowbitdepth = CONFIG_LOWBITDEPTH;
    ctx->priv = (aom_codec_priv_t *)priv;
    ctx->priv->init_flags = ctx->init_flags;
    priv->flushed = 0;
    ctx->cfg = &priv->cfg;
    priv->cfg.cfg.ext_partition = 1;
    priv->row_mt = 0;
    priv->tile_mode = 0;
    priv->decode_tile_row = -1;
    priv->decode_tile_col = -1;
    priv->last_show_frame = NULL;
    priv->need_resync = 1;
    priv->flushed = 0;
    // The decoder lives in the application's memory, which need not be zero. The first operating point of
    // the stream and one frame per temporal unit unless a control says otherwise; no Annex B stream (no
    // control sets it).
    priv->operating_point = 0;
    priv->output_all_layers = 0;
    priv->is_annexb = 0;
    priv->cfg.host_size -= alg_priv_size;
    priv->cfg.host_memory = (uint8_t *)priv->cfg.host_memory + alg_priv_size;
    // A failed creation is destroyed through decoder_destroy(): these must not be whatever the host memory held.
    priv->gpu_decoder = NULL;
    priv->pbi = NULL;
    priv->buffer_pool = NULL;
    if (av1_create_gpu_decoder(&priv->gpu_decoder, &priv->cfg)) {
        // The creation has released everything it built. The line that names what failed is the detail.
        set_error_detail(priv, av1_last_create_error());
        return AOM_CODEC_MEM_ERROR;
    }
    av1_allocate_pbi(priv->gpu_decoder, &priv->pbi, &priv->buffer_pool);
#if CONFIG_MULTITHREAD
    if (pthread_mutex_init(&priv->buffer_pool->pool_mutex, NULL)) {
        set_error_detail(priv, "Failed to allocate buffer pool mutex");
        return AOM_CODEC_MEM_ERROR;
    }
#endif


    if (av1_decoder_create(priv->pbi, priv->buffer_pool))
    {
        set_error_detail(priv, "Failed to create decoder");
        return AOM_CODEC_ERROR;
    }

    priv->pbi->common.options = &priv->cfg.cfg;
    priv->pbi->allow_lowbitdepth = priv->cfg.allow_lowbitdepth;
    priv->pbi->max_threads = AOMMIN(8, priv->cfg.threads);
    priv->pbi->inv_tile_order = priv->invert_tile_order;
    priv->pbi->common.large_scale_tile = priv->tile_mode;
    priv->pbi->common.is_annexb = priv->is_annexb;
    priv->pbi->dec_tile_row = priv->decode_tile_row;
    priv->pbi->dec_tile_col = priv->decode_tile_col;
    priv->pbi->operating_point = priv->operating_point;
    priv->pbi->output_all_layers = priv->output_all_layers;
    priv->pbi->ext_tile_debug = priv->ext_tile_debug;
    priv->pbi->row_mt = priv->row_mt;
    priv->pbi->gpu_decoder = priv->gpu_decoder;

    // If postprocessing was enabled by the application and a
    // configuration has not been provided, default it.
    set_default_ppflags(&priv->postproc_cfg);

    init_buffer_callbacks(priv);
  }

  return AOM_CODEC_OK;
}

static aom_codec_err_t decoder_destroy(aom_codec_alg_priv_t *ctx) {
    if (ctx->pbi)
    {
    //    aom_free(ctx->pbi->common.tpl_mvs);
        ctx->pbi->common.tpl_mvs = NULL;
        av1_remove_common(&ctx->pbi->common);
    //    av1_free_restoration_buffers(&ctx->pbi->common);
        av1_decoder_remove(ctx->pbi);
    }
#if CONFIG_MULTITHREAD
    if (ctx->buffer_pool)
        pthread_mutex_destroy(&ctx->buffer_pool->pool_mutex);
#endif

  av1_destroy_gpu_decoder(ctx->gpu_decoder);
  return AOM_CODEC_OK;
}

static aom_codec_err_t decoder_peek_si_internal(const uint8_t *data,
                                                size_t data_sz,
                                                aom_codec_stream_info_t *si,
                                                int *is_intra_only) {
  int intra_only_flag = 0;
  int got_sequence_header = 0;
  int found_keyframe = 0;

  if (data + data_sz <= data || data_sz < 1) return AOM_CODEC_INVALID_PARAM;

  si->w = 0;
  si->h = 0;
  si->bit_depth = 0;
  si->is_kf = 0;  // is_kf indicates whether the current packet contains a RAP

  ObuHeader obu_header;
  memset(&obu_header, 0, sizeof(obu_header));
  size_t payload_size = 0;
  size_t bytes_read = 0;
  uint8_t reduced_still_picture_hdr = 0;
  aom_codec_err_t status = aom_read_obu_header_and_size(
      data, data_sz, si->is_annexb, &obu_header, &payload_size, &bytes_read);
  if (status != AOM_CODEC_OK) return status;

  // If the first OBU is a temporal delimiter, skip over it and look at the next
  // OBU in the bitstream
  if (obu_header.type == OBU_TEMPORAL_DELIMITER) {
    // Skip any associated payload (there shouldn't be one, but just in case)
    if (data_sz < bytes_read + payload_size) return AOM_CODEC_CORRUPT_FRAME;
    data += bytes_read + payload_size;
    data_sz -= bytes_read + payload_size;

    status = aom_read_obu_header_and_size(
        data, data_sz, si->is_annexb, &obu_header, &payload_size, &bytes_read);
    if (status != AOM_CODEC_OK) return status;
  }
  while (1) {
    data += bytes_read;
    data_sz -= bytes_read;
    if (data_sz < payload_size) return AOM_CODEC_CORRUPT_FRAME;
    // Check that the selected OBU is a sequence header
    if (obu_header.type == OBU_SEQUENCE_HEADER) {
      // Sanity check on sequence header size
      if (data_sz < 2) return AOM_CODEC_CORRUPT_FRAME;
      // Read a few values from the sequence header payload. The fields in
      // front of the frame size (timing info, decoder model info, operating
      // points) have a variable length; av1_peek_sequence_header() walks them
      // as read_sequence_header_obu() does, and no further than this OBU.
      Av1SeqHeaderPeek sh;
      const int peek_status = av1_peek_sequence_header(data, payload_size, &sh);
      if (peek_status == AV1_SEQ_PEEK_UNSUPPORTED)
        return AOM_CODEC_UNSUP_BITSTREAM;
      if (peek_status != AV1_SEQ_PEEK_OK) return AOM_CODEC_CORRUPT_FRAME;
      reduced_still_picture_hdr = (uint8_t)sh.reduced_still_picture_header;

      if (aom_get_num_layers_from_operating_point_idc(
              sh.operating_point_idc0, &si->number_spatial_layers,
              &si->number_temporal_layers) != AOM_CODEC_OK) {
        return AOM_CODEC_ERROR;
      }

      si->w = sh.max_frame_width;
      si->h = sh.max_frame_height;
      si->bit_depth = sh.bit_depth;
      got_sequence_header = 1;
    } else if (obu_header.type == OBU_FRAME_HEADER ||
               obu_header.type == OBU_FRAME) {
      if (got_sequence_header && reduced_still_picture_hdr) {
        found_keyframe = 1;
        break;
      } else {
        // make sure we have enough bits to get the frame type out
        if (data_sz < 1) return AOM_CODEC_CORRUPT_FRAME;
        struct aom_read_bit_buffer rb = { data, data + data_sz, 0, NULL, NULL };
        const int show_existing_frame = aom_rb_read_bit(&rb);
        if (!show_existing_frame) {
          const FRAME_TYPE frame_type = (FRAME_TYPE)aom_rb_read_literal(&rb, 2);
          if (frame_type == KEY_FRAME) {
            found_keyframe = 1;
            break;  // Stop here as no further OBUs will change the outcome.
          }
        }
      }
    }
    // skip past any unread OBU header data
    data += payload_size;
    data_sz -= payload_size;
    if (data_sz == 0) break;  // exit if we're out of OBUs
    status = aom_read_obu_header_and_size(
        data, data_sz, si->is_annexb, &obu_header, &payload_size, &bytes_read);
    if (status != AOM_CODEC_OK) return status;
  }
  if (got_sequence_header && found_keyframe) si->is_kf = 1;
  if (is_intra_only != NULL) *is_intra_only = intra_only_flag;
  return AOM_CODEC_OK;
}

static aom_codec_err_t decoder_peek_si(const uint8_t *data, size_t data_sz,
                                       aom_codec_stream_info_t *si) {
  return decoder_peek_si_internal(data, data_sz, si, NULL);
}

static aom_codec_err_t decoder_get_si(aom_codec_alg_priv_t *ctx,
                                      aom_codec_stream_info_t *si) {
  memcpy(si, &ctx->si, sizeof(*si));

  return AOM_CODEC_OK;
}


static aom_codec_err_t update_error_state(
    aom_codec_alg_priv_t *ctx, const struct aom_internal_error_info *error) {
  if (error->error_code)
    set_error_detail(ctx, error->has_detail ? error->detail : NULL);

  return error->error_code;
}

static INLINE void check_resync(aom_codec_alg_priv_t *const ctx,
                                const AV1Decoder *const pbi) {
  // Clear resync flag if worker got a key frame or intra only frame.
  if (ctx->need_resync == 1 && pbi->need_resync == 0 &&
      frame_is_intra_only(&pbi->common))
    ctx->need_resync = 0;
}

static aom_codec_err_t decode_one(aom_codec_alg_priv_t *ctx,
                                  const uint8_t **data, size_t data_sz,
                                  void *user_priv) {
  // Determine the stream parameters. Note that we rely on peek_si to
  // validate that we have a buffer that does not wrap around the top
  // of the heap.
  if (!ctx->si.h) {
    int is_intra_only = 0;
    ctx->si.is_annexb = ctx->is_annexb;
    const aom_codec_err_t res =
        decoder_peek_si_internal(*data, data_sz, &ctx->si, &is_intra_only);
    if (res != AOM_CODEC_OK) return res;

    if (!ctx->si.is_kf && !is_intra_only) return AOM_CODEC_ERROR;
  }

  AV1Decoder * pbi = ctx->pbi;
  pbi->data = *data;
  pbi->data_size = data_sz;
  pbi->user_priv = user_priv;
  pbi->common.large_scale_tile = ctx->tile_mode;
  pbi->dec_tile_row = ctx->decode_tile_row;
  pbi->dec_tile_col = ctx->decode_tile_col;
  pbi->ext_tile_debug = ctx->ext_tile_debug;
  pbi->row_mt = ctx->row_mt;
  pbi->ext_refs = ctx->ext_refs;
  pbi->common.is_annexb = ctx->is_annexb;

//  winterface->execute(worker);
  //const uint8_t *data = frame_worker_data->data;
  pbi->data_end = pbi->data;
  int result = av1_receive_compressed_data(pbi, pbi->data_size, &pbi->data_end);

  // Update data pointer after decode.
  *data = pbi->data_end;

  if (result)
    return update_error_state(ctx, &pbi->common.error);
  check_resync(ctx, pbi);
  return AOM_CODEC_OK;
}

static aom_codec_err_t decoder_decode(aom_codec_alg_priv_t *ctx,
                                      const uint8_t *data, size_t data_sz,
                                      void *user_priv) {
  aom_codec_err_t res = AOM_CODEC_OK;

  /* Sanity checks */
  /* NULL data ptr allowed if data_sz is 0 too */
  if (data == NULL && data_sz == 0) {
    ctx->flushed = 1;
    if (ctx->gpu_decoder) {
        av1_drain_gpu_decoder(ctx->gpu_decoder);
    }
    return AOM_CODEC_OK;
  }
  if (data == NULL || data_sz == 0) return AOM_CODEC_INVALID_PARAM;

  // Reset flushed when receiving a valid frame.
  ctx->flushed = 0;

  const uint8_t *data_start = data;
  const uint8_t *data_end = data + data_sz;

  if (ctx->is_annexb) {
    // read the size of this temporal unit
    size_t length_of_size;
    uint64_t temporal_unit_size;
    if (aom_uleb_decode(data_start, data_sz, &temporal_unit_size,
                        &length_of_size) != 0) {
      return AOM_CODEC_CORRUPT_FRAME;
    }
    data_start += length_of_size;
    if (temporal_unit_size > (size_t)(data_end - data_start))
      return AOM_CODEC_CORRUPT_FRAME;
    data_end = data_start + temporal_unit_size;
  }

  // The data of this call is one temporal unit, or what is left of one: where av1_frame_is_output() stops
  // looking for the frames that follow a frame.
  ctx->pbi->temporal_unit_end = data_end;

  // Decode in serial mode.
  while (data_start < data_end) {
    uint64_t frame_size;
    if (ctx->is_annexb) {
      // read the size of this frame unit
      size_t length_of_size;
      if (aom_uleb_decode(data_start, (size_t)(data_end - data_start),
                          &frame_size, &length_of_size) != 0) {
        return AOM_CODEC_CORRUPT_FRAME;
      }
      data_start += length_of_size;
      if (frame_size > (size_t)(data_end - data_start))
        return AOM_CODEC_CORRUPT_FRAME;
    } else {
      frame_size = (uint64_t)(data_end - data_start);
    }

    res = decode_one(ctx, &data_start, (size_t)frame_size, user_priv);
    if (res != AOM_CODEC_OK) return res;

    // Allow extra zero bytes after the frame end
    while (data_start < data_end) {
      const uint8_t marker = data_start[0];
      if (marker) break;
      ++data_start;
    }
  }

  return res;
}

static aom_image_t *decoder_get_frame(aom_codec_alg_priv_t *ctx, aom_codec_iter_t *iter) 
{
    aom_image_t *img = NULL;
    // To avoid having to allocate any extra storage, treat 'iter' as
    // simply a pointer to an integer index
    if (ctx->pbi == NULL)
        return NULL;
    // NOTE(david.barker): This code does not support multiple worker threads
    // yet. We should probably move the iteration over threads into *iter
    // instead of using ctx->next_output_worker_id.
    AV1Decoder *const pbi = ctx->pbi;

    img = &ctx->img;
    if (get_output_frame(pbi, img))
        return NULL;
    return img;
}

static aom_codec_err_t decoder_set_fb_fn(
    aom_codec_alg_priv_t *ctx, aom_get_frame_buffer_cb_fn_t cb_get,
    aom_release_frame_buffer_cb_fn_t cb_release, void *cb_priv) {
  return AOM_CODEC_ERROR;
}

static aom_codec_err_t ctrl_set_reference(aom_codec_alg_priv_t *ctx,
                                          va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_copy_reference(aom_codec_alg_priv_t *ctx,
                                           va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_reference(aom_codec_alg_priv_t *ctx,
                                          va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_new_frame_image(aom_codec_alg_priv_t *ctx,
                                                va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_copy_new_frame_image(aom_codec_alg_priv_t *ctx,
                                                 va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_postproc(aom_codec_alg_priv_t *ctx,
                                         va_list args) {
  (void)ctx;
  (void)args;
  return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_dbg_options(aom_codec_alg_priv_t *ctx,
                                            va_list args) {
  (void)ctx;
  (void)args;
  return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_last_ref_updates(aom_codec_alg_priv_t *ctx,
                                                 va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_last_quantizer(aom_codec_alg_priv_t *ctx,
                                               va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_frame_corrupted(aom_codec_alg_priv_t *ctx,
                                                va_list args) {
   return AOM_CODEC_INVALID_PARAM;
}

static aom_codec_err_t ctrl_get_frame_size(aom_codec_alg_priv_t *ctx,
                                           va_list args) {
    return AOM_CODEC_INCAPABLE;

}

static aom_codec_err_t ctrl_get_frame_header_info(aom_codec_alg_priv_t *ctx,
                                                  va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_tile_data(aom_codec_alg_priv_t *ctx,
                                          va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_ext_ref_ptr(aom_codec_alg_priv_t *ctx,
                                            va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_render_size(aom_codec_alg_priv_t *ctx,
                                            va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_bit_depth(aom_codec_alg_priv_t *ctx,
                                          va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_img_format(aom_codec_alg_priv_t *ctx,
                                           va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_tile_size(aom_codec_alg_priv_t *ctx,
                                          va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_get_tile_count(aom_codec_alg_priv_t *ctx,
                                           va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_invert_tile_order(aom_codec_alg_priv_t *ctx,
                                                  va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_byte_alignment(aom_codec_alg_priv_t *ctx,
                                               va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_skip_loop_filter(aom_codec_alg_priv_t *ctx,
                                                 va_list args) {
  ctx->skip_loop_filter = va_arg(args, int);
  
  if (ctx->pbi) {
    ctx->pbi->common.skip_loop_filter = ctx->skip_loop_filter;
  }

  return AOM_CODEC_OK;
}

static aom_codec_err_t ctrl_set_skip_film_grain(aom_codec_alg_priv_t *ctx,
                                                va_list args) {
  ctx->skip_film_grain = va_arg(args, int);

  if (ctx->pbi) {
    ctx->pbi->common.skip_film_grain = ctx->skip_film_grain;
  }

  return AOM_CODEC_OK;
}

static aom_codec_err_t ctrl_get_accounting(aom_codec_alg_priv_t *ctx,
                                           va_list args) {
#if !CONFIG_ACCOUNTING
  (void)ctx;
  (void)args;
  return AOM_CODEC_INCAPABLE;
#else
  if (ctx->frame_workers) {
    AVxWorker *const worker = ctx->frame_workers;
    FrameWorkerData *const frame_worker_data = (FrameWorkerData *)worker->data1;
    AV1Decoder *pbi = frame_worker_data->pbi;
    Accounting **acct = va_arg(args, Accounting **);
    *acct = &pbi->accounting;
    return AOM_CODEC_OK;
  }
  return AOM_CODEC_ERROR;
#endif
}
static aom_codec_err_t ctrl_set_decode_tile_row(aom_codec_alg_priv_t *ctx,
                                                va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_decode_tile_col(aom_codec_alg_priv_t *ctx,
                                                va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_tile_mode(aom_codec_alg_priv_t *ctx,
                                          va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_is_annexb(aom_codec_alg_priv_t *ctx,
                                          va_list args) {
    return AOM_CODEC_INCAPABLE;
}

// The operating point of a stream with layers: the index into the operating points of the sequence header
// (0, the default, is the first one; an index the stream does not have selects 0, as in libaom). It takes
// effect at the next sequence header.
static aom_codec_err_t ctrl_set_operating_point(aom_codec_alg_priv_t *ctx,
                                                va_list args) {
  ctx->operating_point = va_arg(args, int);
  if (ctx->pbi) ctx->pbi->operating_point = ctx->operating_point;
  return AOM_CODEC_OK;
}

// 0 (the default): one frame per temporal unit, the highest spatial layer of the operating point that the
// temporal unit has (av1_frame_is_output() of obu.c). 1: every shown frame, one per spatial layer.
static aom_codec_err_t ctrl_set_output_all_layers(aom_codec_alg_priv_t *ctx,
                                                  va_list args) {
  ctx->output_all_layers = va_arg(args, int) != 0;
  if (ctx->pbi) ctx->pbi->output_all_layers = ctx->output_all_layers;
  return AOM_CODEC_OK;
}

static aom_codec_err_t ctrl_set_inspection_callback(aom_codec_alg_priv_t *ctx,
                                                    va_list args) {
#if !CONFIG_INSPECTION
  (void)ctx;
  (void)args;
  return AOM_CODEC_INCAPABLE;
#else
  aom_inspect_init *init = va_arg(args, aom_inspect_init *);
  ctx->inspect_cb = init->inspect_cb;
  ctx->inspect_ctx = init->inspect_ctx;
  return AOM_CODEC_OK;
#endif
}

static aom_codec_err_t ctrl_ext_tile_debug(aom_codec_alg_priv_t *ctx,
                                           va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_err_t ctrl_set_row_mt(aom_codec_alg_priv_t *ctx,
                                       va_list args) {
    return AOM_CODEC_INCAPABLE;
}

static aom_codec_ctrl_fn_map_t decoder_ctrl_maps[] = {
  { AV1_COPY_REFERENCE, ctrl_copy_reference },

  // Setters
  { AV1_SET_REFERENCE, ctrl_set_reference },
  { AOM_SET_POSTPROC, ctrl_set_postproc },
  { AOM_SET_DBG_COLOR_REF_FRAME, ctrl_set_dbg_options },
  { AOM_SET_DBG_COLOR_MB_MODES, ctrl_set_dbg_options },
  { AOM_SET_DBG_COLOR_B_MODES, ctrl_set_dbg_options },
  { AOM_SET_DBG_DISPLAY_MV, ctrl_set_dbg_options },
  { AV1_INVERT_TILE_DECODE_ORDER, ctrl_set_invert_tile_order },
  { AV1_SET_BYTE_ALIGNMENT, ctrl_set_byte_alignment },
  { AV1_SET_SKIP_LOOP_FILTER, ctrl_set_skip_loop_filter },
  { AV1_SET_DECODE_TILE_ROW, ctrl_set_decode_tile_row },
  { AV1_SET_DECODE_TILE_COL, ctrl_set_decode_tile_col },
  { AV1_SET_TILE_MODE, ctrl_set_tile_mode },
  { AV1D_SET_IS_ANNEXB, ctrl_set_is_annexb },
  { AV1D_SET_OPERATING_POINT, ctrl_set_operating_point },
  { AV1D_SET_OUTPUT_ALL_LAYERS, ctrl_set_output_all_layers },
  { AV1_SET_INSPECTION_CALLBACK, ctrl_set_inspection_callback },
  { AV1D_EXT_TILE_DEBUG, ctrl_ext_tile_debug },
  { AV1D_SET_ROW_MT, ctrl_set_row_mt },
  { AV1D_SET_EXT_REF_PTR, ctrl_set_ext_ref_ptr },
  { AV1D_SET_SKIP_FILM_GRAIN, ctrl_set_skip_film_grain },

  // Getters
  { AOMD_GET_FRAME_CORRUPTED, ctrl_get_frame_corrupted },
  { AOMD_GET_LAST_QUANTIZER, ctrl_get_last_quantizer },
  { AOMD_GET_LAST_REF_UPDATES, ctrl_get_last_ref_updates },
  { AV1D_GET_BIT_DEPTH, ctrl_get_bit_depth },
  { AV1D_GET_IMG_FORMAT, ctrl_get_img_format },
  { AV1D_GET_TILE_SIZE, ctrl_get_tile_size },
  { AV1D_GET_TILE_COUNT, ctrl_get_tile_count },
  { AV1D_GET_DISPLAY_SIZE, ctrl_get_render_size },
  { AV1D_GET_FRAME_SIZE, ctrl_get_frame_size },
  { AV1_GET_ACCOUNTING, ctrl_get_accounting },
  { AV1_GET_NEW_FRAME_IMAGE, ctrl_get_new_frame_image },
  { AV1_COPY_NEW_FRAME_IMAGE, ctrl_copy_new_frame_image },
  { AV1_GET_REFERENCE, ctrl_get_reference },
  { AV1D_GET_FRAME_HEADER_INFO, ctrl_get_frame_header_info },
  { AV1D_GET_TILE_DATA, ctrl_get_tile_data },

  { -1, NULL },
};

#ifndef VERSION_STRING
#define VERSION_STRING
#endif
CODEC_INTERFACE(aom_codec_av1_dx) = {
  "AOMedia Project AV1 Decoder" VERSION_STRING,
  AOM_CODEC_INTERNAL_ABI_VERSION,
  AOM_CODEC_CAP_DECODER |
      AOM_CODEC_CAP_EXTERNAL_FRAME_BUFFER,  // aom_codec_caps_t
  decoder_init,                             // aom_codec_init_fn_t
  decoder_destroy,                          // aom_codec_destroy_fn_t
  decoder_ctrl_maps,                        // aom_codec_ctrl_fn_map_t
  {
      // NOLINT
      decoder_peek_si,    // aom_codec_peek_si_fn_t
      decoder_get_si,     // aom_codec_get_si_fn_t
      decoder_decode,     // aom_codec_decode_fn_t
      decoder_get_frame,  // aom_codec_get_frame_fn_t
      decoder_set_fb_fn,  // aom_codec_set_fb_fn_t
  },
  {
      // NOLINT
      0,
      NULL,  // aom_codec_enc_cfg_map_t
      NULL,  // aom_codec_encode_fn_t
      NULL,  // aom_codec_get_cx_data_fn_t
      NULL,  // aom_codec_enc_config_set_fn_t
      NULL,  // aom_codec_get_global_headers_fn_t
      NULL,  // aom_codec_get_preview_frame_fn_t
      NULL   // aom_codec_enc_mr_get_mem_loc_fn_t
  }
};

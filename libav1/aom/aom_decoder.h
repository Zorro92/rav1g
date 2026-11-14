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
#ifndef AOM_AOM_AOM_DECODER_H_
#define AOM_AOM_AOM_DECODER_H_

/*!\defgroup decoder Decoder Algorithm Interface
 * \ingroup codec
 * This abstraction allows applications using this decoder to easily support
 * multiple video formats with minimal code duplication. This section describes
 * the interface common to all decoders.
 * @{
 */

/*!\file
 * \brief Describes the decoder algorithm interface to applications.
 *
 * This file describes the interface between an application and a
 * video decoder algorithm.
 *
 */
#ifdef __cplusplus
extern "C" {
#endif

#include "aom/aom_codec.h"
#include "aom/aom_frame_buffer.h"

/*!\brief Current ABI version number
 *
 * \internal
 * If this file is altered in any way that changes the ABI, this value
 * must be bumped.  Examples include, but are not limited to, changing
 * types, removing or reassigning enums, adding/removing/rearranging
 * fields to structures
 */
#define AOM_DECODER_ABI_VERSION \
  (3 + AOM_CODEC_ABI_VERSION) /**<\hideinitializer*/

/*! \brief Decoder capabilities bitfield
 *
 *  Each decoder advertises the capabilities it supports as part of its
 *  ::aom_codec_iface_t interface structure. Capabilities are extra interfaces
 *  or functionality, and are not required to be supported by a decoder.
 *
 *  The available flags are specified by AOM_CODEC_CAP_* defines.
 */
#define AOM_CODEC_CAP_PUT_SLICE 0x10000 /**< Will issue put_slice callbacks */
#define AOM_CODEC_CAP_PUT_FRAME 0x20000 /**< Will issue put_frame callbacks */
#define AOM_CODEC_CAP_POSTPROC 0x40000  /**< Can postprocess decoded frame */

/*! \brief Initialization-time Feature Enabling
 *
 *  Certain codec features must be known at initialization time, to allow for
 *  proper memory allocation.
 *
 *  The available flags are specified by AOM_CODEC_USE_* defines.
 */
/*!brief Can support external frame buffers */
#define AOM_CODEC_CAP_EXTERNAL_FRAME_BUFFER 0x200000

#define AOM_CODEC_USE_POSTPROC 0x10000 /**< Postprocess decoded frame */

/*!\brief Stream properties
 *
 * This structure is used to query or set properties of the decoded
 * stream.
 */
typedef struct aom_codec_stream_info {
  unsigned int w;                      /**< Width (or 0 for unknown/default) */
  unsigned int h;                      /**< Height (or 0 for unknown/default) */
  unsigned int is_kf;                  /**< Current frame is a keyframe */
  unsigned int number_spatial_layers;  /**< Number of spatial layers */
  unsigned int number_temporal_layers; /**< Number of temporal layers */
  unsigned int is_annexb;              /**< Is Bitstream in Annex-B format */
  unsigned int bit_depth;              /**< Bit depth of the sequence header a
                                            peek has read: 8, 10 or 12 (0 when
                                            there was none) */
  /* The colour description of that sequence header (color_config), as it is
     coded: the three code points are 2 (unspecified) when the header has no
     description. */
  unsigned int monochrome;
  unsigned int color_description_present;
  unsigned int color_primaries;
  unsigned int transfer_characteristics;
  unsigned int matrix_coefficients;
  unsigned int color_range;            /**< 1: full range */
  unsigned int chroma_sample_position;
} aom_codec_stream_info_t;

/* REQUIRED FUNCTIONS
 *
 * The following functions are required to be implemented for all decoders.
 * They represent the base case functionality expected of all decoders.
 */

typedef struct av1_out_buffers_cb
{
    av1_get_decoded_buffer_cb_fn_t get_out_buffer_cb; /**< Get external output buffer */
    av1_release_decoded_buffer_cb_fn_t release_out_buffer_cb; /**< Release external output buffer */
    aom_notify_frame_ready_cb_fn_t notify_frame_ready_cb;
    void* out_buffers_priv; /**< private pointer for functions above */
} av1_out_buffers_cb_t;

/*!\brief Bytes the decoder may still allocate, see
 * aom_codec_dec_cfg_t::memory_available_cb. Negative: not known. */
typedef int64_t (*av1_memory_available_cb_fn_t)(void *priv);

/*!\brief What a decoder creation guarded by memory_available_cb must leave
 * free: headroom for what decoding takes outside the decoder's buffers and
 * the output images (command allocators, the application's stream buffers).
 *
 * Nothing is added for the creation itself. Measured in a console app (limit
 * 1024 MiB): the pipeline states of av1_create_pipeline_cache_handle take
 * about 84 MiB of the app's memory, once per process. The first decoder
 * creation waits for them before it asks the callback, so they are in what
 * the callback reports (77 MiB in use before that creation, 160.8 MiB when
 * the callback was asked). Creating the 4096x2176 10-bit decoder with two
 * slots, 681.9 MiB of device buffers, then raised the app's memory to 812.7
 * MiB: by about 652 MiB, less than its buffers. The margin used to be 84 MiB,
 * from reading the pipeline states as a cost of the creation (51 MiB) and
 * adding 32 MiB; with a 4-image 4K output pool that refused the default
 * decoder in a fresh launch by 3.6 MiB. */
#define AV1_MEMORY_GUARD_MARGIN ((size_t)48 << 20)
/*!\brief A creation that does not fit is not refused at once: the system
 * returns the memory of a decoder that was just destroyed some time after
 * the release, so the creation asks the callback again every 20 ms for as
 * long as the answer keeps growing (by 1 MiB within
 * AV1_MEMORY_GUARD_SETTLE_MS), at most AV1_MEMORY_GUARD_WAIT_MS. */
#define AV1_MEMORY_GUARD_WAIT_MS 2000
#define AV1_MEMORY_GUARD_SETTLE_MS 300
/*!\brief The same for one buffer created after the creation (a slot's
 * palette buffer). */
#define AV1_MEMORY_GUARD_MARGIN_BUFFER ((size_t)16 << 20)

/*!\brief aom_codec_dec_cfg_t::low_memory. The decoded output is the same
 * with every combination.
 *
 * 0 selects the default, AV1_LOW_MEMORY_DEFAULT: the shared back buffer and
 * nothing else. Any other value is taken as it is: the sum of the parts
 * wanted, or AV1_LOW_MEMORY_NONE for none of them (every frame takes its back
 * buffer from the frame buffer pool, as the decoder did before the reductions
 * existed).
 *
 * AV1_LOW_MEMORY_SHARED_BACK_BUFFER: the frame that is filtered after its
 * reconstruction (CDEF, loop restoration, superres) is reconstructed into one
 * buffer shared by all frames instead of a buffer of its own from the frame
 * buffer pool, and the pool has two buffers less. Covers every stream.
 *
 * AV1_LOW_MEMORY_MODE_INFO: a slot's share of the mode info pool holds a
 * quarter of the worst case instead of half. A frame takes a second slot when
 * a tile has more blocks than a quarter of its 4x4 units; a tile with more
 * blocks than half of its 4x4 units is a decode error.
 *
 * AV1_LOW_MEMORY_COEFS: a slot's share of the coefficient buffer holds 12/24
 * of the worst case instead of 17/24. A frame takes a second slot earlier; a
 * tile whose coefficients come within one superblock of the worst case (every
 * coefficient of every transform block coded) is a decode error.
 *
 * With AV1_PARSER_RAV1D the decoder has no shares: the frames in flight take
 * their mode info records, coefficients and transform block records in
 * pieces from pools that hold the worst case of one frame
 * (docs/phase2-rav1d.md, step 5). There AV1_LOW_MEMORY_MODE_INFO halves the
 * mode info pool and AV1_LOW_MEMORY_COEFS sizes the coefficient pool for 24
 * values a 4x4 unit instead of 30; a frame that does not fit such a pool is
 * a decode error. */
#define AV1_LOW_MEMORY_SHARED_BACK_BUFFER 1
#define AV1_LOW_MEMORY_MODE_INFO 2
#define AV1_LOW_MEMORY_COEFS 4
#define AV1_LOW_MEMORY_ALL 7
#define AV1_LOW_MEMORY_NONE 8
#define AV1_LOW_MEMORY_DEFAULT AV1_LOW_MEMORY_SHARED_BACK_BUFFER

/* aom_codec_dec_cfg_t::parser. */
#define AV1_PARSER_LIBAOM 0
#define AV1_PARSER_RAV1D 1

/* aom_codec_dec_cfg_t::parser_options, values to add up: ways of doing the
 * parse of AV1_PARSER_RAV1D that give the same frames (docs/phase2-rav1d.md,
 * "Where the time goes"). The library's settings have the first two on
 * (rav1g::ParserOptions::default()); the value 1 was a switch that is gone. */
#define AV1_PARSER_OPTION_NO_LF_MASKS 2       /* rav1d leaves out the masks of its own loop filter */
#define AV1_PARSER_OPTION_TILE_KEEPS_THREAD 4 /* a tile stays on its thread between its superblock rows */
#define AV1_PARSER_OPTION_TIME_CALLS 8        /* the calls of the sink are timed (AV1_PERF_CPU_SINK_CALLS) */
#define AV1_PARSER_OPTION_LATE_LISTS 16       /* the block lists of a tile at its end, not superblock by superblock */
#define AV1_PARSER_OPTION_CLEAR_GRID 32       /* the mode info grid of a slot is cleared for every frame */
#define AV1_PARSER_OPTION_TEST_PANIC 64       /* test only: the parse of the third frame panics */
#define AV1_PARSER_OPTION_TEST_POOL_WAIT 128  /* test only: a frame finds the pools empty while an older frame holds a piece */

/* aom_codec_dec_cfg_t::frames_in_parse: the most the decoder takes. */
#define AV1_FRAMES_IN_PARSE_MAX 4

/*!\brief Initialization Configurations
 *
 * This structure is used to pass init time configuration options to the
 * decoder.
 */
typedef struct aom_codec_dec_cfg {
  /* Threads that parse: the tiles of a frame and, with frames_in_parse above
   * 1, the frames. 0 selects the default of AV1_PARSER_RAV1D by the size the
   * decoder is created for: 1 up to 2560x1440, 2 above; for
   * AV1_PARSER_LIBAOM 0 is 1. At most 8. */
  unsigned int threads;
  unsigned int width;       /**< Width */
  unsigned int height;       /**< Height */
  unsigned int bitdepth;
  unsigned int allow_lowbitdepth; /**< Allow use of low-bitdepth coding path */
  cfg_options_t cfg;              /**< Options defined per config attributes */
  void * host_memory;
  size_t host_size;
  av1_out_buffers_cb_t out_buffers_cb; /**< External output buffers functions*/
  void* dx12device;
  void* dx12command_queue;
  void* dxPsos;
  int tryHDR10x3;
  /* Pipeline slots: frames that can be between the start of their CPU decode
   * and the end of their GPU work at the same time. 0 selects the default
   * (one more than frames in parse: 2, or 3 with two frames in parse); other
   * values are clamped to 2..4. Each slot has its own buffers, so
   * the value must be the same for av1_codec_query_memory_requirements and
   * aom_codec_dec_init. */
  int frame_slots;
  /* Non-zero creates the buffers of the optional intra path (intra_v2.hlsl,
   * av1_set_intra_v2). Without them the decoder keeps the original intra
   * shaders whatever av1_set_intra_v2 is set to. Must be the same for
   * av1_codec_query_memory_requirements and aom_codec_dec_init, like every
   * field that changes what is allocated (frame_slots, low_memory). */
  int intra_v2;
  /* Non-zero makes room in the block maps for the lists of the optional
   * inter path (av1_set_inter_fast: blocks listed by the fraction of their
   * motion vectors), 1984 bytes per tile the size can have (one per 64x64)
   * and pipeline slot: 1.0 MiB per slot at 1080p. Without it the
   * decoder keeps the original inter path whatever av1_set_inter_fast is set
   * to. Must be the same for av1_codec_query_memory_requirements and
   * aom_codec_dec_init. */
  int inter_fast;
  /* Memory reductions: a combination of the AV1_LOW_MEMORY_* values above.
   * 0 selects the default (AV1_LOW_MEMORY_DEFAULT, the shared back buffer);
   * AV1_LOW_MEMORY_NONE switches all of them off.
   *
   * What a zero-initialised configuration gets, next to the fields the
   * application has to fill (width, height, bitdepth, host memory, device,
   * pipeline cache, output buffer callbacks): two pipeline slots, the shared
   * back buffer (10 frame buffers in the pool), no intra v2 buffers, no
   * budget guard; CDEF runs through cdef_filter_v2.hlsl and intra prediction
   * through the original shaders (process-wide switches in dx/av1_perf.h).
   * The decoder is created for the width, height and bit depth given here;
   * aom_codec_peek_stream_info gives those of a stream. */
  int low_memory;
  /* The parser: AV1_PARSER_LIBAOM (0), the entropy decoder of libaom, or
   * AV1_PARSER_RAV1D, the parse pass of rav1d (docs/phase2-rav1d.md), which
   * the library's settings choose unless they are told otherwise
   * (rav1g::Settings::parser). Both give the same frames; they differ in
   * speed and in what a damaged stream does (the document says how). */
  int parser;
  /* AV1_PARSER_OPTION_* values added up, for AV1_PARSER_RAV1D; 0 for none. */
  int parser_options;
  /* Frames whose parse may run at the same time, for AV1_PARSER_RAV1D
   * (docs/phase2-rav1d.md, "Frames in parallel"): 1 to
   * AV1_FRAMES_IN_PARSE_MAX, and never more than the threads. With 1 a
   * decode call returns when the frames of its data are parsed, recorded and
   * submitted. With more it returns when their parse has begun: the frames
   * are parsed on the decoder's threads next to those of the next calls, and
   * recorded and submitted in decode order by a thread of the decoder; the
   * output is the same, in the same order. Then
   *  - the error of a frame comes back from a later decode call, or from the
   *    drain (aom_codec_decode with no data), one error a call;
   *  - the callback for the output image is called on a thread of the
   *    decoder, not inside the decode call.
   * 0 selects the default: 2 when the decoder is created for frames above
   * 2560x1440 with at least 2 threads and 3 or more pipeline slots, else 1.
   * A frame in parse holds a pipeline slot, so more frames in parse want
   * more slots: with frame_slots 0 the decoder has 1 slot more than frames
   * in parse, at most 4. If the budget guard (memory_available_cb) finds
   * that the decoder with those slots does not fit, the decoder is created
   * with one frame in parse and the slots of that (frame_slots, or 2), and
   * says so in a "MEMORY frames in parse" log line: asking for frames in
   * parse never makes a creation fail that would work without.
   * av1_decoder_frames_in_parse says what a decoder has. */
  int frames_in_parse;
  /* Memory budget guard, optional. On a console an application that goes over
   * its memory limit loses its D3D12 device for the rest of the process, so
   * the decoder asks before it allocates: at the creation, and when it
   * creates a palette buffer on first use. The callback returns the bytes the
   * decoder may still take: what the application has left of its limit, less
   * what the application is going to allocate itself (output images that do
   * not exist yet), or a negative value when that is not known. The creation
   * is refused when its device buffers would leave less than
   * AV1_MEMORY_GUARD_MARGIN, a palette buffer when it would leave less than
   * AV1_MEMORY_GUARD_MARGIN_BUFFER; both say so in a "MEMORY REFUSED" log
   * line. A refused creation fails, a frame without its palette buffer is a
   * decode error; nothing was allocated and the device is intact. Without a
   * callback nothing is checked. The callback is called on the thread that
   * creates the decoder, on the thread that calls aom_codec_decode, and on
   * the thread that destroys the decoder (aom_codec_destroy logs what the
   * application has left after each step of the release, "MEMORY destroy"),
   * so memory_available_priv must be valid until aom_codec_destroy returns. */
  av1_memory_available_cb_fn_t memory_available_cb;
  void *memory_available_priv;
  /* Filled by av1_codec_query_memory_requirements next to host_size: the
   * bytes of the D3D12 buffers the creation allocates, and of the buffers
   * that are only created when a stream needs them (one palette buffer per
   * slot, on the first frame of the slot that allows screen content tools). */
  size_t device_size;
  size_t device_size_deferred;
} aom_codec_dec_cfg_t;            /**< alias for struct aom_codec_dec_cfg */

/*!\brief Initialize a decoder instance
 *
 * Initializes a decoder context using the given interface. Applications
 * should call the aom_codec_dec_init convenience macro instead of this
 * function directly, to ensure that the ABI version number parameter
 * is properly initialized.
 *
 * If the library was configured with --disable-multithread, this call
 * is not thread safe and should be guarded with a lock if being used
 * in a multithreaded context.
 *
 * \param[in]    ctx     Pointer to this instance's context.
 * \param[in]    iface   Pointer to the algorithm interface to use.
 * \param[in]    cfg     Configuration to use, if known. May be NULL.
 * \param[in]    flags   Bitfield of AOM_CODEC_USE_* flags
 * \param[in]    ver     ABI version number. Must be set to
 *                       AOM_DECODER_ABI_VERSION
 * \retval #AOM_CODEC_OK
 *     The decoder algorithm initialized.
 * \retval #AOM_CODEC_MEM_ERROR
 *     Memory allocation failed.
 */
aom_codec_err_t aom_codec_dec_init_ver(aom_codec_ctx_t *ctx,
                                       aom_codec_iface_t *iface,
                                       const aom_codec_dec_cfg_t *cfg,
                                       aom_codec_flags_t flags, int ver);

/*!\brief Convenience macro for aom_codec_dec_init_ver()
 *
 * Ensures the ABI version parameter is properly set.
 */
#define aom_codec_dec_init(ctx, iface, cfg, flags) \
  aom_codec_dec_init_ver(ctx, iface, cfg, flags, AOM_DECODER_ABI_VERSION)

/*!\brief Parse stream info from a buffer
 *
 * Performs high level parsing of the bitstream. Construction of a decoder
 * context is not necessary. Can be used to determine if the bitstream is
 * of the proper format, and to extract information from the stream.
 *
 * \param[in]      iface   Pointer to the algorithm interface
 * \param[in]      data    Pointer to a block of data to parse
 * \param[in]      data_sz Size of the data buffer
 * \param[in,out]  si      Pointer to stream info to update. The is_annexb
 *                         member \ref MUST be properly initialized. This
 *                         function sets the rest of the members.
 *
 * \retval #AOM_CODEC_OK
 *     Bitstream is parsable and stream information updated.
 * \retval #AOM_CODEC_INVALID_PARAM
 *     One of the arguments is invalid, for example a NULL pointer.
 * \retval #AOM_CODEC_UNSUP_BITSTREAM
 *     The decoder didn't recognize the coded data, or the
 *     buffer was too short.
 */
aom_codec_err_t aom_codec_peek_stream_info(aom_codec_iface_t *iface,
                                           const uint8_t *data, size_t data_sz,
                                           aom_codec_stream_info_t *si);

/*!\brief Return information about the current stream.
 *
 * Returns information about the stream that has been parsed during decoding.
 *
 * \param[in]      ctx     Pointer to this instance's context
 * \param[in,out]  si      Pointer to stream info to update.
 *
 * \retval #AOM_CODEC_OK
 *     Bitstream is parsable and stream information updated.
 * \retval #AOM_CODEC_INVALID_PARAM
 *     One of the arguments is invalid, for example a NULL pointer.
 * \retval #AOM_CODEC_UNSUP_BITSTREAM
 *     The decoder couldn't parse the submitted data.
 */
aom_codec_err_t aom_codec_get_stream_info(aom_codec_ctx_t *ctx,
                                          aom_codec_stream_info_t *si);

/*!\brief Decode data
 *
 * Processes a buffer of coded data. If the processing results in a new
 * decoded frame becoming available, PUT_SLICE and PUT_FRAME events may be
 * generated, as appropriate. Encoded data \ref MUST be passed in DTS (decode
 * time stamp) order. Frames produced will always be in PTS (presentation
 * time stamp) order.
 *
 * \param[in] ctx          Pointer to this instance's context
 * \param[in] data         Pointer to this block of new coded data. If
 *                         NULL, a AOM_CODEC_CB_PUT_FRAME event is posted
 *                         for the previously decoded frame.
 * \param[in] data_sz      Size of the coded data, in bytes.
 * \param[in] user_priv    Application specific data to associate with
 *                         this frame.
 *
 * \return Returns #AOM_CODEC_OK if the coded data was processed completely
 *         and future pictures can be decoded without error. Otherwise,
 *         see the descriptions of the other error codes in ::aom_codec_err_t
 *         for recoverability capabilities.
 */
aom_codec_err_t aom_codec_decode(aom_codec_ctx_t *ctx, const uint8_t *data,
                                 size_t data_sz, void *user_priv);

/*!\brief Decoded frames iterator
 *
 * Iterates over a list of the frames available for display. The iterator
 * storage should be initialized to NULL to start the iteration. Iteration is
 * complete when this function returns NULL.
 *
 * The list of available frames becomes valid upon completion of the
 * aom_codec_decode call, and remains valid until the next call to
 * aom_codec_decode.
 *
 * \param[in]     ctx      Pointer to this instance's context
 * \param[in,out] iter     Iterator storage, initialized to NULL
 *
 * \return Returns a pointer to an image, if one is ready for display. Frames
 *         produced will always be in PTS (presentation time stamp) order.
 */
aom_image_t *aom_codec_get_frame(aom_codec_ctx_t *ctx, aom_codec_iter_t *iter);

/*!\defgroup cap_put_frame Frame-Based Decoding Functions
 *
 * The following functions are required to be implemented for all decoders
 * that advertise the AOM_CODEC_CAP_PUT_FRAME capability. Calling these
 * functions
 * for codecs that don't advertise this capability will result in an error
 * code being returned, usually AOM_CODEC_ERROR
 * @{
 */

/*!\brief put frame callback prototype
 *
 * This callback is invoked by the decoder to notify the application of
 * the availability of decoded image data.
 */
typedef void (*aom_codec_put_frame_cb_fn_t)(void *user_priv,
                                            const aom_image_t *img);

/*!\brief Register for notification of frame completion.
 *
 * Registers a given function to be called when a decoded frame is
 * available.
 *
 * \param[in] ctx          Pointer to this instance's context
 * \param[in] cb           Pointer to the callback function
 * \param[in] user_priv    User's private data
 *
 * \retval #AOM_CODEC_OK
 *     Callback successfully registered.
 * \retval #AOM_CODEC_ERROR
 *     Decoder context not initialized, or algorithm not capable of
 *     posting slice completion.
 */
aom_codec_err_t aom_codec_register_put_frame_cb(aom_codec_ctx_t *ctx,
                                                aom_codec_put_frame_cb_fn_t cb,
                                                void *user_priv);

/*!@} - end defgroup cap_put_frame */

/*!\defgroup cap_put_slice Slice-Based Decoding Functions
 *
 * The following functions are required to be implemented for all decoders
 * that advertise the AOM_CODEC_CAP_PUT_SLICE capability. Calling these
 * functions
 * for codecs that don't advertise this capability will result in an error
 * code being returned, usually AOM_CODEC_ERROR
 * @{
 */

/*!\brief put slice callback prototype
 *
 * This callback is invoked by the decoder to notify the application of
 * the availability of partially decoded image data. The
 */
typedef void (*aom_codec_put_slice_cb_fn_t)(void *user_priv,
                                            const aom_image_t *img,
                                            const aom_image_rect_t *valid,
                                            const aom_image_rect_t *update);

/*!\brief Register for notification of slice completion.
 *
 * Registers a given function to be called when a decoded slice is
 * available.
 *
 * \param[in] ctx          Pointer to this instance's context
 * \param[in] cb           Pointer to the callback function
 * \param[in] user_priv    User's private data
 *
 * \retval #AOM_CODEC_OK
 *     Callback successfully registered.
 * \retval #AOM_CODEC_ERROR
 *     Decoder context not initialized, or algorithm not capable of
 *     posting slice completion.
 */
aom_codec_err_t aom_codec_register_put_slice_cb(aom_codec_ctx_t *ctx,
                                                aom_codec_put_slice_cb_fn_t cb,
                                                void *user_priv);

/*!@} - end defgroup cap_put_slice*/

/*!\defgroup cap_external_frame_buffer External Frame Buffer Functions
 *
 * The following section is required to be implemented for all decoders
 * that advertise the AOM_CODEC_CAP_EXTERNAL_FRAME_BUFFER capability.
 * Calling this function for codecs that don't advertise this capability
 * will result in an error code being returned, usually AOM_CODEC_ERROR.
 *
 * \note
 * Currently this only works with AV1.
 * @{
 */

/*!\brief Pass in external frame buffers for the decoder to use.
 *
 * Registers functions to be called when libaom needs a frame buffer
 * to decode the current frame and a function to be called when libaom does
 * not internally reference the frame buffer. This set function must
 * be called before the first call to decode or libaom will assume the
 * default behavior of allocating frame buffers internally.
 *
 * \param[in] ctx          Pointer to this instance's context
 * \param[in] cb_get       Pointer to the get callback function
 * \param[in] cb_release   Pointer to the release callback function
 * \param[in] cb_priv      Callback's private data
 *
 * \retval #AOM_CODEC_OK
 *     External frame buffers will be used by libaom.
 * \retval #AOM_CODEC_INVALID_PARAM
 *     One or more of the callbacks were NULL.
 * \retval #AOM_CODEC_ERROR
 *     Decoder context not initialized, or algorithm not capable of
 *     using external frame buffers.
 *
 * \note
 * When decoding AV1, the application may be required to pass in at least
 * #AOM_MAXIMUM_WORK_BUFFERS external frame
 * buffers.
 */
aom_codec_err_t aom_codec_set_frame_buffer_functions(
    aom_codec_ctx_t *ctx, aom_get_frame_buffer_cb_fn_t cb_get,
    aom_release_frame_buffer_cb_fn_t cb_release, void *cb_priv);

/*!@} - end defgroup cap_external_frame_buffer */
aom_codec_err_t av1_codec_query_memory_requirements(aom_codec_dec_cfg_t *cfg);
/*!@} - end defgroup decoder*/
/*!\brief Precreate compute shaders.
 *
 */
void* av1_create_pipeline_cache_handle(void* d3d12device, int threads);
void av1_destroy_pipeline_cache_handle(void* handle);
/*!\brief Wait until the pipeline states of the handle are created.
 *
 * With threads the creation above returns while they are still being
 * created, and the first decoder creation waits for them. An application that
 * measures its memory (aom_codec_dec_cfg_t::memory_available_cb, a log) waits
 * here first: the pipeline states are memory of the process, not of a
 * decoder, and stay until av1_destroy_pipeline_cache_handle. Returns 0 when
 * all were created. Not to be called while a decoder is being created with
 * the handle on another thread. */
int av1_wait_pipeline_cache_handle(void* handle);
#ifdef __cplusplus
}
#endif
#endif  // AOM_AOM_AOM_DECODER_H_

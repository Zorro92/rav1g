/* SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/* rav1g: an AV1 decoder whose reconstruction runs in D3D12 compute shaders. The interface for C.
 *
 * Written by cbindgen from crates/rav1g/src/capi.rs (cargo run -p rav1g-header). Do not edit.
 *
 * The order of use:
 *   1. rav1g_device_create on the application's D3D12 device and queue (or without, for a device of the
 *      decoder's own); rav1g_device_wait_pipelines.
 *   2. rav1g_peek_stream_info on the temporal units of the stream until one starts with a key frame: the
 *      frame size and bit depth of the sequence.
 *   3. rav1g_settings_default, the size and bit depth of the sequence, the output callbacks;
 *      rav1g_decoder_create.
 *   4. rav1g_decoder_decode per temporal unit. The decoder takes an image from the output for every frame
 *      that will be shown (get_image) and hands the frame over when the GPU has written it (frame).
 *      RAV1G_STATUS_NEEDS_RECONFIGURE: the stream went on with a sequence the decoder does not fit.
 *   5. rav1g_decoder_drain, rav1g_decoder_destroy, rav1g_device_destroy.
 *
 * A decoder is used by one thread at a time. The callbacks are called from the decoder's threads.
 */

#ifndef RAV1G_H
#define RAV1G_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* The records of the timing run and of the stage dumps (rav1g_set_perf_callback, rav1g_set_stage_dump)
 * are defined in dx/av1_perf.h of the decoder's sources. */
#ifndef AV1_PERF_H_
typedef struct av1_perf_frame av1_perf_frame;
typedef struct av1_stage_dump av1_stage_dump;
#endif

/*
 The shared back buffer: one back buffer for all pipeline slots and a pool of 10 frame buffers. The
 default of `rav1g_settings_default`.
 */
#define RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER 1

/*
 The mode info of a frame in shares that a pipeline slot takes as it needs them.
 */
#define RAV1G_LOW_MEMORY_MODE_INFO 2

/*
 The coefficients of a frame in shares, likewise.
 */
#define RAV1G_LOW_MEMORY_COEFS 4

/*
 All memory reductions.
 */
#define RAV1G_LOW_MEMORY_ALL 7

/*
 `Rav1gSettings::parser`: the entropy decoder of libaom reads the bitstream. It stays until the last
 step of phase 2 (docs/phase2-rav1d.md).
 */
#define RAV1G_PARSER_LIBAOM 0

/*
 `Rav1gSettings::parser`: the parse pass of rav1d reads the bitstream. The default of
 `rav1g_settings_default`, and what every value other than `RAV1G_PARSER_LIBAOM` selects. The same
 frames; the speed and what a damaged stream does differ (docs/phase2-rav1d.md).
 */
#define RAV1G_PARSER_RAV1D 1

/*
 `Rav1gSettings::parser_options`, values to add up: ways of doing the parse of `RAV1G_PARSER_RAV1D`
 that give the same frames (docs/phase2-rav1d.md, "Where the time goes"). The value 1 was a switch
 that is gone and means nothing. This one: rav1d leaves out the masks and levels of its own loop
 filter, which nothing here reads. Part of `RAV1G_PARSER_OPTIONS_DEFAULT`.
 */
#define RAV1G_PARSER_OPTION_NO_LF_MASKS 2

/*
 A tile stays on the thread it is on between its superblock rows. Part of
 `RAV1G_PARSER_OPTIONS_DEFAULT`.
 */
#define RAV1G_PARSER_OPTION_TILE_KEEPS_THREAD 4

/*
 The calls of the sink are timed, for a timing run (`rav1g_set_perf_callback`).
 */
#define RAV1G_PARSER_OPTION_TIME_CALLS 8

/*
 The `Rav1gSettings::parser_options` of `rav1g_settings_default`: `RAV1G_PARSER_OPTION_NO_LF_MASKS`
 and `RAV1G_PARSER_OPTION_TILE_KEEPS_THREAD`. An application that wants the timing as well adds
 `RAV1G_PARSER_OPTION_TIME_CALLS` to it; 0 is rav1d's parse without either.
 */
#define RAV1G_PARSER_OPTIONS_DEFAULT 6

/*
 `rav1g_set_inter_fast`: the fast inter path is on.
 */
#define RAV1G_INTER_FAST_ON 1

/*
 With `RAV1G_INTER_FAST_ON`: the blocks with one reference are listed and predicted by the original
 shader.
 */
#define RAV1G_INTER_FAST_SINGLE_ORIGINAL 2

/*
 With `RAV1G_INTER_FAST_ON`: the same for the averaged compound blocks.
 */
#define RAV1G_INTER_FAST_COMPOUND_ORIGINAL 4

/*
 The lists of the fast inter path with the original shaders on all of them.
 */
#define RAV1G_INTER_FAST_LISTS_ONLY 7

/*
 `rav1g_set_poison_buffers`: the mode info pool.
 */
#define RAV1G_POISON_MODE_INFO 1

/*
 `rav1g_set_poison_buffers`: every buffer the CPU writes for the shaders, and the constants of every
 stage in every frame.
 */
#define RAV1G_POISON_BUFFERS 2

/*
 The budget guard refuses a decoder whose device buffers would leave the application less than this
 many bytes (`Rav1gSettings::memory_available`).
 */
#define RAV1G_MEMORY_GUARD_MARGIN 50331648

/*
 How a call ended.
 */
typedef enum Rav1gStatus {
  RAV1G_STATUS_OK = 0,
  /*
   An error without a kind of its own; also a device or a pipeline state that could not be created.
   */
  RAV1G_STATUS_ERROR = 1,
  /*
   Memory could not be had, or the budget guard refused the creation.
   */
  RAV1G_STATUS_MEMORY = 2,
  RAV1G_STATUS_ABI_MISMATCH = 3,
  RAV1G_STATUS_INCAPABLE = 4,
  /*
   The stream is not one the decoder reads.
   */
  RAV1G_STATUS_UNSUPPORTED_BITSTREAM = 5,
  /*
   The stream uses something the decoder does not have.
   */
  RAV1G_STATUS_UNSUPPORTED_FEATURE = 6,
  /*
   The data of a frame is damaged or cut.
   */
  RAV1G_STATUS_CORRUPT_FRAME = 7,
  RAV1G_STATUS_INVALID_PARAM = 8,
  /*
   A new sequence whose frames are larger, or have more bits a sample, than the decoder was created
   for. Nothing of the temporal unit was decoded: drain and destroy the decoder, create one for the
   new sequence (`rav1g_peek_stream_info` on the temporal unit), pass the temporal unit again.
   */
  RAV1G_STATUS_NEEDS_RECONFIGURE = 10,
} Rav1gStatus;

/*
 The samples of an output image.
 */
typedef enum Rav1gImageFormat {
  /*
   One byte a sample; textures of R8.
   */
  RAV1G_IMAGE_FORMAT_BITS8 = 0,
  /*
   Two bytes a sample, the 10 bits in the low bits; textures of R16.
   */
  RAV1G_IMAGE_FORMAT_BITS10 = 1,
  /*
   Three samples of 10 bits packed into 32 bits (`Rav1gSettings::packed_10x3`); textures of
   R10G10B10A2 with a third of the width.
   */
  RAV1G_IMAGE_FORMAT_PACKED10X3 = 2,
} Rav1gImageFormat;

/*
 A decoder.
 */
typedef struct Rav1gDecoder Rav1gDecoder;

/*
 A device for decoders: a D3D12 device, the direct queue the decoders submit to, and the decoder's
 compute pipeline states on that device.
 */
typedef struct Rav1gDevice Rav1gDevice;

/*
 The parser of phase 2 on a stream, without a decoder or a device: rav1d's parse pass, with what it
 reads from the stream folded into one number (docs/phase2-rav1d.md). For tests: the number of a
 stream is the same on every platform and for any number of threads.
 */
typedef struct Rav1gParseDigest Rav1gParseDigest;

/*
 What the sequence header of a temporal unit says about the stream.
 */
typedef struct Rav1gStreamInfo {
  /*
   Largest frame width and height of the sequence.
   */
  uint32_t width;
  uint32_t height;
  /*
   Bits a sample: 8, 10 or 12; 0 when the temporal unit has no sequence header.
   */
  uint32_t bit_depth;
  /*
   The temporal unit starts with a key frame.
   */
  bool is_key_frame;
} Rav1gStreamInfo;

/*
 What the decoder needs an output image for: a frame that is going to be output.
 */
typedef struct Rav1gImageRequest {
  /*
   Bytes of a buffer that takes the three planes of the frame.
   */
  size_t buffer_size;
  /*
   Luma samples of a row, and rows, of the frame.
   */
  uint32_t width;
  uint32_t height;
  enum Rav1gImageFormat format;
} Rav1gImageRequest;

/*
 An output image of the application: where the decoder's GPU writes a frame. A buffer that takes the
 three planes one behind the other (to read the frame on the CPU), one texture per plane in the copy
 destination state (to use it on the GPU), or both. The decoder holds a reference of each resource from
 `Rav1gOutput::get_image` until it has handed the image back (`Rav1gOutput::frame`,
 `Rav1gOutput::unused`).
 */
typedef struct Rav1gImage {
  /*
   `ID3D12Resource *` of the buffer, or null.
   */
  void *d3d12_buffer;
  /*
   The mapping of the buffer, or null. Valid for `buffer_size` bytes while the decoder has the image.
   */
  uint8_t *memory;
  /*
   Bytes of the buffer.
   */
  size_t buffer_size;
  /*
   `ID3D12Resource *` of the textures of the luma, U and V plane, or null.
   */
  void *d3d12_textures[3];
  /*
   The application's name for the image; the decoder only carries it.
   */
  void *token;
} Rav1gImage;

/*
 A decoded frame, in the image the application gave for it. Valid during `Rav1gOutput::frame`.
 */
typedef struct Rav1gFrame {
  /*
   The image, as `Rav1gOutput::get_image` filled it.
   */
  struct Rav1gImage image;
  /*
   Luma samples of a row, and rows.
   */
  uint32_t width;
  uint32_t height;
  /*
   Bits a sample: 8 or 10.
   */
  uint32_t bit_depth;
  /*
   The frame has a luma plane only.
   */
  bool monochrome;
  /*
   The samples are packed three to 32 bits (`Rav1gImageFormat::Packed10x3`).
   */
  bool packed_10x3;
  /*
   What was passed to `rav1g_decoder_decode` with the temporal unit of the frame.
   */
  uint64_t user_data;
  /*
   The planes (luma, U, V) in the mapping of the image's buffer; null without a mapping, and for the
   chroma planes of a monochrome frame.
   */
  const uint8_t *planes[3];
  /*
   Bytes from one row of a plane to the next.
   */
  uint32_t strides[3];
  /*
   `ID3D12Fence *` of the command list that wrote the frame into its image, and the value the fence
   has when that list has run. The frame is handed over when the list has completed, so the fence has
   reached the value; the pair is here for an application that orders its GPU work by fence. The
   pointer is valid during the call (AddRef to keep it).
   */
  void *d3d12_fence;
  uint64_t fence_value;
} Rav1gFrame;

/*
 The output of a decoder: where it gets its images and puts its frames. `get_image` and `unused` are
 called on the thread that calls `rav1g_decoder_decode` (or destroys the decoder), `frame` and
 `end_of_frames` on the decoder's completion thread. `get_image` and `frame` are needed.
 */
typedef struct Rav1gOutput {
  /*
   Passed to every callback.
   */
  void *context;
  /*
   An image for a frame that is going to be output: fills `image` and returns 0, or returns another
   value, which fails the temporal unit. The call may wait until the application has an image free;
   the decoder then waits with it.
   */
  int32_t (*get_image)(void *context, const struct Rav1gImageRequest *request, struct Rav1gImage *image);
  /*
   A frame is ready: the GPU has written it into its image. Frames arrive in output order. The image
   is the application's again when the call returns.
   */
  void (*frame)(void *context, const struct Rav1gFrame *frame);
  /*
   An image the decoder took and did not write a frame into; `token` is `Rav1gImage::token`.
   */
  void (*unused)(void *context, void *token);
  /*
   Every frame that was submitted before a drain has been handed over.
   */
  void (*end_of_frames)(void *context);
} Rav1gOutput;

/*
 What a decoder is created with. `rav1g_settings_default` fills the defaults.
 */
typedef struct Rav1gSettings {
  /*
   The frame size the buffers are made for: the largest frame of the sequence
   (`rav1g_peek_stream_info`). The decoder rounds both up to a multiple of 128.
   */
  uint32_t width;
  uint32_t height;
  /*
   8 for a decoder with 8-bit frame buffers (an 8-bit stream), 10 otherwise.
   */
  uint32_t bit_depth;
  /*
   Tile threads.
   */
  uint32_t threads;
  /*
   Pipeline slots: frames that can be between the start of their CPU decode and the end of their GPU
   work at the same time, 2 to 4. 0 is the default, 2. Each slot has buffers of its own.
   */
  uint32_t frame_slots;
  /*
   The memory reductions: `RAV1G_LOW_MEMORY_*` values added up, 0 for none.
   */
  uint32_t low_memory;
  /*
   Buffers for the second intra path (`rav1g_set_intra_v2`).
   */
  bool intra_v2;
  /*
   Room in the block maps for the lists of the fast inter path (`rav1g_set_inter_fast`).
   */
  bool inter_fast;
  /*
   10-bit frames are put out with three samples packed into 32 bits.
   */
  bool packed_10x3;
  /*
   The frame of every spatial layer of a temporal unit is output, instead of the highest one.
   */
  bool output_all_layers;
  /*
   The operating point of a stream with layers.
   */
  uint32_t operating_point;
  /*
   What reads the bitstream: `RAV1G_PARSER_RAV1D` (the default) or `RAV1G_PARSER_LIBAOM`.
   */
  uint32_t parser;
  /*
   `RAV1G_PARSER_OPTION_*` values added up, for `RAV1G_PARSER_RAV1D`: `RAV1G_PARSER_OPTIONS_DEFAULT`
   by default; 0 for none.
   */
  uint32_t parser_options;
  /*
   The budget guard, or null. On a console an application that goes over its memory limit loses its
   D3D12 device, so with this callback the decoder asks before it allocates. The callback returns the
   bytes the decoder may still take: what the application has left of its limit, less what it is
   going to allocate itself (output images that do not exist yet), or a negative value when that is
   not known. A creation whose device buffers would leave less than `RAV1G_MEMORY_GUARD_MARGIN` is
   refused (`Rav1gStatus::Memory`, a "MEMORY REFUSED" log line), and so is a palette buffer that would
   leave less than a third of that (a decode error). It is called on the threads that create, use and
   destroy the decoder, with `memory_available_context`.
   */
  int64_t (*memory_available)(void *context);
  void *memory_available_context;
  /*
   The output of the decoder.
   */
  struct Rav1gOutput output;
} Rav1gSettings;

/*
 What a decoder of some settings takes.
 */
typedef struct Rav1gMemoryRequirements {
  /*
   Bytes of host memory, which the decoder allocates at creation, before it asks
   `Rav1gSettings::memory_available`: the pool of the entropy decoder and the decoder itself.
   */
  size_t host_size;
  /*
   Bytes of the device buffers the creation allocates.
   */
  size_t device_size;
  /*
   Bytes of the device buffers that are only created when a stream needs them: one palette buffer per
   pipeline slot.
   */
  size_t device_size_deferred;
} Rav1gMemoryRequirements;

/*
 What `rav1g_parse_digest_result` writes.
 */
typedef struct Rav1gParseDigestResult {
  /*
   Frames parsed without an error, and with one.
   */
  uint64_t frames;
  uint64_t failed_frames;
  /*
   Blocks, coded transform blocks and coefficients (up to the last one of each transform block).
   */
  uint64_t blocks;
  uint64_t tx_blocks;
  uint64_t coefs;
  /*
   The number of the stream.
   */
  uint64_t hash;
} Rav1gParseDigestResult;

/*
 A record of the timing run (`av1_perf_frame` of `dx/av1_perf.h` in the decoder's sources).
 */
typedef void (*Rav1gPerfCallback)(void *context, const av1_perf_frame *frame);

/*
 The buffers behind a GPU stage of a frame (`av1_stage_dump` of `dx/av1_perf.h`).
 */
typedef void (*Rav1gStageDumpCallback)(void *context, const av1_stage_dump *dump);

/*
 A log line of the decoder.
 */
typedef void (*Rav1gLogCallback)(void *context, bool is_error, const char *line);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/*
 The text of the last error on this thread of a call that has no decoder to ask:
 `rav1g_device_create`, `rav1g_device_wait_pipelines`, `rav1g_decoder_create`. For a creation the
 budget guard refused or that ran out of memory it ends with the decoder's "MEMORY REFUSED" or
 "MEMORY FAILED" line. Valid until the next such call on the thread.
 */
const char *rav1g_last_error(void);

/*
 The version of the library and what it was built with, for a log:
 "rav1g 0.4.0, the Rust host; entropy decoder: MSVC, /GL, DLL runtime".
 */
const char *rav1g_version(void);

/*
 Reads the sequence header of a temporal unit, without a decoder.

 # Safety
 `data` is valid for `size` bytes; `info` is valid for writing.
 */
enum Rav1gStatus rav1g_peek_stream_info(const uint8_t *data, size_t size, struct Rav1gStreamInfo *info);

/*
 A device for decoders. `d3d12_device` is the application's `ID3D12Device *`, or null for a device of
 the decoder's own on the first hardware adapter; `d3d12_queue` is the direct `ID3D12CommandQueue *` the
 decoders submit to, or null for a queue of the decoder's own. The decoder's compute pipeline states
 are created on `pipeline_threads` threads (6 at most) while the call returns
 (`rav1g_device_wait_pipelines`), or by this call with 0. Null when the device, the queue or a pipeline
 layout could not be created (`rav1g_last_error`).

 # Safety
 The pointers are null or valid interfaces of the kinds named.
 */
struct Rav1gDevice *rav1g_device_create(void *d3d12_device, void *d3d12_queue, uint32_t pipeline_threads);

/*
 Waits until every pipeline state of the device exists; the first decoder creation does the same. An
 error when one could not be created; the log names it.

 # Safety
 `device` is a device of `rav1g_device_create`.
 */
enum Rav1gStatus rav1g_device_wait_pipelines(const struct Rav1gDevice *device);

/*
 The `ID3D12Device *` of the device; no reference is added.

 # Safety
 `device` is a device of `rav1g_device_create`.
 */
void *rav1g_device_d3d12_device(const struct Rav1gDevice *device);

/*
 The `ID3D12CommandQueue *` the decoders of the device submit to; no reference is added.

 # Safety
 `device` is a device of `rav1g_device_create`.
 */
void *rav1g_device_d3d12_queue(const struct Rav1gDevice *device);

/*
 Releases the application's handle of a device. The D3D12 device, the queue and the pipeline states
 live until the last decoder of the device is destroyed.

 # Safety
 `device` is null or a device of `rav1g_device_create`, which is not used afterwards.
 */
void rav1g_device_destroy(struct Rav1gDevice *device);

/*
 One log line with the video memory of the adapter of an `ID3D12Device *` ("MEMORY dxgi `label`: ...").

 # Safety
 `d3d12_device` is null or a valid `ID3D12Device *`; `label` is null or a C string.
 */
void rav1g_log_video_memory(void *d3d12_device, const char *label);

/*
 The defaults: a decoder for 8-bit 1920x1088 frames with one tile thread, two pipeline slots and the
 shared back buffer, the parse pass of rav1d as its parser (`RAV1G_PARSER_RAV1D` with
 `RAV1G_PARSER_OPTIONS_DEFAULT`), no budget guard, and no output (the application sets `output`).

 # Safety
 `settings` is valid for writing.
 */
void rav1g_settings_default(struct Rav1gSettings *settings);

/*
 The memory a decoder of some settings takes.

 # Safety
 `settings` is valid; `requirements` is valid for writing.
 */
void rav1g_memory_requirements(const struct Rav1gSettings *settings,
                               struct Rav1gMemoryRequirements *requirements);

/*
 Creates a decoder on a device; waits for the pipeline states of the device when they are still being
 created. `*decoder` is the decoder, or null with the reason as the result and in `rav1g_last_error`.
 The context of the output callbacks and of the budget callback must stay valid until
 `rav1g_decoder_destroy` has returned.

 # Safety
 `device` is a device of `rav1g_device_create`; `settings` is valid; `decoder` is valid for writing.
 */
enum Rav1gStatus rav1g_decoder_create(const struct Rav1gDevice *device,
                                      const struct Rav1gSettings *settings,
                                      struct Rav1gDecoder **decoder);

/*
 Decodes one temporal unit. `user_data` comes back with the frames of the unit
 (`Rav1gFrame::user_data`). The call returns when the frames of the unit are recorded and submitted to
 the GPU, not when they are complete; it waits when every pipeline slot is in flight, and when the
 output has no image. On an error `rav1g_decoder_error` and `rav1g_decoder_error_detail` say more.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time; `data` is valid for
 `size` bytes.
 */
enum Rav1gStatus rav1g_decoder_decode(struct Rav1gDecoder *decoder,
                                      const uint8_t *data,
                                      size_t size,
                                      uint64_t user_data);

/*
 The text of the kind of the last error of the decoder ("Corrupt frame detected", ...); valid until the
 next call on the decoder.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`.
 */
const char *rav1g_decoder_error(const struct Rav1gDecoder *decoder);

/*
 What the decoder said about its last error, an empty string when it said nothing; valid until the next
 call on the decoder.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`.
 */
const char *rav1g_decoder_error_detail(const struct Rav1gDecoder *decoder);

/*
 Waits for the frames in flight: every frame that was submitted has been handed to the output, then
 `Rav1gOutput::end_of_frames` is called. The decoder can go on decoding afterwards.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
 */
void rav1g_decoder_drain(struct Rav1gDecoder *decoder);

/*
 The frame of every spatial layer of a temporal unit is output, instead of the highest one.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
 */
enum Rav1gStatus rav1g_decoder_set_output_all_layers(struct Rav1gDecoder *decoder, bool enable);

/*
 The operating point of a stream with layers.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
 */
enum Rav1gStatus rav1g_decoder_set_operating_point(struct Rav1gDecoder *decoder, uint32_t operating_point);

/*
 Waits for the frames in flight, hands them over and releases the decoder. With a budget callback the
 release is logged step by step ("MEMORY destroy").

 # Safety
 `decoder` is null or a decoder of `rav1g_decoder_create`, which is not used afterwards.
 */
void rav1g_decoder_destroy(struct Rav1gDecoder *decoder);

/*
 CDEF through `cdef_filter_v2.hlsl` (true, the default) or `cdef_filter.hlsl`.
 */
void rav1g_set_cdef_v2(bool enable);

/*
 Intra prediction through `intra_v2.hlsl`; only in a decoder created with `Rav1gSettings::intra_v2`.
 */
void rav1g_set_intra_v2(bool enable);

/*
 The optimised build of the original intra shader.
 */
void rav1g_set_intra_opt(bool enable);

/*
 The clears of a pipeline slot on the completion thread instead of the decode thread.
 */
void rav1g_set_async_clear(bool enable);

/*
 The fast inter path: 0, or `RAV1G_INTER_FAST_ON` with the `RAV1G_INTER_FAST_*` bits; only in a decoder
 created with `Rav1gSettings::inter_fast`.
 */
void rav1g_set_inter_fast(uint32_t mode);

/*
 Experiment: how the horizontal loop filter is dispatched, 0 (one dispatch per plane), 1 or 2.
 */
void rav1g_set_lf_h_split(uint32_t mode);

/*
 Experiment: the 8-bit above edge filter bound of the intra shader.
 */
void rav1g_set_intra_edge_fix(bool enable);

/*
 Debugging: the decoders created after the call fill the mode info pool (`RAV1G_POISON_MODE_INFO`) or
 every buffer the CPU writes for the shaders (`RAV1G_POISON_BUFFERS`) with the byte 0xA5 when they
 create it; 0 switches it off. The output must be the same.
 */
void rav1g_set_poison_buffers(uint32_t level);

/*
 The inter breakdown of the timing records: 0 off, 1 sections, 2 every dispatch alone.
 */
void rav1g_set_perf_detail(uint32_t mode);

/*
 A parser for one stream. `threads`: its threads (0 or 1: the thread that sends the data). Null when
 it could not be created.
 */
struct Rav1gParseDigest *rav1g_parse_digest_create(uint32_t threads);

/*
 Parses the frames of a temporal unit. False when a frame of it could not be parsed.

 # Safety
 `digest` is a parser of `rav1g_parse_digest_create`; `data` is valid for `size` bytes.
 */
bool rav1g_parse_digest_send(struct Rav1gParseDigest *digest, const uint8_t *data, size_t size);

/*
 What the stream came to so far.

 # Safety
 `digest` is a parser of `rav1g_parse_digest_create`; `result` is valid for writing.
 */
void rav1g_parse_digest_result(const struct Rav1gParseDigest *digest, struct Rav1gParseDigestResult *result);

/*
 Ends a parser. Null does nothing.

 # Safety
 `digest` is null or a parser of `rav1g_parse_digest_create` that is not used afterwards.
 */
void rav1g_parse_digest_destroy(struct Rav1gParseDigest *digest);

/*
 An empty string for the normal build of the shaders, "uavsync" for the experiment build.
 */
const char *rav1g_shader_variant(void);

/*
 The timing run: with a callback every decoder takes GPU timestamps and CPU timers per stage and hands
 one record per command list to the callback, on its completion thread. Null switches the timing off.

 # Safety
 `callback` is null or a function that may be called from the decoders' threads with `context` until it
 is replaced.
 */
void rav1g_set_perf_callback(Rav1gPerfCallback callback, void *context);

/*
 The stage dumps: the decoders copy the buffers behind every GPU stage of the frames whose numbers (in
 decoding order, from 0 per decoder) are the `count` values at `frames` and hand them to the callback on
 their completion thread. Null, or no frames, switches them off.

 # Safety
 `callback` is null or a function that may be called from the decoders' threads with `context` until it
 is replaced; `frames` is valid for `count` values.
 */
void rav1g_set_stage_dump(Rav1gStageDumpCallback callback,
                          void *context,
                          const int32_t *frames,
                          size_t count);

/*
 The log lines of the decoders ("MEMORY ...", errors), from whatever thread a decoder is on. Without a
 callback the lines go to the debugger output.

 # Safety
 `callback` is null or a function that may be called from any thread with `context` until it is
 replaced.
 */
void rav1g_set_log_callback(Rav1gLogCallback callback, void *context);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* RAV1G_H */

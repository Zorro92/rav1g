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
 *      frame size, the bit depth and the colour description of the sequence.
 *   3. rav1g_settings_default, the size and bit depth of the sequence, the output callbacks;
 *      rav1g_decoder_create.
 *   4. rav1g_decoder_decode per temporal unit. The decoder takes an image from the output for every frame
 *      that will be shown (get_image) and hands the frame over when the GPU has written it (frame).
 *      RAV1G_STATUS_NEEDS_RECONFIGURE: the stream went on with a sequence the decoder does not fit.
 *      A seek: rav1g_decoder_flush, then the temporal units from the new place (docs/integration.md).
 *   5. rav1g_decoder_drain, rav1g_decoder_destroy, rav1g_device_destroy.
 *
 * Colour: the decoder puts out the samples of the stream and converts nothing. What they mean is in
 * Rav1gStreamInfo::color and, with the HDR metadata, in every Rav1gFrame; the application that shows the
 * frames has to use it (docs/color.md).
 *
 * Output: by default an image is a buffer with the planes of the frame, one texture a plane, or both.
 * With Rav1gSettings::output_kind RAV1G_OUTPUT_SEMI_PLANAR it is one NV12 or P010 texture, which the
 * decoder's output pass writes and an application can share with a D3D11 device; the output fence
 * (rav1g_decoder_output_fence, Rav1gFrame::fence_value) says when a frame is written (docs/output.md).
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
 The shared back buffer: one back buffer for all pipeline slots and a pool of 10 frame buffers.
 */
#define RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER 1

/*
 A mode info pool of half the worst case of a frame: 55.8 MiB less for 3840x2160. A frame that needs
 more than the pool is a decode error.
 */
#define RAV1G_LOW_MEMORY_MODE_INFO 2

/*
 A coefficient pool of four fifths of the worst case of a frame, likewise. In the default above
 1920x1088.
 */
#define RAV1G_LOW_MEMORY_COEFS 4

/*
 The three memory reductions above.
 */
#define RAV1G_LOW_MEMORY_ALL 7

/*
 A frame of a decoder with more than one frame in parse takes its frame buffer when it is recorded,
 not when its parse begins, and the pool has no frame buffer for the slots above two: 10 with the
 shared back buffer, where three slots have 11 (24.2 MiB less for 3840x2160 10-bit). Covers every
 stream. It has an effect only together with `RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER`. Not in
 `RAV1G_LOW_MEMORY_ALL`; in the default above 1920x1088.
 */
#define RAV1G_LOW_MEMORY_LATE_FRAME_BUFFER 32

/*
 What `rav1g_settings_default` sets: the default, which is by the size of the decoder. Up to 1920x1088
 it is `RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER`; above, `RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER |
 RAV1G_LOW_MEMORY_MODE_INFO | RAV1G_LOW_MEMORY_COEFS | RAV1G_LOW_MEMORY_LATE_FRAME_BUFFER`, every part (memory is short at 4K; the whole pool of a decoder for 1080p is 29.5 MiB,
 and the streams that come near the worst case, lossless and low quantizers, are of such sizes). Not a
 part to add: settings that have it get the default. The parts alone are taken as they are at every
 size, `RAV1G_LOW_MEMORY_SHARED_BACK_BUFFER` for a decoder of any size that has to take every stream.
 */
#define RAV1G_LOW_MEMORY_DEFAULT 16

/*
 `Rav1gSettings::parser_options`, values to add up: ways of doing the parse (rav1d's parse pass)
 that give the same frames (docs/phase2-rav1d.md, "Where the time goes"). The values 1 and 256 were
 switches that are gone and mean nothing. The values 16, 32, 1024 and 4096 were switches retired with
 interface version 3 (`LATE_LISTS`, `CLEAR_GRID`, `GPU_GRID_CHECK`, `GPU_PRED_LISTS`):
 `rav1g_decoder_create` refuses settings that have one of them (`RAV1G_STATUS_INVALID_PARAM`). This
 one: rav1d leaves out the masks and levels of its own loop filter, which nothing here reads. Part of
 `RAV1G_PARSER_OPTIONS_DEFAULT`.
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
 For a test of the decoder, never for an application: the parse of the third frame panics, on the
 thread that parses it, which is what a bug of the parser does. The decode call that finds it returns
 `Rav1gStatus::Error` and the decoder decodes nothing more.
 */
#define RAV1G_PARSER_OPTION_TEST_PANIC 64

/*
 For a test of the decoder, never for an application: behind its first piece, a frame finds the pools
 the frames in flight take their mode info records, coefficients and transform block records from
 empty for as long as an older frame holds anything of them, so every frame waits in the middle of
 its parse for memory to come back, as a frame does that finds a pool exhausted. The frames are the
 same.
 */
#define RAV1G_PARSER_OPTION_TEST_POOL_WAIT 128

/*
 The two lists CDEF reads besides the frame (the strength index of every 64x64 block, the skip flag
 of every 8x8 block) are written by a pass of the frame's command list, in front of CDEF, and not by
 the thread that records the frame, which reads the frame's mode info again for them (3 ms of a
 heavy 4K frame on the console). The lists are the same. Read when the decoder is created: the two
 buffers of every pipeline slot are then buffers the GPU may write. Part of
 `RAV1G_PARSER_OPTIONS_DEFAULT`.
 */
#define RAV1G_PARSER_OPTION_GPU_CDEF_LISTS 512

/*
 The mode info grid of a frame (for every 4x4 unit the record of its block) is built by a pass of the
 frame's command list from the frame's mode info records, in front of every stage that reads it, and
 not written by the threads that parse (1.98 MiB a 4K frame, 1.1 to 1.2 ms a frame of those threads on
 the console). The grid is the same. Read when the decoder is created: the grid of every pipeline slot
 is then a buffer the GPU may write, and the decoder works as with
 `RAV1G_PARSER_OPTION_GPU_CDEF_LISTS`, set or not. Part of `RAV1G_PARSER_OPTIONS_DEFAULT`.
 */
#define RAV1G_PARSER_OPTION_GPU_GRID 2048

/*
 The most frames a decoder parses at the same time (`Rav1gSettings::frames_in_parse`).
 */
#define RAV1G_FRAMES_IN_PARSE_MAX 4

/*
 The `Rav1gSettings::parser_options` of `rav1g_settings_default`: `RAV1G_PARSER_OPTION_NO_LF_MASKS`,
 `RAV1G_PARSER_OPTION_TILE_KEEPS_THREAD`, `RAV1G_PARSER_OPTION_GPU_CDEF_LISTS` and
 `RAV1G_PARSER_OPTION_GPU_GRID`. An application that wants the timing as well adds
 `RAV1G_PARSER_OPTION_TIME_CALLS` to it; 0 is rav1d's parse without any of them.
 */
#define RAV1G_PARSER_OPTIONS_DEFAULT 2566

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
 `Rav1gDeviceSettings::prepare`, the default: every pipeline state (96) is compiled at the device
 creation, as before interface version 4.
 */
#define RAV1G_PREPARE_ALL 0

/*
 `Rav1gDeviceSettings::prepare` with `RAV1G_PREPARE_ONLY`: the states of 8-bit streams (prediction,
 reconstruction, loop filter; 17 states).
 */
#define RAV1G_PREPARE_BIT_DEPTH_8 1

/*
 The states of streams of more than 8 bits (17).
 */
#define RAV1G_PREPARE_BIT_DEPTH_10 2

/*
 The display pass (`Rav1gSettings::display`) of SDR frames (`RAV1G_DISPLAY_TRANSFORM_SCALE` and
 `_GAMUT`): one state.
 */
#define RAV1G_PREPARE_DISPLAY_SDR 4

/*
 The display pass of PQ frames: one state.
 */
#define RAV1G_PREPARE_DISPLAY_PQ 8

/*
 The display pass of HLG frames: one state.
 */
#define RAV1G_PREPARE_DISPLAY_HLG 16

/*
 Film grain synthesis (3 states).
 */
#define RAV1G_PREPARE_FILM_GRAIN 32

/*
 The super-resolution upscale (2 states).
 */
#define RAV1G_PREPARE_SUPERRES 64

/*
 Inter prediction from references of another frame size, of the bit depths named (8 states a depth).
 */
#define RAV1G_PREPARE_SCALED_REFERENCES 128

/*
 Only the states of the `RAV1G_PREPARE_*` bits next to it, and those every stream needs (the
 transforms, CDEF, loop restoration, the copies and block lists: 38 states). Without it the value is
 `RAV1G_PREPARE_ALL`, whatever else it has.
 */
#define RAV1G_PREPARE_ONLY 1073741824

/*
 `Rav1gColorInfo::primaries`, the values an application meets (AV1 specification 6.4.2, ITU-T H.273):
 BT.709, which is also sRGB.
 */
#define RAV1G_COLOR_PRIMARIES_BT709 1

/*
 The stream does not say; so do `RAV1G_TRANSFER_UNSPECIFIED` and `RAV1G_MATRIX_UNSPECIFIED`.
 */
#define RAV1G_COLOR_PRIMARIES_UNSPECIFIED 2

/*
 BT.601 with 625 lines (PAL).
 */
#define RAV1G_COLOR_PRIMARIES_BT470BG 5

/*
 BT.601 with 525 lines (NTSC).
 */
#define RAV1G_COLOR_PRIMARIES_BT601 6

/*
 BT.2020 and BT.2100: UHD and HDR video.
 */
#define RAV1G_COLOR_PRIMARIES_BT2020 9

/*
 Display P3.
 */
#define RAV1G_COLOR_PRIMARIES_SMPTE432 12

/*
 `Rav1gColorInfo::transfer_characteristics`: BT.709.
 */
#define RAV1G_TRANSFER_BT709 1

#define RAV1G_TRANSFER_UNSPECIFIED 2

/*
 BT.601.
 */
#define RAV1G_TRANSFER_BT601 6

#define RAV1G_TRANSFER_SRGB 13

/*
 BT.2020 for 10 bits: the curve of BT.709.
 */
#define RAV1G_TRANSFER_BT2020_10BIT 14

/*
 SMPTE ST 2084, the perceptual quantiser: HDR10.
 */
#define RAV1G_TRANSFER_PQ 16

/*
 ARIB STD-B67, hybrid log-gamma: HLG.
 */
#define RAV1G_TRANSFER_HLG 18

/*
 `Rav1gColorInfo::matrix_coefficients`: BT.709.
 */
#define RAV1G_MATRIX_BT709 1

#define RAV1G_MATRIX_UNSPECIFIED 2

/*
 BT.601 with 625 lines.
 */
#define RAV1G_MATRIX_BT470BG 5

/*
 BT.601 with 525 lines; the same matrix as `RAV1G_MATRIX_BT470BG`.
 */
#define RAV1G_MATRIX_BT601 6

/*
 BT.2020 with non-constant luminance: HDR10 and HLG video.
 */
#define RAV1G_MATRIX_BT2020_NCL 9

/*
 `Rav1gColorInfo::chroma_sample_position`: the stream does not say.
 */
#define RAV1G_CHROMA_SAMPLE_POSITION_UNKNOWN 0

/*
 In the column of the left luma samples, between the two rows ("left"): MPEG-2 and most HD video.
 */
#define RAV1G_CHROMA_SAMPLE_POSITION_VERTICAL 1

/*
 On the top left luma sample ("top left"): BT.2020 and BT.2100 video.
 */
#define RAV1G_CHROMA_SAMPLE_POSITION_COLOCATED 2

/*
 The budget guard refuses a decoder whose device buffers would leave the application less than this
 many bytes (`Rav1gSettings::memory_available`).
 */
#define RAV1G_MEMORY_GUARD_MARGIN 50331648

/*
 The default of `Rav1gSettings::memory_headroom`: what a creation should leave of what
 `Rav1gSettings::memory_available` returns so that its frames are fast, 128 MiB.
 */
#define RAV1G_MEMORY_GUARD_HEADROOM 134217728

/*
 `Rav1gSettings::output_kind`, the default: an output image is a buffer with the planes of the frame
 one behind the other, one texture a plane, or both.
 */
#define RAV1G_OUTPUT_PLANES 0

/*
 `Rav1gSettings::output_kind`: the semi-planar output. An output image is one texture of
 `DXGI_FORMAT_NV12` (8-bit frames) or `DXGI_FORMAT_P010` (10-bit frames), which the output pass of the
 decoder writes (docs/output.md).
 */
#define RAV1G_OUTPUT_SEMI_PLANAR 1

/*
 `Rav1gDisplay::transform`, the default: each frame by its colour description (with the assumed code
 points): PQ `RAV1G_DISPLAY_TRANSFORM_PQ`, HLG `RAV1G_DISPLAY_TRANSFORM_HLG`, other transfers with
 BT.2020 primaries `RAV1G_DISPLAY_TRANSFORM_GAMUT`, the rest `RAV1G_DISPLAY_TRANSFORM_SCALE`.
 */
#define RAV1G_DISPLAY_TRANSFORM_AUTO 0

/*
 `Rav1gFrame::display_transform`: no display pass, the image is the frame itself.
 */
#define RAV1G_DISPLAY_TRANSFORM_NONE 0

/*
 SDR with BT.709 primaries (or any but BT.2020): downscaled in display-linear light (BT.1886), no tone
 map, no gamut step, no clip before the encode.
 */
#define RAV1G_DISPLAY_TRANSFORM_SCALE 1

/*
 SDR with BT.2020 primaries: downscaled in display-linear light, BT.2020 to BT.709 (BT.2087), clipped.
 */
#define RAV1G_DISPLAY_TRANSFORM_GAMUT 2

/*
 PQ (SMPTE ST 2084): the BT.2390 EETF from the source peak to 2.03 times the SDR white on the largest
 channel, BT.2020 to BT.709 when the primaries are BT.2020, clipped.
 */
#define RAV1G_DISPLAY_TRANSFORM_PQ 3

/*
 HLG (ARIB STD-B67): the inverse OETF and the OOTF of a 1000-nit display (BT.2100), then as PQ.
 */
#define RAV1G_DISPLAY_TRANSFORM_HLG 4

/*
 `Rav1gDisplay::tone_curve`: ITU-R BT.2390's EETF, the default (0 is the same).
 */
#define RAV1G_TONE_CURVE_BT2390 1

/*
 `Rav1gDisplay::tone_curve`: Hable's filmic curve. Not in this version: `rav1g_decoder_create` returns
 `RAV1G_STATUS_INCAPABLE` (the application keeps its own stage).
 */
#define RAV1G_TONE_CURVE_HABLE 2

/*
 `Rav1gDisplay::dither`, the default: the ordered dither for 8-bit codes (NV12), rounded 10-bit codes
 (P010).
 */
#define RAV1G_DISPLAY_DITHER_AUTO 0

/*
 `Rav1gDisplay::dither`: rounded at either depth.
 */
#define RAV1G_DISPLAY_DITHER_OFF 1

/*
 `Rav1gDisplay::dither`: the ordered dither at either depth.
 */
#define RAV1G_DISPLAY_DITHER_ORDERED 2

/*
 The version of the interface of this header: raised whenever a declaration of `rav1g.h` changes (a
 struct, an enum, a function), and not for a constant, a comment or a change inside the library.
 `rav1g_abi_version` returns the value the library was built with; an application that loads the
 library at run time compares the two and refuses a library with another value.
 */
#define RAV1G_ABI_VERSION 4

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
   The data of a frame is damaged or cut, or a frame names a reference whose own decode failed. Not
   a frame whose references were never decoded (a start in the middle of a stream): that frame is
   skipped without an error (`rav1g_decoder_decode`).
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
  /*
   The semi-planar output (`RAV1G_OUTPUT_SEMI_PLANAR`) of an 8-bit frame: a luma plane and one
   chroma plane with U and V side by side, one byte a sample; one texture of `DXGI_FORMAT_NV12`.
   */
  RAV1G_IMAGE_FORMAT_NV12 = 3,
  /*
   The same for a 10-bit frame: two bytes a sample, the 10 bits in the high bits (the sample times
   64); one texture of `DXGI_FORMAT_P010`.
   */
  RAV1G_IMAGE_FORMAT_P010 = 4,
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
 The colour description of a sequence header (`color_config` of the AV1 specification): what the
 samples of its frames mean. The decoder does not use it; the application that shows the frames does
 (docs/color.md).

 The three code points are the numbers of the stream (AV1 specification 6.4.2, which are those of
 ITU-T H.273), also values the specification reserves; the `RAV1G_COLOR_PRIMARIES_*`,
 `RAV1G_TRANSFER_*` and `RAV1G_MATRIX_*` constants name the ones an application meets. The matrix and
 the range are what the conversion to RGB needs. The transfer says what output the frames want: PQ
 (with BT.2020 primaries: HDR10) an HDR10 output, HLG an HLG output or a conversion to PQ, the others
 an SDR output.
 */
typedef struct Rav1gColorInfo {
  /*
   The header has a colour description (`color_description_present_flag`). Without one the three
   code points are 2 (unspecified), and the application decides: what a container says, or BT.709.
   */
  bool description_present;
  /*
   `color_primaries`.
   */
  uint8_t primaries;
  /*
   `transfer_characteristics`.
   */
  uint8_t transfer_characteristics;
  /*
   `matrix_coefficients`: what turns luma and chroma into red, green and blue.
   */
  uint8_t matrix_coefficients;
  /*
   `color_range`: true for full range (0 to 255 in 8 bits), false for limited range (luma 16 to
   235, chroma 16 to 240 in 8 bits; times 4 in 10 bits). Always coded; it is needed with the matrix.
   */
  bool full_range;
  /*
   `chroma_sample_position`: `RAV1G_CHROMA_SAMPLE_POSITION_*`, 3 is reserved.
   */
  uint8_t chroma_sample_position;
} Rav1gColorInfo;

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
  /*
   The sequence has a luma plane only.
   */
  bool monochrome;
  /*
   The colour description of the sequence header. Every `Rav1gFrame` has the one of its own
   sequence; HDR metadata is not in a sequence header and comes with the frames.
   */
  struct Rav1gColorInfo color;
  /*
   `film_grain_params_present` and `enable_superres` of the sequence header: its frames can have
   film grain, super-resolution. New in interface version 4 (`Rav1gSettings::stream`).
   */
  bool film_grain_params_present;
  bool enable_superres;
} Rav1gStreamInfo;

/*
 The mastering display colour volume of a frame (the metadata OBU `METADATA_TYPE_HDR_MDCV`, SMPTE ST
 2086), in the fixed point of the OBU. That is not the fixed point of `DXGI_HDR_METADATA_HDR10`: a
 chromaticity is `value / 65536` here and `value / 50000` there, the largest luminance `value / 256`
 cd/m² here and whole cd/m² there, the smallest `value / 16384` cd/m² here and `value / 10000` there
 (docs/color.md has the conversion). The primaries are red, green, blue, in this order.
 */
typedef struct Rav1gMasteringDisplay {
  /*
   The chromaticity (x, y) of red, green and blue, in 0.16 fixed point.
   */
  uint16_t primaries[3][2];
  /*
   The chromaticity (x, y) of the white point, in 0.16 fixed point.
   */
  uint16_t white_point[2];
  /*
   The largest luminance in cd/m², in 24.8 fixed point.
   */
  uint32_t max_luminance;
  /*
   The smallest luminance in cd/m², in 18.14 fixed point.
   */
  uint32_t min_luminance;
} Rav1gMasteringDisplay;

/*
 The content light level of a frame (the metadata OBU `METADATA_TYPE_HDR_CLL`, CTA-861.3), in cd/m².
 */
typedef struct Rav1gContentLightLevel {
  /*
   MaxCLL: the brightest sample of the content.
   */
  uint16_t max_content_light_level;
  /*
   MaxFALL: the highest average of a frame.
   */
  uint16_t max_frame_average_light_level;
} Rav1gContentLightLevel;

/*
 What a device is created with (`rav1g_device_create_with_settings`). Zero in every field but
 `pipeline_threads` is the device of `rav1g_device_create`. New in interface version 4.
 */
typedef struct Rav1gDeviceSettings {
  /*
   The threads that compile the pipeline states, 6 at most (the console's 4 cores give no gain
   beyond 4): the prepared states while the creation returns (`rav1g_device_wait_pipelines` waits
   for them), and the states a decoder creation needs that are not prepared, next to the creating
   thread. With 0 the creation compiles the prepared states itself, and a decoder creation its own.
   */
  uint32_t pipeline_threads;
  /*
   Which states are compiled at the creation: `RAV1G_PREPARE_ALL` (0), or `RAV1G_PREPARE_ONLY` with
   the `RAV1G_PREPARE_*` bits of the streams the application is going to play, for a shorter
   creation that takes less memory. A decoder creation compiles what its stream needs that is not
   prepared and waits only for that (`Rav1gSettings::stream`); a state a later frame needs is
   compiled then, the frame waiting for it. No state is ever used before it exists, and the frames
   are the same either way (docs/integration.md, "Preparing the pipeline states").
   */
  uint32_t prepare;
  /*
   Zero.
   */
  uint32_t reserved[6];
} Rav1gDeviceSettings;

/*
 What the decoder needs an output image for: a frame that is going to be output.
 */
typedef struct Rav1gImageRequest {
  /*
   Bytes of a buffer that takes the planes of the frame.
   */
  size_t buffer_size;
  /*
   Luma samples of a row, and rows, of the frame. The texture of a semi-planar image
   (`Rav1gImageFormat::Nv12`, `Rav1gImageFormat::P010`) has both rounded up to even.
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

 With the semi-planar output (`RAV1G_OUTPUT_SEMI_PLANAR`; docs/output.md) the image is one texture,
 `d3d12_textures[0]`: `DXGI_FORMAT_NV12` for a request of `Rav1gImageFormat::Nv12`, `DXGI_FORMAT_P010`
 for one of `Rav1gImageFormat::P010`, of the width and height of the request, both rounded up to even,
 with one mip level. It is in the state `D3D12_RESOURCE_STATE_COMMON` when `get_image` returns, and in
 that state again when the frame is handed over; in between it is the decoder's. A texture created
 with `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` is written by the output pass itself, where the
 device lets shaders write such a texture; any other is written by one copy a plane. It may be a
 texture of another device or API (a D3D11 texture with a shared handle) that the application opened
 on the decoder's device. A buffer, alone or next to the texture, gets the two planes one behind the
 other, as `Rav1gFrame::planes` says.
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
   `ID3D12Resource *` of the textures of the luma, U and V plane, or null. With the semi-planar
   output the first is the NV12 or P010 texture and the other two are null.
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

   A semi-planar frame (`format`) has two: the luma plane and the plane of U and V side by side
   (U first), the third is null. A monochrome frame has both too, with the middle value (128, or 512
   times 64) in the second.
   */
  const uint8_t *planes[3];
  /*
   Bytes from one row of a plane to the next.
   */
  uint32_t strides[3];
  /*
   The output fence of the decoder (`ID3D12Fence *`, the one of `rav1g_decoder_output_fence`: the
   same for every frame of a decoder), and the value it has when the command list that wrote the
   frame into its image has run. The value of a frame is larger than that of the frame before it.
   The frame is handed over when the list has completed, so the fence has reached the value; the
   pair is here for an application that orders its GPU work by fence, and for one that reads the
   image on another device or API: it opens the fence there (`Rav1gSettings::shared_fence`) and waits
   for the value on that device's queue (`ID3D11DeviceContext4::Wait`, `ID3D12CommandQueue::Wait`).
   The pointer is valid during the call (AddRef to keep it).

   Up to 0.2.159 this was the fence of the pipeline slot that decoded the frame, one of several.
   */
  void *d3d12_fence;
  uint64_t fence_value;
  /*
   The colour description of the frame's sequence header. A stream can change it with a new
   sequence, so every frame has the one that holds for it.
   */
  struct Rav1gColorInfo color;
  /*
   `mastering_display` is the mastering display colour volume that goes with the frame; false when
   the stream has sent none (the struct is zero then).

   Which frames a metadata OBU goes with: every frame from the OBU on, in decoding order, until
   another OBU of its kind replaces it or a sequence header that differs from the one before starts
   a new sequence; a frame that a later temporal unit shows again (`show_existing_frame`) has what
   holds when it is shown. So a frame has the metadata that is current for it, and an application
   that wants "the latest" takes it from the last frame it got.
   */
  bool has_mastering_display;
  /*
   `content_light` is the content light level that goes with the frame, by the same rule.
   */
  bool has_content_light;
  struct Rav1gMasteringDisplay mastering_display;
  struct Rav1gContentLightLevel content_light;
  /*
   The samples of the image, as the request for it said. New in 0.2.160, behind the other fields.
   */
  enum Rav1gImageFormat format;
  /*
   What the display pass did (`Rav1gSettings::display`): a `RAV1G_DISPLAY_TRANSFORM_*`, or
   `RAV1G_DISPLAY_TRANSFORM_NONE` (0) when the image is the frame itself. With a transform, `width`,
   `height`, `bit_depth` and `format` describe the image (the display size and depth), `monochrome`
   is false, `color` is BT.709 SDR (primaries, transfer and matrix 1, limited range, chroma sample
   position 1), and what was decoded is in `source_width`, `source_height` and `source_color`; the HDR
   metadata stays the stream's.

   New in interface version 2, with the three fields behind it.
   */
  uint32_t display_transform;
  /*
   The frame that was decoded (after super-resolution): luma samples of a row, and rows. Without the
   display pass `width` and `height`.
   */
  uint32_t source_width;
  uint32_t source_height;
  /*
   The colour description of the frame's sequence header, as the stream has it (not the assumed code
   points). Without the display pass `color`.
   */
  struct Rav1gColorInfo source_color;
  /*
   The source peak the display pass's tone map took for the frame, nits (`Rav1gDisplay`, "The source
   peak" in docs/integration.md): for PQ the HDR metadata's (MaxCLL, else the mastering display's
   largest luminance, the smaller of both, clamped to 400..10000) or the configured
   `source_peak_nits`; for HLG the configured one; 0 without a tone map (no display pass, SDR).

   New in interface version 3.
   */
  float display_source_peak_nits;
} Rav1gFrame;

/*
 The output of a decoder: where it gets its images and puts its frames. `get_image` is called on the
 thread that calls `rav1g_decoder_decode` or, with more than one frame in parse, on a thread of the
 decoder; `unused` on the thread that asked for the image, or on the decoder's completion thread for the
 image of a frame a flush dropped (`rav1g_decoder_flush`); `frame` and `end_of_frames` on the completion
 thread. `get_image` and `frame` are needed.
 */
typedef struct Rav1gOutput {
  /*
   Passed to every callback.
   */
  void *context;
  /*
   An image for a frame that is going to be output: fills `image` and returns 0, or returns another
   value: the frame is then not output. It is decoded all the same (a reference of the frames behind
   it, which decode as ever), and `rav1g_decoder_decode` (with more than one frame in parse a later
   call or `rav1g_decoder_drain`) returns `RAV1G_STATUS_MEMORY` ("Failed to allocate output frame.")
   for it. The call may wait until the application has an image free; the decoder then waits with
   it. An application that seeks lets a waiting call return non-zero, then calls
   `rav1g_decoder_flush` (docs/integration.md).
   */
  int32_t (*get_image)(void *context, const struct Rav1gImageRequest *request, struct Rav1gImage *image);
  /*
   A frame is ready: the GPU has written it into its image. Frames arrive in output order. The image
   is the application's again when the call returns.
   */
  void (*frame)(void *context, const struct Rav1gFrame *frame);
  /*
   An image the decoder took and did not write a frame into, or the image of a frame a flush dropped
   (when the GPU is through with it); `token` is `Rav1gImage::token`.
   */
  void (*unused)(void *context, void *token);
  /*
   Every frame that was submitted before a drain has been handed over. A flush does not call it.
   */
  void (*end_of_frames)(void *context);
} Rav1gOutput;

/*
 The display pass (docs/integration.md, "The display pass"): every output image is the frame
 downscaled to the display size (area weights in display-linear light) and tone-mapped or
 gamut-converted to BT.709 SDR, written by one compute pass at the end of the frame's command list into
 an NV12 or P010 texture of that size, in place of the frame itself. The decoded frames and the
 reference frames are the same with and without it. `width` and `height` 0 (what
 `rav1g_settings_default` sets) mean no display pass: the images are the frames, as in interface
 version 1.

 It needs `Rav1gSettings::output_kind` `RAV1G_OUTPUT_SEMI_PLANAR`, and images that are a texture alone
 created with `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS` (no buffer: the pass writes the texture, no
 copy does); another image fails the temporal unit with a "DISPLAY" log line. A device that lets no
 shader write the format gets no decoder (`RAV1G_STATUS_INCAPABLE`). Zero in any field is its default.
 */
typedef struct Rav1gDisplay {
  /*
   The size of every image (`Rav1gImageRequest::width`, `height`), both rounded up to even, at most
   16384. Any ratio to the frame works; exactly half (4K to 1080p), the same size (4K to 4K) and two
   thirds (4K to 1440p) of the frame in both are the fast paths.
   */
  uint32_t width;
  uint32_t height;
  /*
   `RAV1G_DISPLAY_TRANSFORM_AUTO` (0), or a `RAV1G_DISPLAY_TRANSFORM_*` every frame takes whatever its
   colour description says.
   */
  uint32_t transform;
  /*
   `RAV1G_TONE_CURVE_BT2390` (0 is the same) or `RAV1G_TONE_CURVE_HABLE` (not in this version).
   */
  uint32_t tone_curve;
  /*
   The PQ and HLG light the EETF maps to the SDR range: 0 is 1000 nits; at most 10000. A PQ frame
   takes the peak of its HDR metadata instead when there is one (`max_cll_nits`; docs/integration.md,
   "The source peak"): this is the peak of HLG, and of PQ without metadata.
   */
  float source_peak_nits;
  /*
   The SDR white: the EETF maps to 2.03 times it (203 nits for 100). 0 is 100 nits; up to 4926.
   */
  float target_white_nits;
  /*
   Not in this version: 0 (another value is `RAV1G_STATUS_INCAPABLE`).
   */
  float desaturation;
  /*
   The images' codes: 10 (`RAV1G_IMAGE_FORMAT_P010`), 8 (`RAV1G_IMAGE_FORMAT_NV12`), or 0: by
   `Rav1gSettings::bit_depth`, for every frame whatever its sequence's depth.
   */
  uint32_t depth;
  /*
   The H.273 code points taken where the sequence header says "unspecified" (2), as an application
   merges what its container says: 0 for none (unspecified is BT.709 SDR then).
   */
  uint8_t assumed_primaries;
  uint8_t assumed_transfer_characteristics;
  uint8_t assumed_matrix_coefficients;
  /*
   The AV1 `chroma_sample_position` taken where the header says 0 (unknown): 0 for none (unknown is
   between rows, as 1).
   */
  uint8_t assumed_chroma_sample_position;
  /*
   The last quantisation: `RAV1G_DISPLAY_DITHER_AUTO` (0: an ordered dither for NV12, rounded P010),
   `RAV1G_DISPLAY_DITHER_OFF` (rounded) or `RAV1G_DISPLAY_DITHER_ORDERED` (dithered at either depth).
   The dither is a threshold at each output sample from the R2 lattice, the same for every frame
   (docs/integration.md, "The dither").
   */
  uint32_t dither;
  /*
   The HDR metadata the application has for the stream (from its container, for example Matroska's
   MaxCLL and the mastering display's largest luminance), nits; 0 for none. The source peak of a PQ
   frame is MaxCLL, else the mastering display's largest luminance, the smaller of both when MaxCLL is
   above it, clamped to 400..10000 nits, else `source_peak_nits`; each value is the stream's (its
   metadata OBUs), and this one where the stream says nothing or 0: the stream wins, as for the
   assumed code points. `Rav1gFrame::display_source_peak_nits` says what a frame took.

   New in interface version 3, in place of two reserved fields.
   */
  float max_cll_nits;
  float mastering_max_nits;
  /*
   Zero.
   */
  uint32_t reserved[1];
} Rav1gDisplay;

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
   The threads that parse: the tiles of a frame and, with more than one frame in parse, the frames;
   at most 8. 0 is the default, by the frame size the decoder is created for: 1 up to 2560x1440, 4
   above.
   */
  uint32_t threads;
  /*
   Pipeline slots: frames that can be between the start of their CPU decode and the end of their GPU
   work at the same time, 2 to 4. 0 is the default: four above 2560x1440, else one more than
   frames in parse, at most four.
   Each slot has buffers of its own.
   */
  uint32_t frame_slots;
  /*
   The memory reductions: `RAV1G_LOW_MEMORY_DEFAULT`, which `rav1g_settings_default` sets, or the
   parts wanted added up, 0 for none.
   */
  uint32_t low_memory;
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
   Not used: 0. Up to 0.2.157 this was `parser`, the choice between the parse pass of rav1d
   (`RAV1G_PARSER_RAV1D`, 1, the default) and the entropy decoder of libaom (`RAV1G_PARSER_LIBAOM`,
   0). That entropy decoder is no longer in the library, the two constants are gone, and every
   decoder parses with rav1d whatever this field holds. The field keeps its place, so the fields
   behind it are where they were: an application that set `parser` deletes the assignment, one that
   did not changes nothing.
   */
  uint32_t reserved_parser;
  /*
   `RAV1G_PARSER_OPTION_*` values added up: `RAV1G_PARSER_OPTIONS_DEFAULT`
   by default; 0 for none.
   */
  uint32_t parser_options;
  /*
   Frames whose parse may run at the same time: 1 to
   `RAV1G_FRAMES_IN_PARSE_MAX`, and never more than the threads. 0 is the default: as many as the
   threads, at most `RAV1G_FRAMES_IN_PARSE_MAX` and at most the pipeline slots, for a decoder that is
   created for frames above 2560x1440 with at least 2 threads and 3 or more pipeline slots, else 1.

   With 1, `rav1g_decoder_decode` returns when the frames of its data are parsed, recorded and
   submitted. With more it returns when their parse has begun; the frames are parsed on the
   decoder's threads next to those of the next calls, and a thread of the decoder records and
   submits them in decode order. The frames and their order are the same. What an application sees
   of it: the error of a frame comes back from a later `rav1g_decoder_decode` or from
   `rav1g_decoder_drain`, and `Rav1gOutput::get_image` is called on a thread of the decoder.

   A frame in parse holds a pipeline slot, so more frames in parse want more slots
   (`frame_slots`). When `memory_available` says that the decoder with those slots would not
   leave `memory_headroom`, the decoder is created with fewer slots and frames in parse, and says
   so in "MEMORY headroom" log lines: asking for frames in parse or slots never makes a creation
   fail that would work without. `rav1g_decoder_configuration` says what was created.
   */
  uint32_t frames_in_parse;
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
   With `memory_available`: the bytes a creation should leave of what the callback returns, behind
   its device buffers, so that the frames are not slowed by the memory running out (in a console
   app with 1 GiB a 4K decoder that left 77 MiB ran at 19.5 frames a second, one that left 128 MiB
   at 45.7). 0 is the default, `RAV1G_MEMORY_GUARD_HEADROOM`; a value below
   `RAV1G_MEMORY_GUARD_MARGIN` is that margin. A decoder that would leave less is not refused: it
   is created with one slot less at a time down to 2, and at most one frame in parse less than
   slots (4 slots, then 2 frames in parse and 3 slots, then 1 frame in parse and 2 slots), the
   first that leaves the headroom, or the smallest; that one is refused only for
   `RAV1G_MEMORY_GUARD_MARGIN`. "MEMORY headroom" log lines say what was compared, and
   `rav1g_decoder_configuration` what was created. The callback has to hold back what the
   application itself still allocates for the frames that come out (see `memory_available`): the
   headroom is what stays free when all of that exists.

   New in 0.2.157: a field behind `memory_available_context`. An application that fills the
   settings with `rav1g_settings_default` only has to be compiled with this header.
   */
  uint64_t memory_headroom;
  /*
   What the output images are: `RAV1G_OUTPUT_PLANES` (0, the default) or
   `RAV1G_OUTPUT_SEMI_PLANAR`, one NV12 or P010 texture for a frame, which the output pass writes
   (docs/output.md). `Rav1gImageRequest::format` then says which of the two a frame needs.

   New in 0.2.160, with `shared_fence`: two fields behind `memory_headroom`. An application that
   fills the settings with `rav1g_settings_default` only has to be compiled with this header.
   */
  uint32_t output_kind;
  /*
   The output fence (`rav1g_decoder_output_fence`, `Rav1gFrame::d3d12_fence`) is created with
   `D3D12_FENCE_FLAG_SHARED`: the application can make a handle of it
   (`ID3D12Device::CreateSharedHandle`) and open it on another device, a D3D11 device among them
   (`ID3D11Device5::OpenSharedFence`), to wait there for the frames it reads there.
   */
  bool shared_fence;
  /*
   The output of the decoder.
   */
  struct Rav1gOutput output;
  /*
   The display pass: the images at the display size, tone-mapped to BT.709 SDR. Zero (what
   `rav1g_settings_default` sets) is none. New in interface version 2, behind `output`.
   */
  struct Rav1gDisplay display;
  /*
   What `rav1g_peek_stream_info` says of the temporal unit with the key frame the decoder starts
   with. Its bit depth, colour, film grain and super-resolution say which pipeline states the
   creation compiles when the device has not prepared them (`Rav1gDeviceSettings::prepare`;
   docs/integration.md, "Preparing the pipeline states"); the scaled-reference states of its depth
   are compiled in the background behind it. A `bit_depth` of 0 (what `rav1g_settings_default` sets)
   is not known: the creation then waits for every state the other settings allow (both bit depths
   when `bit_depth` is above 8, the three display modes, film grain, super-resolution). The frames
   are the same either way. New in interface version 4, behind `display`.
   */
  struct Rav1gStreamInfo stream;
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
   Bytes of the device buffers the creation allocates: those of the decoder the settings ask for.
   The decoder that is created may have fewer slots and take less (`Rav1gSettings::frames_in_parse`,
   `rav1g_decoder_configuration`).
   */
  size_t device_size;
  /*
   Bytes of the device buffers that are only created when a stream needs them: one palette buffer per
   pipeline slot, and what the buffers of the stages that are created with a quarter of their worst case
   grow by on a frame that writes more blocks.
   */
  size_t device_size_deferred;
} Rav1gMemoryRequirements;

/*
 What a decoder was created with, where its settings left the choice to it
 (`rav1g_decoder_configuration`).
 */
typedef struct Rav1gConfiguration {
  /*
   The threads that parse (`Rav1gSettings::threads`).
   */
  uint32_t threads;
  /*
   The frames whose parse may run at the same time (`Rav1gSettings::frames_in_parse`): what the
   settings asked for or the default, unless the budget guard found that the decoder would not
   leave the headroom, and then fewer (`Rav1gSettings::memory_headroom`).
   */
  uint32_t frames_in_parse;
  /*
   The pipeline slots (`Rav1gSettings::frame_slots`), or fewer for the same reason.
   */
  uint32_t frame_slots;
  /*
   The bytes of the device buffers the creation allocated.
   */
  size_t device_size;
} Rav1gConfiguration;

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
 `RAV1G_ABI_VERSION` of the `rav1g.h` the library was built with. A loader compares it with the
 constant of the header it was compiled with before it uses anything else of the library.
 */
uint32_t rav1g_abi_version(void);

/*
 The version of the library and what it was built with, for a log:
 "rav1g 0.4.0, the Rust host; entropy decoder: MSVC, /GL, DLL runtime; shaders: DXC 1.9.2602.17 cs_6_0".
 */
const char *rav1g_version(void);

/*
 Reads the sequence header of a temporal unit, without a decoder.

 # Safety
 `data` is valid for `size` bytes; `info` is valid for writing.
 */
enum Rav1gStatus rav1g_peek_stream_info(const uint8_t *data, size_t size, struct Rav1gStreamInfo *info);

/*
 The colour of a frame, or of a stream, as one line for a log:

 "primaries 9 (BT.2020) transfer 16 (PQ) matrix 9 (BT.2020 NCL) range limited chroma pos 2 (colocated);
 MDCV R 0.6800,0.3200 G 0.2650,0.6900 B 0.1500,0.0600 W 0.3127,0.3290 L 0.0050..1000 cd/m2 [44564,20972
 17367,45220 9830,3932 20493,21561 256000 82]; CLL 1000/400"

 The code points with their names, ", no colour description" behind them when the header has none;
 the mastering display in its units (x,y of red, green, blue and the white point, the smallest and the
 largest luminance) and, in brackets, as the numbers of the OBU; MaxCLL/MaxFALL. "no MDCV" and "no CLL"
 for a null pointer. Writes the text with its terminating zero into `text`, cut to `size` bytes, and
 returns the length of the whole text without the zero.

 # Safety
 `color` is valid; `mastering_display` and `content_light` are null or valid; `text` is valid for
 writing `size` bytes, or `size` is 0.
 */
size_t rav1g_color_text(const struct Rav1gColorInfo *color,
                        const struct Rav1gMasteringDisplay *mastering_display,
                        const struct Rav1gContentLightLevel *content_light,
                        char *text,
                        size_t size);

/*
 A device for decoders. `d3d12_device` is the application's `ID3D12Device *`, or null for a device of
 the decoder's own on the first hardware adapter; `d3d12_queue` is the direct `ID3D12CommandQueue *` the
 decoders submit to, or null for a queue of the decoder's own. Every compute pipeline state of the
 decoder is created, on `pipeline_threads` threads (6 at most) while the call returns
 (`rav1g_device_wait_pipelines`), or by this call with 0: `rav1g_device_create_with_settings` with
 `RAV1G_PREPARE_ALL`. Null when the device, the queue or a pipeline layout could not be created, or the
 device does not support Shader Model 6.0 (`rav1g_last_error`).

 # Safety
 The pointers are null or valid interfaces of the kinds named.
 */
struct Rav1gDevice *rav1g_device_create(void *d3d12_device, void *d3d12_queue, uint32_t pipeline_threads);

/*
 `rav1g_device_create` with the pipeline states of `settings` prepared. Null as there, and for a
 `prepare` with a bit that is not a `RAV1G_PREPARE_*` or a nonzero reserved field
 (`rav1g_last_error`).

 # Safety
 The pointers are null or valid interfaces of the kinds named; `settings` is valid.
 */
struct Rav1gDevice *rav1g_device_create_with_settings(void *d3d12_device,
                                                      void *d3d12_queue,
                                                      const struct Rav1gDeviceSettings *settings);

/*
 Waits until every pipeline state the device prepared exists (`Rav1gDeviceSettings::prepare`; all of
 them for a device of `rav1g_device_create`); the calling thread compiles those no thread has begun.
 The first decoder creation waits for the states its stream needs. An error when one could not be
 created; the log names it.

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
 The defaults: a decoder for 8-bit 1920x1088 frames with the shared back buffer, the parse pass of
 rav1d as its parser (with `RAV1G_PARSER_OPTIONS_DEFAULT`), no budget guard, and
 no output (the application sets `output`). Its threads, frames in parse and pipeline slots are 0, the
 defaults for the frame size the application then sets: one thread, one frame in parse and two slots
 up to 2560x1440; above, two threads and, where the slots fit, two frames in parse with four slots.

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

 With more than one frame in parse (`Rav1gSettings::frames_in_parse`) the call returns when the parse
 of the unit's frames has begun, and it waits when as many frames are in parse as may be. An error is
 then that of this unit (a header that cannot be read, a frame whose references failed) or that of
 a frame of an earlier call whose parse or submission failed since (when this unit has no error of its own): one error a call, the oldest first.

 A start in the middle of a stream (AV1 7.6, random access): give the decoder the sequence header first
 (the codec data of the container, Matroska's av1C configOBUs, in front of the first temporal unit, or
 in it). A frame that names a reference slot no frame has been decoded into since the decoder was
 created (or since a new sequence, or a `rav1g_decoder_flush`), a `show_existing_frame` of such a
 slot, and a frame before the first sequence header are skipped: not decoded, not output, and no
 error (the call returns `Rav1gStatus::Ok` for them). The output starts with the first frame whose references were all
 decoded, and is right from there on. The skipped frames are counted in a log line that is not an
 error (`rav1g_set_log_callback`), "SKIPPED <n> frames ...", when the next frame is decoded or shown,
 at a drain or a flush and when the decoder is destroyed (docs/integration.md).

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
 Waits for the frames in flight: every frame whose parse has begun is parsed and submitted, every frame
 that was submitted has been handed to the output, then `Rav1gOutput::end_of_frames` is called. The
 decoder can go on decoding afterwards.

 With one frame in parse the result is `Rav1gStatus::Ok`. With more
 (`Rav1gSettings::frames_in_parse`) it is the error of a frame that was still in parse when the last
 `rav1g_decoder_decode` returned, if one failed (`rav1g_decoder_error`, `rav1g_decoder_error_detail`):
 one error a call, so an application that counts them calls this until it returns `Rav1gStatus::Ok`.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
 */
enum Rav1gStatus rav1g_decoder_drain(struct Rav1gDecoder *decoder);

/*
 Forgets the stream, for a seek or any other break in the input (AV1 7.5, 7.6: the decode goes on at a
 random access point). Called between two calls of the decoder: right after it was created, after a
 drain, with frames in flight, twice in a row. New in interface version 4.

 Dropped, and never handed over: every frame in parse (the parse threads stop at their next
 superblock), every frame that waits for its submission (not recorded, no image asked for), and every
 frame on the GPU, whose image comes back through `Rav1gOutput::unused` when the GPU is through with
 it, on the decoder's completion thread. Once the call has returned, no frame that was given to the
 decoder before it reaches `Rav1gOutput::frame`. `Rav1gOutput::end_of_frames` is not called: the
 return is the end, and nothing was handed over.

 Emptied: the eight reference slots. No frame has been decoded into any of them (the spec's
 RefValid), and their frame buffers go back to the pool (that of a frame on the GPU when it is
 through). The frames up to the next random access point are skipped as at a start in the middle of a
 stream (`rav1g_decoder_decode`). Kept: the sequence header and the HDR metadata that goes with it,
 the device buffers, the pipeline states, the threads and the settings. So after a flush the
 application passes the temporal units from the new place on, from a key frame or another random
 access point; the sequence header in front (the container's configOBUs) is not needed, and does no
 harm (docs/integration.md).

 The result is `Rav1gStatus::Ok`: the errors of the dropped frames are not returned. An error only for
 a decoder that stopped with a panic in an earlier call (it decodes nothing any more). Nothing is
 allocated. The call waits for the parse threads (the rest of a superblock each), for the frame the
 decoder's submission thread records when it begins (with more than one frame in parse: its recording,
 which may wait for a frame buffer a frame on the GPU holds, and its `get_image`, which the application
 answers), and for a `Rav1gOutput::frame` call in progress; not for the GPU work of the frames in
 flight. A log line that is not an error ("FLUSH <n>: ...") has the time of each part.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time.
 */
enum Rav1gStatus rav1g_decoder_flush(struct Rav1gDecoder *decoder);

/*
 What the decoder was created with: the threads, the frames in parse and the pipeline slots, with the
 defaults of the settings resolved and after the budget guard's choice, and the bytes of its device
 buffers.

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`, used by one thread at a time; `configuration` is
 valid for writing.
 */
void rav1g_decoder_configuration(struct Rav1gDecoder *decoder, struct Rav1gConfiguration *configuration);

/*
 The output fence of the decoder, an `ID3D12Fence *`; no reference is added, and it lives as long as
 the decoder. The queue signals it behind every command list of the decoder, so its value says which
 frames are written: `Rav1gFrame::fence_value` is the value of a frame, and the values grow from
 frame to frame. It is the fence every `Rav1gFrame::d3d12_fence` of the decoder points to; this call
 has it before the first frame, for an application that opens it on another device
 (`Rav1gSettings::shared_fence`).

 # Safety
 `decoder` is a decoder of `rav1g_decoder_create`.
 */
void *rav1g_decoder_output_fence(const struct Rav1gDecoder *decoder);

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
 Experiment (a library whose `rav1g_shader_variant` is "shader-pick"): which bytecode the pipeline
 state of a shader is made of, in the devices created after the call. The build has two sets.
 `dxc` is the default build's DXC bytecode, also used without a pick.
 `od` uses DXC with `-Od` where supported.
 In a `pick` of the form "od=<list>,dxc=<list>", a list is shader names (the names of
 the `.hlsl` files) separated by commas, `all`, or names with `*` for any characters; a later part
 overrides an earlier one ("od=all,dxc=loopfilter_vert_hbd"). Null or an empty text is no pick. The
 device logs one "SHADERSET" line per bytecode set and a "SHADERPICK" line with the shaders of each.
 `RAV1G_STATUS_INVALID_PARAM` (and the reason in `rav1g_last_error`) for a text that is not a pick, a
 name no shader has, and any pick in a library that is not that build; the pick then stays what it was.

 # Safety
 `pick` is null or a C string.
 */
enum Rav1gStatus rav1g_set_shader_pick(const char *pick);

/*
 Debugging: the decoders created after the call fill the mode info pool (`RAV1G_POISON_MODE_INFO`) or
 every buffer the CPU writes for the shaders (`RAV1G_POISON_BUFFERS`) with the byte 0xA5 when they
 create it; 0 switches it off. The output must be the same.
 */
void rav1g_set_poison_buffers(uint32_t level);

/*
 The memory statistics of the test programs (docs/memory.md): 0 off; 1: a decoder logs a block of
 lines that start with "MEMSTATS " when it has drained (`rav1g_decoder_drain` without an error) or is
 destroyed, about the frames since the last block; 2: also a line for every frame. The output is the
 same.
 */
void rav1g_set_memory_stats(uint32_t level);

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
 An empty string for the normal build of the shaders, "uavsync" and "shader-pick" for the experiment builds
 (what compiled the shaders is in the text of `rav1g_version`).
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

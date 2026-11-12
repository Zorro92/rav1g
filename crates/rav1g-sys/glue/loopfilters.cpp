/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The functions of libav1/dx/loopfilters.cpp for the C++ that remains in the cargo build: calls of the Rust
// that has what they do (crates/rav1g/src/host/loopfilter.rs, postfilter.rs, film_grain.rs, output.rs).
// av1_set_lf_h_split and av1_set_cdef_v2 are Rust functions of those names.

#include "dx/types.h"
#include "dx/av1_core.h"

extern "C" {
void rav1g_loopfilter_gpu(Av1Core *dec, AV1_COMMON *cm, MACROBLOCKD *xd);
void rav1g_cdef_looprestoration(Av1Core *dec, AV1_COMMON *cm, void *lr_ctxt);
int rav1g_output_image(Av1Core *dec, AV1_COMMON *cm, av1_decoded_frame_buffer_t *fb);
void rav1g_copy_output(Av1Core *dec, AV1_COMMON *cm, const av1_decoded_frame_buffer_t *fb);
}

void av1_loopfilter_gpu(Av1Core *dec, AV1_COMMON *cm, MACROBLOCKD *xd) { rav1g_loopfilter_gpu(dec, cm, xd); }

void av1_cdef_looprestoration(Av1Core *dec, AV1_COMMON *cm, void *lr_ctxt) {
  rav1g_cdef_looprestoration(dec, cm, lr_ctxt);
}

// The output of a frame is two calls of the Rust. Between them, when the application gave no output image,
// the error of the decoder is raised here: aom_internal_error leaves the decode call with a longjmp when the
// caller has set one up, and a longjmp must not pass a Rust function. When it returns, the frame goes on
// without an image, as it did in av1_postprocess_copy_output of loopfilters.cpp.
int av1_postprocess_copy_output(Av1Core *dec, AV1_COMMON *cm) {
  av1_decoded_frame_buffer_t fb = {0};
  const int state = rav1g_output_image(dec, cm, &fb);
  if (!state) return 0;
  if (state == 2) aom_internal_error(&cm->error, AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
  rav1g_copy_output(dec, cm, &fb);
  return 0;
}

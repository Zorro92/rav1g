/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/* The entropy decoder as the Rust host sees it: bindgen reads this as C (build.rs) for the module `parser`
 * of the bindings, and the layout probe includes it. The host reads the header of a frame and the state of
 * the tile threads in place (AV1Decoder, AV1_COMMON, MACROBLOCKD, ThreadData), fills the frame buffer
 * descriptions the entropy decoder keeps (YV12_BUFFER_CONFIG, HwFrameBuffer), and reads a few of its
 * tables. */
#ifndef RAV1G_SYS_PARSER_H_
#define RAV1G_SYS_PARSER_H_

#include "dx/av1_core.h"

#include "av1/common/av1_loopfilter.h"
#include "av1/common/reconinter.h"
#include "av1/common/reconintra.h"
#include "av1/common/restoration.h"
#include "av1/common/scale.h"
#include "av1/common/scan.h"
#include "av1/common/warped_motion.h"

#endif /* RAV1G_SYS_PARSER_H_ */

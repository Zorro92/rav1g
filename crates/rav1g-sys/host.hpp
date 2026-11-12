/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/* The C++ host as the Rust host modules see it: bindgen reads this as C++ (build.rs) for the module `host`
 * of the bindings, and the layout probe of the C++ host includes it. The headers are those the C++ files
 * of libav1/dx include. */
#ifndef RAV1G_SYS_HOST_HPP_
#define RAV1G_SYS_HOST_HPP_

#include "dx/types.h"
#include "dx/av1_core.h"
#include "dx/av1_memory.h"
#include "dx/av1_compute.h"
#include "dx/av1_thread.h"

/* Tables of the entropy decoder the host reads. */
extern "C" {
#include "av1/common/reconinter.h"
#include "av1/common/warped_motion.h"
}

#endif /* RAV1G_SYS_HOST_HPP_ */

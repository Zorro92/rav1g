/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The function of libav1/dx/transform.cpp for the C++ that remains in the cargo build: a call of the Rust
// that has what it does (crates/rav1g/src/host/transform.rs).

#include "dx/types.h"

extern "C" void rav1g_idct_run(Av1Core *dec);

void av1_idct_run(Av1Core *dec) { rav1g_idct_run(dec); }

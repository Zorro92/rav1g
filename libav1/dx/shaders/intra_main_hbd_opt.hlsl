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

// intra_main_hbd.hlsl, unchanged, built with FXC optimisation enabled (test player flag -intraopt).
// The project compiles intra_main_hbd.hlsl itself with optimisation disabled; this file only exists
// so that the same source gets a second FxCompile item with the project's default settings.
#include "intra_main_hbd.hlsl"

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

#pragma once

#include <inttypes.h>
#include <cstring>

enum { MaxChuncks = 256, MaxAllocations = 256, BlockAlignment = 65536 };

struct AllocItem {
  size_t offset;
  size_t size;
};

struct LinearAllocator {
  AllocItem free_mem_chuncks[MaxChuncks];
  AllocItem allocated_items[MaxAllocations];

  int free_chunks_count;
  int allocated_item_count;

  uint8_t* base_ptr;
};

void la_init(LinearAllocator* allocator, uint8_t* ptr, size_t size);
uint8_t* la_allocate(LinearAllocator* allocator, size_t size);
void la_free(LinearAllocator* allocator, uint8_t* ptr);

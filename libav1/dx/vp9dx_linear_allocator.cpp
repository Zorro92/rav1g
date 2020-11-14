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

#include "dx/vp9dx_linear_allocator.h"

static void remove_item(AllocItem* items, int items_count, int pos) {
  for (int i = pos + 1; i < items_count; ++i) {
    items[i - 1] = items[i];
  }
}

void la_init(LinearAllocator* allocator, uint8_t* ptr, size_t size) {
  allocator->base_ptr = ptr;
  allocator->free_chunks_count = 1;
  allocator->allocated_item_count = 0;
  allocator->free_mem_chuncks[0].offset = 0;
  allocator->free_mem_chuncks[0].size = size;
}

uint8_t* la_allocate(LinearAllocator* allocator, size_t size) {
  size = (size + BlockAlignment - 1) & (~(BlockAlignment - 1));

  int chunk = -1;
  for (int i = 0; i < allocator->free_chunks_count; ++i) {
    if (allocator->free_mem_chuncks[i].size >= size) {
      chunk = i;
      break;
    }
  }

  if (chunk == -1) return NULL;

  size_t offset = allocator->free_mem_chuncks[chunk].offset;
  allocator->free_mem_chuncks[chunk].offset += size;
  allocator->free_mem_chuncks[chunk].size -= size;

  if (allocator->free_mem_chuncks[chunk].size == 0) {
    remove_item(allocator->free_mem_chuncks, allocator->free_chunks_count, chunk);
    --allocator->free_chunks_count;
  }

  uint8_t* ptr = allocator->base_ptr + offset;

  allocator->allocated_items[allocator->allocated_item_count].size = size;
  allocator->allocated_items[allocator->allocated_item_count].offset = offset;

  ++allocator->allocated_item_count;
  return ptr;
}

void la_free(LinearAllocator* allocator, uint8_t* ptr) {
  size_t offset = (size_t)(ptr - allocator->base_ptr);
  int index = -1;
  for (int i = 0; i < allocator->allocated_item_count; ++i) {
    if (allocator->allocated_items[i].offset == offset) {
      index = i;
      break;
    }
  }

  if (index == -1) {
    return;
  }
  size_t size = allocator->allocated_items[index].size;

  remove_item(allocator->allocated_items, allocator->allocated_item_count, index);
  --allocator->allocated_item_count;

  int next_chunk = -1;
  for (int i = 0; i < allocator->free_chunks_count; ++i) {
    if (allocator->free_mem_chuncks[i].offset > offset) {
      next_chunk = i;
      break;
    }
  }

  int prev_chunk = (next_chunk == -1) ? allocator->free_chunks_count - 1 : next_chunk - 1;

  // add new memory chunk and/or merge with existing chunks
  if (prev_chunk >= 0 &&
      (allocator->free_mem_chuncks[prev_chunk].offset + allocator->free_mem_chuncks[prev_chunk].size) == offset) {
    allocator->free_mem_chuncks[prev_chunk].size += size;
    const size_t chunk_end =
        allocator->free_mem_chuncks[prev_chunk].offset + allocator->free_mem_chuncks[prev_chunk].size;
    if (prev_chunk < (allocator->free_chunks_count - 1) &&
        chunk_end == allocator->free_mem_chuncks[prev_chunk + 1].offset) {
      allocator->free_mem_chuncks[prev_chunk].size += allocator->free_mem_chuncks[prev_chunk + 1].size;
      remove_item(allocator->free_mem_chuncks, allocator->free_chunks_count, prev_chunk + 1);
      --allocator->free_chunks_count;
    }
  } else {
    if (next_chunk != -1 && (offset + size) == allocator->free_mem_chuncks[next_chunk].offset) {
      allocator->free_mem_chuncks[next_chunk].offset = offset;
      allocator->free_mem_chuncks[next_chunk].size += size;
    } else {
      int pos = allocator->free_chunks_count;
      if (next_chunk != -1) {
        pos = next_chunk;
        for (int i = allocator->free_chunks_count; i > next_chunk; --i) {
          allocator->free_mem_chuncks[i] = allocator->free_mem_chuncks[i - 1];
        }
      }
      allocator->free_mem_chuncks[pos].offset = offset;
      allocator->free_mem_chuncks[pos].size = size;
      ++allocator->free_chunks_count;
    }
  }
}

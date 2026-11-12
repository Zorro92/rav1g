/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The C++ functions of libav1/dx/av1_memory.cpp for the C++ that remains in the cargo build. Rust has what
// they do (crates/rav1g/src/host/log.rs, memory.rs); here each is a call of it. The two functions with a
// printf format print the line here and pass it on. The extern "C" functions of the file
// (av1_set_log_callback, av1_last_create_error, av1_log_error, av1_log_video_memory_now,
// av1_set_poison_buffers) are Rust functions of those names.

#include "dx/av1_memory.h"
#include <stdarg.h>
#include <stdio.h>

extern "C" {
void rav1g_log_line(int is_error, const char *line);
void rav1g_log_create_error_line(const char *line);
void rav1g_clear_create_error(void);
void rav1g_poison_constants(void *ptr, size_t size);
const char *rav1g_memory_type_name(int mem);
void rav1g_query_video_memory(void *device, av1_video_memory *info);
void rav1g_log_video_memory(int is_error, const char *label, const av1_video_memory *info);
void rav1g_memory_setup(av1_memory_allocator *mem, uint8_t *host_mem, size_t host_size);
size_t rav1g_memory_device_bytes(const av1_memory_allocator *mem);
GpuBufferObject *rav1g_memory_create_buffer(av1_memory_allocator *mem, size_t size, int type, const char *name,
                                            int slot);
void *rav1g_memory_host_allocate(av1_memory_allocator *mem, size_t size, int align);
size_t rav1g_memory_release_type(av1_memory_allocator *mem, int type, int *count);
void rav1g_memory_release(av1_memory_allocator *mem);
void rav1g_memory_dummy_create_buffer(av1_memory_allocator_dummy *mem, size_t size);
void rav1g_memory_dummy_host_allocate(av1_memory_allocator_dummy *mem, size_t size, int align);
}

void av1_log(int is_error, const char *format, ...) {
  char line[768];
  va_list args;
  va_start(args, format);
  vsnprintf(line, sizeof(line), format, args);
  va_end(args);
  line[sizeof(line) - 1] = 0;
  rav1g_log_line(is_error, line);
}

void av1_log_create_error(const char *format, ...) {
  char line[512];
  va_list args;
  va_start(args, format);
  vsnprintf(line, sizeof(line), format, args);
  va_end(args);
  line[sizeof(line) - 1] = 0;
  rav1g_log_create_error_line(line);
}

void av1_clear_create_error() { rav1g_clear_create_error(); }

void av1_poison_constants(void *ptr, size_t size) { rav1g_poison_constants(ptr, size); }

const char *av1_memory_type_name(MemoryType mem) { return rav1g_memory_type_name(mem); }

void av1_query_video_memory(ID3D12Device *device, av1_video_memory *info) { rav1g_query_video_memory(device, info); }

void av1_log_video_memory(int is_error, const char *label, const av1_video_memory *info) {
  rav1g_log_video_memory(is_error, label, info);
}

void av1_memory_allocator::setup(uint8_t *host_mem, size_t host_size) { rav1g_memory_setup(this, host_mem, host_size); }

size_t av1_memory_allocator::device_bytes() const { return rav1g_memory_device_bytes(this); }

GpuBufferObject *av1_memory_allocator::create_buffer(size_t size, MemoryType mem, const char *name, int slot) {
  return rav1g_memory_create_buffer(this, size, mem, name, slot);
}

void *av1_memory_allocator::host_allocate(size_t size, int align) {
  return rav1g_memory_host_allocate(this, size, align);
}

size_t av1_memory_allocator::release_type(MemoryType mem, int *count) {
  return rav1g_memory_release_type(this, mem, count);
}

void av1_memory_allocator::release() { rav1g_memory_release(this); }

GpuBufferObject *av1_memory_allocator_dummy::create_buffer(size_t size, MemoryType mem, const char *name, int slot) {
  rav1g_memory_dummy_create_buffer(this, size);
  return NULL;
}

void *av1_memory_allocator_dummy::host_allocate(size_t size, int align) {
  rav1g_memory_dummy_host_allocate(this, size, align);
  return NULL;
}

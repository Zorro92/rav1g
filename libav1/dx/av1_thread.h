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
#include "aom_util/aom_thread.h"
#define BUFFERS_COUNT 100

typedef struct {
  void* m_element[BUFFERS_COUNT];
  int m_head;
  int m_tail;
  int m_QueueNotEmpty;
} DataPtrQueue;

typedef struct {
  DataPtrQueue int_queue;
  pthread_cond_t m_empty_cond;  // event
  pthread_mutex_t m_mutex;
} DataPtrQueueMT;

void QueueInit(DataPtrQueue* queue);
void QueuePop(DataPtrQueue* queue);
void QueuePush(DataPtrQueue* queue, void* item);
void* QueueFront(DataPtrQueue* queue);
void* QueueGet(DataPtrQueue* queue);
int QueueIsEmpty(DataPtrQueue* queue);

void MTQueueInit(DataPtrQueueMT* queue);
void MTQueuePush(DataPtrQueueMT* queue, void* buffer);
void* MTQueueGet(DataPtrQueueMT* queue);
void MTQueuePushSafe(DataPtrQueueMT* queue, void* buffer);
void* MTQueueGetSafe(DataPtrQueueMT* queue);
int MTQueueIsEmpty(DataPtrQueueMT* queue);
void MTQueueDestroy(DataPtrQueueMT* queue);
void* MTQueueFront(DataPtrQueueMT* queue);
void MTQueuePop(DataPtrQueueMT* queue);
void MTQueueClear(DataPtrQueueMT* queue);
int MTQueueGetCount(DataPtrQueueMT* queue);

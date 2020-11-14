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

#include "dx/av1_thread.h"

void QueueInit(DataPtrQueue* queue) {
  queue->m_head = queue->m_tail = 0;
  queue->m_QueueNotEmpty = 0;
}

void QueuePop(DataPtrQueue* queue) {
  if (queue->m_tail == queue->m_head) return;

  queue->m_element[queue->m_tail % BUFFERS_COUNT] = NULL;
  queue->m_tail++;
  if (queue->m_tail == queue->m_head)  // its empty now
  {
    queue->m_tail = queue->m_head = 0;
    queue->m_QueueNotEmpty = 0;
  }
}

void QueuePush(DataPtrQueue* queue, void* item) {
  queue->m_element[queue->m_head % BUFFERS_COUNT] = item;
  queue->m_head++;
  queue->m_QueueNotEmpty = 1;
}

void* QueueFront(DataPtrQueue* queue) {
  if (QueueIsEmpty(queue)) return NULL;
  return queue->m_element[queue->m_tail % BUFFERS_COUNT];
}

void* QueueGet(DataPtrQueue* queue) {
  void* item = NULL;
  if (QueueIsEmpty(queue)) return NULL;
  item = queue->m_element[queue->m_tail % BUFFERS_COUNT];
  QueuePop(queue);
  return item;
}

int QueueIsEmpty(DataPtrQueue* queue) { return queue->m_head == queue->m_tail; }

void MTQueueInit(DataPtrQueueMT* queue) {
  QueueInit(&queue->int_queue);
  pthread_cond_init(&queue->m_empty_cond, NULL);
  pthread_mutex_init(&queue->m_mutex, NULL);
}

void MTQueueDestroy(DataPtrQueueMT* queue) {
  pthread_mutex_destroy(&queue->m_mutex);
  pthread_cond_destroy(&queue->m_empty_cond);
}

void MTQueuePush(DataPtrQueueMT* queue, void* item) {
  pthread_mutex_lock(&queue->m_mutex);  // cs.Lock();

  QueuePush(&queue->int_queue, item);
  pthread_cond_signal(&queue->m_empty_cond);

  pthread_mutex_unlock(&queue->m_mutex);
}

void* MTQueueGet(DataPtrQueueMT* queue) {
  void* frontElement = NULL;
  pthread_mutex_lock(&queue->m_mutex);
  {
    while (!queue->int_queue.m_QueueNotEmpty) pthread_cond_wait(&queue->m_empty_cond, &queue->m_mutex);

    frontElement = QueueGet(&queue->int_queue);
  }
  pthread_mutex_unlock(&queue->m_mutex);

  return frontElement;
}

void MTQueuePushSafe(DataPtrQueueMT* queue, void* item) {
  pthread_mutex_lock(&queue->m_mutex);  // cs.Lock();

  while (queue->int_queue.m_head - queue->int_queue.m_tail >= BUFFERS_COUNT)
    pthread_cond_wait(&queue->m_empty_cond, &queue->m_mutex);

  QueuePush(&queue->int_queue, item);
  pthread_cond_signal(&queue->m_empty_cond);

  pthread_mutex_unlock(&queue->m_mutex);
}

void* MTQueueGetSafe(DataPtrQueueMT* queue) {
  void* frontElement = NULL;
  pthread_mutex_lock(&queue->m_mutex);
  {
    while (!queue->int_queue.m_QueueNotEmpty) pthread_cond_wait(&queue->m_empty_cond, &queue->m_mutex);

    frontElement = QueueGet(&queue->int_queue);
    pthread_cond_signal(&queue->m_empty_cond);
  }
  pthread_mutex_unlock(&queue->m_mutex);

  return frontElement;
}

int MTQueueIsEmpty(DataPtrQueueMT* queue) {
  int is_empty = 0;
  pthread_mutex_lock(&queue->m_mutex);
  is_empty = QueueIsEmpty(&queue->int_queue);
  pthread_mutex_unlock(&queue->m_mutex);
  return is_empty;
}

void* MTQueueFront(DataPtrQueueMT* queue) {
  void* ret = NULL;
  pthread_mutex_lock(&queue->m_mutex);
  ret = QueueFront(&queue->int_queue);
  pthread_mutex_unlock(&queue->m_mutex);
  return ret;
}

void MTQueuePop(DataPtrQueueMT* queue) {
  pthread_mutex_lock(&queue->m_mutex);
  QueuePop(&queue->int_queue);
  pthread_mutex_unlock(&queue->m_mutex);
}

void MTQueueClear(DataPtrQueueMT* queue) {
  pthread_mutex_lock(&queue->m_mutex);
  queue->int_queue.m_head = queue->int_queue.m_tail = 0;
  queue->int_queue.m_QueueNotEmpty = 0;
  pthread_mutex_unlock(&queue->m_mutex);
}

int MTQueueGetCount(DataPtrQueueMT* queue) {
  int ret = 0;
  pthread_mutex_lock(&queue->m_mutex);
  ret = queue->int_queue.m_head - queue->int_queue.m_tail;
  pthread_mutex_unlock(&queue->m_mutex);
  return ret;
}

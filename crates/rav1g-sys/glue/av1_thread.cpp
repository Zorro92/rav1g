/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// The functions of libav1/dx/av1_thread.cpp for the C++ that remains in the cargo build: each is a call of
// the Rust that has what it does (crates/rav1g/src/host/thread.rs).

#include "dx/av1_thread.h"

extern "C" {
void rav1g_queue_init(DataPtrQueue *queue);
void rav1g_queue_pop(DataPtrQueue *queue);
void rav1g_queue_push(DataPtrQueue *queue, void *item);
void *rav1g_queue_front(DataPtrQueue *queue);
void *rav1g_queue_get(DataPtrQueue *queue);
int rav1g_queue_is_empty(DataPtrQueue *queue);
void rav1g_mt_queue_init(DataPtrQueueMT *queue);
void rav1g_mt_queue_destroy(DataPtrQueueMT *queue);
void rav1g_mt_queue_push(DataPtrQueueMT *queue, void *item);
void *rav1g_mt_queue_get(DataPtrQueueMT *queue);
void rav1g_mt_queue_push_safe(DataPtrQueueMT *queue, void *item);
void *rav1g_mt_queue_get_safe(DataPtrQueueMT *queue);
int rav1g_mt_queue_is_empty(DataPtrQueueMT *queue);
void *rav1g_mt_queue_front(DataPtrQueueMT *queue);
void rav1g_mt_queue_pop(DataPtrQueueMT *queue);
void rav1g_mt_queue_clear(DataPtrQueueMT *queue);
int rav1g_mt_queue_get_count(DataPtrQueueMT *queue);
}

void QueueInit(DataPtrQueue *queue) { rav1g_queue_init(queue); }
void QueuePop(DataPtrQueue *queue) { rav1g_queue_pop(queue); }
void QueuePush(DataPtrQueue *queue, void *item) { rav1g_queue_push(queue, item); }
void *QueueFront(DataPtrQueue *queue) { return rav1g_queue_front(queue); }
void *QueueGet(DataPtrQueue *queue) { return rav1g_queue_get(queue); }
int QueueIsEmpty(DataPtrQueue *queue) { return rav1g_queue_is_empty(queue); }

void MTQueueInit(DataPtrQueueMT *queue) { rav1g_mt_queue_init(queue); }
void MTQueueDestroy(DataPtrQueueMT *queue) { rav1g_mt_queue_destroy(queue); }
void MTQueuePush(DataPtrQueueMT *queue, void *item) { rav1g_mt_queue_push(queue, item); }
void *MTQueueGet(DataPtrQueueMT *queue) { return rav1g_mt_queue_get(queue); }
void MTQueuePushSafe(DataPtrQueueMT *queue, void *item) { rav1g_mt_queue_push_safe(queue, item); }
void *MTQueueGetSafe(DataPtrQueueMT *queue) { return rav1g_mt_queue_get_safe(queue); }
int MTQueueIsEmpty(DataPtrQueueMT *queue) { return rav1g_mt_queue_is_empty(queue); }
void *MTQueueFront(DataPtrQueueMT *queue) { return rav1g_mt_queue_front(queue); }
void MTQueuePop(DataPtrQueueMT *queue) { rav1g_mt_queue_pop(queue); }
void MTQueueClear(DataPtrQueueMT *queue) { rav1g_mt_queue_clear(queue); }
int MTQueueGetCount(DataPtrQueueMT *queue) { return rav1g_mt_queue_get_count(queue); }

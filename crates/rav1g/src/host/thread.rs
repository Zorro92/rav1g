// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The queues of the host: a ring of pointers, and the same behind a lock with a condition for waiting.
//! From `libav1/dx/av1_thread.cpp`.
//!
//! The queues are the C++ structs `DataPtrQueue` and `DataPtrQueueMT`, which sit inside `Av1Core` and the
//! shader library and which the frame pipeline also locks itself. So the lock stays what `aom_thread.h`
//! makes of a pthread mutex and condition on Windows, a critical section and a condition variable inside
//! the struct, and the functions here take and wake them the way that header does.

use std::ffi::{c_int, c_void};

use windows::Win32::System::Threading::{
    CONDITION_VARIABLE, CRITICAL_SECTION, DeleteCriticalSection, EnterCriticalSection, INFINITE,
    InitializeConditionVariable, InitializeCriticalSectionEx, LeaveCriticalSection, SleepConditionVariableCS,
    WakeConditionVariable,
};

use crate::sys::host::{BUFFERS_COUNT, DataPtrQueue, DataPtrQueueMT};

const CAPACITY: c_int = BUFFERS_COUNT as c_int;

// The lock and the condition of the C++ struct are the ones of Windows.
const _: () = {
    assert!(size_of::<crate::sys::host::pthread_mutex_t>() == size_of::<CRITICAL_SECTION>());
    assert!(size_of::<crate::sys::host::pthread_cond_t>() == size_of::<CONDITION_VARIABLE>());
};

fn slot(index: c_int) -> usize {
    index.rem_euclid(CAPACITY) as usize
}

fn init(queue: &mut DataPtrQueue) {
    queue.m_head = 0;
    queue.m_tail = 0;
    queue.m_QueueNotEmpty = 0;
}

fn is_empty(queue: &DataPtrQueue) -> bool {
    queue.m_head == queue.m_tail
}

fn pop(queue: &mut DataPtrQueue) {
    if queue.m_tail == queue.m_head {
        return;
    }
    queue.m_element[slot(queue.m_tail)] = std::ptr::null_mut();
    queue.m_tail += 1;
    if queue.m_tail == queue.m_head {
        // Empty now.
        queue.m_tail = 0;
        queue.m_head = 0;
        queue.m_QueueNotEmpty = 0;
    }
}

fn push(queue: &mut DataPtrQueue, item: *mut c_void) {
    queue.m_element[slot(queue.m_head)] = item;
    queue.m_head += 1;
    queue.m_QueueNotEmpty = 1;
}

fn front(queue: &DataPtrQueue) -> *mut c_void {
    if is_empty(queue) { std::ptr::null_mut() } else { queue.m_element[slot(queue.m_tail)] }
}

fn get(queue: &mut DataPtrQueue) -> *mut c_void {
    let item = front(queue);
    if !is_empty(queue) {
        pop(queue);
    }
    item
}

/// `QueueInit`.
///
/// # Safety
/// `queue` is valid for writing, as for every function of this file that takes one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_queue_init(queue: *mut DataPtrQueue) {
    // SAFETY: the caller's contract.
    init(unsafe { &mut *queue });
}

/// `QueuePop`: drops the oldest item, if there is one.
///
/// # Safety
/// `queue` is an initialised queue that no other thread uses during the call; the same for the
/// functions below that take a `DataPtrQueue`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_queue_pop(queue: *mut DataPtrQueue) {
    // SAFETY: the caller's contract.
    pop(unsafe { &mut *queue });
}

/// `QueuePush`.
///
/// # Safety
/// See [`rav1g_queue_pop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_queue_push(queue: *mut DataPtrQueue, item: *mut c_void) {
    // SAFETY: the caller's contract.
    push(unsafe { &mut *queue }, item);
}

/// `QueueFront`: the oldest item, or null.
///
/// # Safety
/// See [`rav1g_queue_pop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_queue_front(queue: *mut DataPtrQueue) -> *mut c_void {
    // SAFETY: the caller's contract.
    front(unsafe { &*queue })
}

/// `QueueGet`: takes the oldest item, or returns null.
///
/// # Safety
/// See [`rav1g_queue_pop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_queue_get(queue: *mut DataPtrQueue) -> *mut c_void {
    // SAFETY: the caller's contract.
    get(unsafe { &mut *queue })
}

/// `QueueIsEmpty`.
///
/// # Safety
/// See [`rav1g_queue_pop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_queue_is_empty(queue: *mut DataPtrQueue) -> c_int {
    // SAFETY: the caller's contract.
    c_int::from(is_empty(unsafe { &*queue }))
}

/// The queue of a `DataPtrQueueMT` with its lock held.
struct Locked {
    queue: *mut DataPtrQueueMT,
}

impl Locked {
    /// # Safety
    /// `queue` is a queue between `MTQueueInit` and `MTQueueDestroy`, and stays one while the lock is held.
    unsafe fn lock(queue: *mut DataPtrQueueMT) -> Locked {
        // SAFETY: an initialised critical section (the caller's contract).
        unsafe { EnterCriticalSection(mutex(queue)) };
        Locked { queue }
    }

    fn inner(&mut self) -> &mut DataPtrQueue {
        // SAFETY: the lock is held, so no other thread uses the ring.
        unsafe { &mut (*self.queue).int_queue }
    }

    /// Waits for a signal of the condition, without the lock while it waits.
    fn wait(&mut self) {
        // SAFETY: the condition and the critical section of a live queue, the latter held by this thread.
        // An infinite wait only returns with the lock held again; its result has nothing to say.
        let _ = unsafe { SleepConditionVariableCS(condition(self.queue), mutex(self.queue), INFINITE) };
    }

    fn signal(&mut self) {
        // SAFETY: the condition of a live queue.
        unsafe { WakeConditionVariable(condition(self.queue)) };
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: held since `lock`.
        unsafe { LeaveCriticalSection(mutex(self.queue)) };
    }
}

fn mutex(queue: *mut DataPtrQueueMT) -> *mut CRITICAL_SECTION {
    // SAFETY: only the address of a field is taken.
    unsafe { (&raw mut (*queue).m_mutex).cast() }
}

fn condition(queue: *mut DataPtrQueueMT) -> *mut CONDITION_VARIABLE {
    // SAFETY: only the address of a field is taken.
    unsafe { (&raw mut (*queue).m_empty_cond).cast() }
}

/// `MTQueueInit`.
///
/// # Safety
/// `queue` is valid for writing and is not moved until `MTQueueDestroy`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_init(queue: *mut DataPtrQueueMT) {
    // SAFETY: the caller's contract. The critical section is created as aom_thread.h creates a mutex.
    unsafe {
        init(&mut (*queue).int_queue);
        condition(queue).write(InitializeConditionVariable());
        let _ = InitializeCriticalSectionEx(mutex(queue), 0, 0);
    }
}

/// `MTQueueDestroy`.
///
/// # Safety
/// `queue` was initialised and no thread uses it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_destroy(queue: *mut DataPtrQueueMT) {
    // SAFETY: the caller's contract. A condition variable needs no release.
    unsafe { DeleteCriticalSection(mutex(queue)) };
}

/// `MTQueuePush`: adds an item and wakes one waiting thread.
///
/// # Safety
/// `queue` is a queue between `MTQueueInit` and `MTQueueDestroy`; the same for the functions below.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_push(queue: *mut DataPtrQueueMT, item: *mut c_void) {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    push(locked.inner(), item);
    locked.signal();
}

/// `MTQueueGet`: takes the oldest item, waiting for one.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_get(queue: *mut DataPtrQueueMT) -> *mut c_void {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    while locked.inner().m_QueueNotEmpty == 0 {
        locked.wait();
    }
    get(locked.inner())
}

/// `MTQueuePushSafe`: adds an item, waiting while the ring is full.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_push_safe(queue: *mut DataPtrQueueMT, item: *mut c_void) {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    while locked.inner().m_head - locked.inner().m_tail >= CAPACITY {
        locked.wait();
    }
    push(locked.inner(), item);
    locked.signal();
}

/// `MTQueueGetSafe`: takes the oldest item, waiting for one, and wakes a thread that waits for room.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_get_safe(queue: *mut DataPtrQueueMT) -> *mut c_void {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    while locked.inner().m_QueueNotEmpty == 0 {
        locked.wait();
    }
    let item = get(locked.inner());
    locked.signal();
    item
}

/// `MTQueueIsEmpty`.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_is_empty(queue: *mut DataPtrQueueMT) -> c_int {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    c_int::from(is_empty(locked.inner()))
}

/// `MTQueueFront`: the oldest item, or null.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_front(queue: *mut DataPtrQueueMT) -> *mut c_void {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    front(locked.inner())
}

/// `MTQueuePop`: drops the oldest item, if there is one.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_pop(queue: *mut DataPtrQueueMT) {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    pop(locked.inner());
}

/// `MTQueueClear`: drops every item.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_clear(queue: *mut DataPtrQueueMT) {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    init(locked.inner());
}

/// `MTQueueGetCount`: the items in the queue.
///
/// # Safety
/// See [`rav1g_mt_queue_push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_mt_queue_get_count(queue: *mut DataPtrQueueMT) -> c_int {
    // SAFETY: the caller's contract.
    let mut locked = unsafe { Locked::lock(queue) };
    locked.inner().m_head - locked.inner().m_tail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(number: usize) -> *mut c_void {
        number as *mut c_void
    }

    #[test]
    fn ring_keeps_the_order_and_starts_over_when_empty() {
        // SAFETY: a queue is numbers and pointers; `init` sets what the functions read.
        let mut queue: DataPtrQueue = unsafe { std::mem::zeroed() };
        init(&mut queue);
        assert!(is_empty(&queue));
        assert!(get(&mut queue).is_null());
        assert!(front(&queue).is_null());
        for round in 0..3 {
            for number in 1..=70 {
                push(&mut queue, item(round * 100 + number));
            }
            for number in 1..=70 {
                assert_eq!(front(&queue), item(round * 100 + number));
                assert_eq!(get(&mut queue), item(round * 100 + number));
            }
            // Empty: the ring starts at its first slot again.
            assert!(is_empty(&queue));
            assert_eq!((queue.m_head, queue.m_tail, queue.m_QueueNotEmpty), (0, 0, 0));
        }
        // Past the end of the array: the slots wrap.
        for number in 1..=60 {
            push(&mut queue, item(number));
        }
        for number in 1..=50 {
            assert_eq!(get(&mut queue), item(number));
        }
        for number in 61..=140 {
            push(&mut queue, item(number));
        }
        for number in 51..=140 {
            assert_eq!(get(&mut queue), item(number));
        }
        assert!(is_empty(&queue));
    }

    struct Shared(*mut DataPtrQueueMT);
    // SAFETY: the queue is made to be used from several threads.
    unsafe impl Send for Shared {}

    #[test]
    fn a_waiting_thread_gets_what_another_pushes() {
        // SAFETY: a queue may start as zeros; `rav1g_mt_queue_init` makes it one. It lives in a box, so it
        // does not move, and both threads are done with it before it is destroyed.
        unsafe {
            let queue: *mut DataPtrQueueMT = Box::into_raw(Box::new(std::mem::zeroed()));
            rav1g_mt_queue_init(queue);
            assert_eq!(rav1g_mt_queue_is_empty(queue), 1);
            let shared = Shared(queue);
            let reader = std::thread::spawn(move || {
                let shared = shared;
                (0..1000).map(|_| rav1g_mt_queue_get_safe(shared.0) as usize).collect::<Vec<_>>()
            });
            for number in 1..=1000 {
                rav1g_mt_queue_push_safe(queue, item(number));
            }
            assert_eq!(reader.join().unwrap(), (1..=1000).collect::<Vec<_>>());
            assert_eq!(rav1g_mt_queue_get_count(queue), 0);
            rav1g_mt_queue_push(queue, item(7));
            rav1g_mt_queue_push(queue, item(8));
            assert_eq!(rav1g_mt_queue_get_count(queue), 2);
            assert_eq!(rav1g_mt_queue_front(queue), item(7));
            rav1g_mt_queue_pop(queue);
            assert_eq!(rav1g_mt_queue_get(queue), item(8));
            rav1g_mt_queue_push(queue, item(9));
            rav1g_mt_queue_clear(queue);
            assert_eq!(rav1g_mt_queue_is_empty(queue), 1);
            rav1g_mt_queue_destroy(queue);
            drop(Box::from_raw(queue));
        }
    }
}

// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The queues of the host and the pool of frame buffers: what the decode thread, the tile threads and the
//! completion thread hand to each other.
//!
//! The C++ host had a ring of 100 pointers behind a critical section and a condition variable, inside the
//! decoder's struct (`DataPtrQueueMT` of `libav1/dx/av1_thread.cpp`). These are the same queues as Rust
//! types the decoder owns: the items in the order they were pushed, a `get` that waits for an item, a
//! `push` that wakes one waiting thread and never waits itself.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use crate::sys::parser::HwFrameBuffer;

/// A lock of the host. A thread that panics with one held ends the process (the panic crosses a function
/// the C calls), so what a poisoned lock guards is never looked at again; the lock is taken regardless.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A queue between the threads of a decoder: first in, first out.
pub(crate) struct Queue<T> {
    items: Mutex<VecDeque<T>>,
    not_empty: Condvar,
}

impl<T> Queue<T> {
    /// An empty queue with room for `capacity` items. It never holds more than the pool its items come
    /// from has, so a push allocates nothing.
    pub(crate) fn with_capacity(capacity: usize) -> Queue<T> {
        Queue { items: Mutex::new(VecDeque::with_capacity(capacity)), not_empty: Condvar::new() }
    }

    /// Adds an item behind the others and wakes one waiting thread.
    pub(crate) fn push(&self, item: T) {
        let mut items = lock(&self.items);
        items.push_back(item);
        self.not_empty.notify_one();
    }

    /// Takes the oldest item, waiting for one.
    pub(crate) fn get(&self) -> T {
        let mut items = lock(&self.items);
        loop {
            if let Some(item) = items.pop_front() {
                return item;
            }
            items = self.not_empty.wait(items).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Takes the oldest item when there is one.
    pub(crate) fn try_get(&self) -> Option<T> {
        lock(&self.items).pop_front()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.items).is_empty()
    }
}

/// The pool of frame buffers: the buffers nothing counts on. Its lock also guards the reference count of
/// every frame buffer of the decoder and the references a pipeline slot holds, so the frame setup and the
/// completion thread take it around what they do with those.
pub(crate) struct FramePool {
    free: Mutex<VecDeque<*mut HwFrameBuffer>>,
    returned: Condvar,
}

/// The pool with its lock held.
pub(crate) struct FramePoolGuard<'a> {
    free: MutexGuard<'a, VecDeque<*mut HwFrameBuffer>>,
}

impl FramePool {
    pub(crate) fn with_capacity(capacity: usize) -> FramePool {
        FramePool { free: Mutex::new(VecDeque::with_capacity(capacity)), returned: Condvar::new() }
    }

    pub(crate) fn lock(&self) -> FramePoolGuard<'_> {
        FramePoolGuard { free: lock(&self.free) }
    }

    /// Wakes the thread that waits for a buffer: one came back.
    pub(crate) fn signal(&self) {
        self.returned.notify_one();
    }

    /// A buffer of the pool, with one reference. Waits until a frame in flight returns one when the pool
    /// is empty.
    ///
    /// # Safety
    /// The buffers in the pool are valid frame buffers of the decoder.
    pub(crate) unsafe fn get(&self) -> *mut HwFrameBuffer {
        let mut free = lock(&self.free);
        loop {
            if let Some(fb) = free.pop_front() {
                // SAFETY: the caller's contract; the count is written under the lock.
                unsafe { (*fb).ref_cnt = 1 };
                return fb;
            }
            free = self.returned.wait(free).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// One more reference to a buffer that is out of the pool.
    ///
    /// # Safety
    /// `fb` is a frame buffer of the decoder of this pool.
    pub(crate) unsafe fn acquire(&self, fb: *mut HwFrameBuffer) {
        let _pool = self.lock();
        // SAFETY: the caller's contract; the count is written under the lock.
        unsafe { (*fb).ref_cnt += 1 };
    }

    /// Gives a reference back; the buffer returns to the pool with its last one.
    ///
    /// # Safety
    /// `fb` is a frame buffer of the decoder of this pool, with a reference.
    pub(crate) unsafe fn release(&self, fb: *mut HwFrameBuffer) {
        let mut pool = self.lock();
        // SAFETY: the caller's contract.
        if unsafe { pool.release(fb) } {
            self.signal();
        }
    }
}

impl FramePoolGuard<'_> {
    /// A buffer nothing counts on goes into the pool (at the creation of the decoder).
    pub(crate) fn add(&mut self, fb: *mut HwFrameBuffer) {
        self.free.push_back(fb);
    }

    /// Gives a reference back. True when the buffer went back to the pool; the caller then signals the
    /// pool ([`FramePool::signal`]).
    ///
    /// # Safety
    /// `fb` is a frame buffer of the decoder of this pool, with a reference.
    pub(crate) unsafe fn release(&mut self, fb: *mut HwFrameBuffer) -> bool {
        // SAFETY: the caller's contract; the count is written under the lock.
        unsafe {
            (*fb).ref_cnt -= 1;
            if (*fb).ref_cnt != 0 {
                return false;
            }
        }
        self.free.push_back(fb);
        true
    }
}

/// Starts a thread of the decoder (the pipeline workers, the completion thread). An error when the
/// system has no thread to give: `std::thread::spawn` would panic then, and a panic in a function the C
/// calls ends the process.
pub(crate) fn spawn_thread<F>(run: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(test)]
    if spawn_test::FAIL.get() {
        return Err(std::io::Error::other("no thread (the test asked for this failure)"));
    }
    std::thread::Builder::new().spawn(run)
}

/// For the tests of what a caller does without a thread.
#[cfg(test)]
pub(crate) mod spawn_test {
    use std::cell::Cell;

    thread_local! {
        /// The next `spawn_thread` calls of this thread fail.
        pub(crate) static FAIL: Cell<bool> = const { Cell::new(false) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queue_keeps_the_order() {
        let queue = Queue::with_capacity(4);
        assert!(queue.is_empty());
        assert_eq!(queue.try_get(), None);
        for round in 0..3 {
            // More than the capacity: the queue takes them all the same.
            for number in 1..=70 {
                queue.push(round * 100 + number);
            }
            for number in 1..=70 {
                assert_eq!(queue.get(), round * 100 + number);
            }
            assert!(queue.is_empty());
        }
        queue.push(7);
        queue.push(8);
        assert_eq!(queue.try_get(), Some(7));
        assert_eq!(queue.get(), 8);
        assert_eq!(queue.try_get(), None);
    }

    #[test]
    fn a_waiting_thread_gets_what_another_pushes() {
        let queue = std::sync::Arc::new(Queue::with_capacity(8));
        let reader = {
            let queue = queue.clone();
            std::thread::spawn(move || (0..1000).map(|_| queue.get()).collect::<Vec<usize>>())
        };
        for batch in 0..20 {
            for number in 1..=50 {
                queue.push(batch * 50 + number);
            }
            if batch % 4 == 0 {
                // The reader runs dry and waits.
                while !queue.is_empty() {
                    std::thread::yield_now();
                }
            }
        }
        assert_eq!(reader.join().unwrap(), (1..=1000).collect::<Vec<_>>());
        assert!(queue.is_empty());
    }

    /// The pool as the decoder uses it; its buffers are only pointers here.
    struct Pool(FramePool);
    // SAFETY: the buffers the pool points to live as long as the test, and are used under its lock.
    unsafe impl Send for Pool {}
    unsafe impl Sync for Pool {}

    #[test]
    fn a_frame_buffer_returns_to_the_pool_with_its_last_reference() {
        // SAFETY: a frame buffer description is numbers and pointers.
        let mut buffers: [HwFrameBuffer; 2] = unsafe { std::mem::zeroed() };
        let first = &raw mut buffers[0];
        let second = &raw mut buffers[1];
        let pool = std::sync::Arc::new(Pool(FramePool::with_capacity(2)));
        pool.0.lock().add(first);
        pool.0.lock().add(second);
        // SAFETY: the two buffers above.
        unsafe {
            assert_eq!(pool.0.get(), first);
            assert_eq!(pool.0.get(), second);
            assert_eq!(((*first).ref_cnt, (*second).ref_cnt), (1, 1));
            pool.0.acquire(second);
            pool.0.release(second);
            assert!(pool.0.lock().free.is_empty());
            // A thread waits for a buffer until the last reference of one is given back.
            let waiter = {
                let pool = pool.clone();
                std::thread::spawn(move || pool.0.get() as usize)
            };
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(!waiter.is_finished());
            pool.0.release(second);
            assert_eq!(waiter.join().unwrap(), second as usize);
            assert_eq!((*second).ref_cnt, 1);
            let mut locked = pool.0.lock();
            assert!(locked.release(first));
            assert!(locked.release(second));
            assert_eq!(locked.free.len(), 2);
        }
    }
}

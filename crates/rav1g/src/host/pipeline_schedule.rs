// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! When the pipeline states of a device are compiled, on which thread, and who waits for them
//! (`compute.rs` has the states; docs/integration.md, "Preparing the pipeline states").
//!
//! A state is compiled once, by one thread, and is then ready for good (or failed for good). There are
//! three priorities:
//!
//! - **Needed**: a decoder creation, a frame or a dispatch waits for the state. The waiting thread compiles
//!   the states it waits for itself, next to up to `threads - 1` helper threads, so a waiter never waits
//!   behind a queue: a state it needs is either compiled by it or already being compiled.
//! - **Batch**: the states prepared at the device creation, on the device's threads (at most 6, as many
//!   as the application gave; the console's 4 cores give no gain beyond 4).
//! - **Background**: the states a decoder may need later (the scaled-reference inter states). One thread
//!   at the lowest priority: it takes only what the 4 shared cores leave, and the decode thread never
//!   compiles them unless a frame needs one right then.
//!
//! Every thread of the schedule exits when there is nothing left for it: no thread waits idle while
//! decoders run. Threads are started again by the next request.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_LOWEST,
};

use super::log::log;
use super::thread::{lock, spawn_thread};

/// Who waits for a state.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Priority {
    /// Nobody yet: a state a decoder may need later.
    Background = 0,
    /// The states the device prepares at its creation.
    Batch = 1,
    /// A creation, a frame or a dispatch.
    Needed = 2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Idle,
    Queued(Priority),
    Compiling,
    Ready,
    Failed(i32),
}

/// A request that is logged when its states are complete.
struct Request {
    label: String,
    /// The states that were not ready when it was made.
    slots: Vec<usize>,
    left: usize,
    begin: Instant,
    threads: usize,
    failed: i32,
}

/// What the log of a complete request has.
pub(crate) struct Report {
    pub label: String,
    /// The states that were not ready when it was made, and the microseconds each took to compile.
    pub slots: Vec<usize>,
    pub micros: Vec<u64>,
    /// From the request to the end of its last state.
    pub wall_micros: u64,
    pub threads: usize,
    /// The codes of the states that failed, or'ed; 0 when every state was created.
    pub failed: i32,
}

struct State {
    slots: Vec<Slot>,
    micros: Vec<u64>,
    /// The helper threads of the Needed and Batch states that are running, and the background thread.
    urgent_workers: usize,
    background_worker: bool,
    handles: Vec<JoinHandle<()>>,
    closing: bool,
    requests: Vec<Request>,
}

/// Compiles a state: its pipeline into its slot of the library. `Err` is the code of the failure.
pub(crate) type Compile = dyn Fn(usize) -> Result<(), i32> + Send + Sync;
/// Writes the line of a complete request.
pub(crate) type ReportFn = dyn Fn(&Report) + Send + Sync;

/// The schedule of the states of one library.
pub(crate) struct Scheduler {
    state: Mutex<State>,
    changed: Condvar,
    /// The device's threads (`pipeline_threads`, at most 6).
    threads: usize,
    compile: Box<Compile>,
    report: Box<ReportFn>,
}

impl Scheduler {
    /// A schedule of `count` states, none of them compiled, with `threads` threads for the batch.
    pub(crate) fn new(count: usize, threads: usize, compile: Box<Compile>, report: Box<ReportFn>) -> Arc<Scheduler> {
        Arc::new(Scheduler {
            state: Mutex::new(State {
                slots: vec![Slot::Idle; count],
                micros: vec![0; count],
                urgent_workers: 0,
                background_worker: false,
                handles: Vec::new(),
                closing: false,
                requests: Vec::new(),
            }),
            changed: Condvar::new(),
            threads,
            compile,
            report,
        })
    }

    /// Queues the states `slots` at `priority` (a state that is queued lower is raised to it) and starts
    /// the threads for them: the device's threads for the batch, one fewer for needed states (the waiter
    /// is the other), one background thread. With `label`, the request is logged when its states are
    /// complete. Returns the states that were not ready.
    pub(crate) fn queue(self: &Arc<Self>, slots: &[usize], priority: Priority, label: Option<&str>) -> Vec<usize> {
        let mut state = lock(&self.state);
        let pending = Self::enqueue(&mut state, slots, priority);
        if let Some(label) = label
            && !pending.is_empty()
        {
            let threads = match priority {
                Priority::Background => 1,
                Priority::Batch => self.threads,
                Priority::Needed => self.threads.max(1),
            };
            let failed = Self::failures(&state, slots);
            state.requests.push(Request {
                label: label.to_string(),
                slots: pending.clone(),
                left: pending.len(),
                begin: Instant::now(),
                threads,
                failed,
            });
        }
        let cap = if priority == Priority::Needed { self.threads.saturating_sub(1) } else { self.threads };
        self.start_workers(&mut state, cap);
        pending
    }

    /// Waits until every state of `slots` is ready or failed; the calling thread compiles those of them
    /// that no thread has begun. The code of the states that failed, or'ed, or 0.
    pub(crate) fn wait(self: &Arc<Self>, slots: &[usize], label: Option<&str>) -> i32 {
        let pending = self.queue(slots, Priority::Needed, label);
        let mut state = lock(&self.state);
        loop {
            if let Some(&slot) = pending.iter().find(|&&slot| matches!(state.slots[slot], Slot::Queued(_) | Slot::Idle))
            {
                state.slots[slot] = Slot::Compiling;
                drop(state);
                self.run(slot);
                state = lock(&self.state);
                continue;
            }
            if pending.iter().all(|&slot| matches!(state.slots[slot], Slot::Ready | Slot::Failed(_))) {
                return Self::failures(&state, slots);
            }
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The state is ready.
    #[cfg(test)]
    pub(crate) fn is_ready(&self, slot: usize) -> bool {
        lock(&self.state).slots[slot] == Slot::Ready
    }

    /// Stops the schedule: nothing queued is compiled any more, and the threads are joined (a state that
    /// is being compiled is finished first). For the release of the library.
    pub(crate) fn close(&self) {
        let handles = {
            let mut state = lock(&self.state);
            state.closing = true;
            for slot in &mut state.slots {
                if matches!(slot, Slot::Queued(_)) {
                    *slot = Slot::Idle;
                }
            }
            std::mem::take(&mut state.handles)
        };
        for handle in handles {
            let _ = handle.join();
        }
    }

    fn failures(state: &State, slots: &[usize]) -> i32 {
        slots
            .iter()
            .fold(0, |codes, &slot| if let Slot::Failed(code) = state.slots[slot] { codes | code } else { codes })
    }

    fn enqueue(state: &mut State, slots: &[usize], priority: Priority) -> Vec<usize> {
        let mut pending = Vec::new();
        for &slot in slots {
            match state.slots[slot] {
                Slot::Idle => state.slots[slot] = Slot::Queued(priority),
                Slot::Queued(queued) => state.slots[slot] = Slot::Queued(queued.max(priority)),
                Slot::Compiling => {}
                Slot::Ready | Slot::Failed(_) => continue,
            }
            if !pending.contains(&slot) {
                pending.push(slot);
            }
        }
        pending
    }

    /// Starts helper threads for the queued Batch and Needed states, up to `cap` running, and the
    /// background thread when there are background states and it does not run.
    fn start_workers(self: &Arc<Self>, state: &mut MutexGuard<'_, State>, cap: usize) {
        if state.closing {
            return;
        }
        // The handles of the threads that have exited.
        let mut index = 0;
        while index < state.handles.len() {
            if state.handles[index].is_finished() {
                let _ = state.handles.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        let urgent = state.slots.iter().filter(|slot| matches!(slot, Slot::Queued(p) if *p >= Priority::Batch)).count();
        while state.urgent_workers < cap.min(urgent) {
            let scheduler = self.clone();
            match spawn_thread(move || scheduler.work(true)) {
                Ok(handle) => {
                    state.urgent_workers += 1;
                    state.handles.push(handle);
                }
                Err(error) => {
                    // The waiters compile what is left (`wait`), the device's wait among them.
                    log(true, &format!("MEMORY FAILED pipeline worker thread: {error}"));
                    break;
                }
            }
        }
        let background = state.slots.contains(&Slot::Queued(Priority::Background));
        if background && !state.background_worker {
            let scheduler = self.clone();
            match spawn_thread(move || scheduler.work(false)) {
                Ok(handle) => {
                    state.background_worker = true;
                    state.handles.push(handle);
                }
                // The states stay queued: a frame that needs one compiles it then.
                Err(error) => log(true, &format!("MEMORY FAILED pipeline background thread: {error}")),
            }
        }
    }

    /// A thread of the schedule: takes the queued state of the highest priority until none is left (a
    /// helper takes only Batch and Needed states, the background thread any).
    fn work(self: Arc<Self>, urgent: bool) {
        let mut priority = None;
        loop {
            let mut state = lock(&self.state);
            let next = if state.closing { None } else { Self::take(&mut state, urgent) };
            let Some((slot, taken)) = next else {
                if urgent {
                    state.urgent_workers -= 1;
                } else {
                    state.background_worker = false;
                }
                return;
            };
            drop(state);
            if priority != Some(taken) {
                // Above normal for what a creation or the device waits for, as the pipeline threads of
                // the C++ (aom_thread.h); the lowest for the background, which takes only what the
                // decoder's threads leave of the 4 shared cores.
                let level: THREAD_PRIORITY =
                    if taken == Priority::Background { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_ABOVE_NORMAL };
                // SAFETY: the pseudo handle of this thread.
                let _ = unsafe { SetThreadPriority(GetCurrentThread(), level) };
                priority = Some(taken);
            }
            self.run(slot);
        }
    }

    fn take(state: &mut State, urgent: bool) -> Option<(usize, Priority)> {
        let lowest = if urgent { Priority::Batch } else { Priority::Background };
        let mut best: Option<(usize, Priority)> = None;
        for (slot, entry) in state.slots.iter().enumerate() {
            if let Slot::Queued(priority) = *entry
                && priority >= lowest
                && best.is_none_or(|(_, other)| priority > other)
            {
                best = Some((slot, priority));
            }
        }
        if let Some((slot, _)) = best {
            state.slots[slot] = Slot::Compiling;
        }
        best
    }

    /// Compiles a state this thread has taken (it is `Compiling`), and logs the requests it completes.
    fn run(&self, slot: usize) {
        let start = Instant::now();
        let result = (self.compile)(slot);
        let micros = start.elapsed().as_micros() as u64;
        let mut reports = Vec::new();
        {
            let mut state = lock(&self.state);
            state.micros[slot] = micros;
            state.slots[slot] = match result {
                Ok(()) => Slot::Ready,
                Err(code) => Slot::Failed(code),
            };
            let mut index = 0;
            while index < state.requests.len() {
                let request = &mut state.requests[index];
                if request.slots.contains(&slot) {
                    request.left -= 1;
                    if let Err(code) = result {
                        request.failed |= code;
                    }
                }
                if request.left == 0 {
                    let request = state.requests.swap_remove(index);
                    let micros = request.slots.iter().map(|&slot| state.micros[slot]).collect();
                    reports.push(Report {
                        label: request.label,
                        slots: request.slots,
                        micros,
                        wall_micros: request.begin.elapsed().as_micros() as u64,
                        threads: request.threads,
                        failed: request.failed,
                    });
                } else {
                    index += 1;
                }
            }
        }
        self.changed.notify_all();
        for report in &reports {
            (self.report)(report);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// A schedule whose compile of a state takes until the test releases it (`gate`), or fails for the
    /// states of `fail`; `compiled` counts the compiles of each state, `reports` the logged requests.
    struct Fixture {
        scheduler: Arc<Scheduler>,
        compiled: Arc<Vec<AtomicUsize>>,
        reports: Arc<Mutex<Vec<(String, usize, i32)>>>,
    }

    fn fixture(count: usize, threads: usize, gate: Option<Arc<Mutex<mpsc::Receiver<()>>>>, fail: &[usize]) -> Fixture {
        let compiled: Arc<Vec<AtomicUsize>> = Arc::new((0..count).map(|_| AtomicUsize::new(0)).collect());
        let reports = Arc::new(Mutex::new(Vec::new()));
        let fail = fail.to_vec();
        let counts = compiled.clone();
        let compile: Box<Compile> = Box::new(move |slot| {
            if let Some(gate) = &gate {
                let _ = gate.lock().unwrap().recv();
            }
            counts[slot].fetch_add(1, Ordering::SeqCst);
            if fail.contains(&slot) { Err(-5) } else { Ok(()) }
        });
        let logged = reports.clone();
        let report: Box<ReportFn> = Box::new(move |report: &Report| {
            logged.lock().unwrap().push((report.label.clone(), report.slots.len(), report.failed));
        });
        Fixture { scheduler: Scheduler::new(count, threads, compile, report), compiled, reports }
    }

    #[test]
    fn a_batch_is_compiled_on_threads_once_and_logged() {
        let f = fixture(10, 4, None, &[]);
        let all: Vec<usize> = (0..10).collect();
        assert_eq!(f.scheduler.queue(&all, Priority::Batch, Some("batch")).len(), 10);
        assert_eq!(f.scheduler.wait(&all, None), 0);
        assert!(all.iter().all(|&slot| f.scheduler.is_ready(slot)));
        assert!(f.compiled.iter().all(|count| count.load(Ordering::SeqCst) == 1));
        // A second request for ready states compiles nothing and logs nothing.
        assert!(f.scheduler.queue(&all, Priority::Needed, Some("again")).is_empty());
        f.scheduler.close();
        assert_eq!(*f.reports.lock().unwrap(), vec![("batch".to_string(), 10, 0)]);
    }

    #[test]
    fn without_threads_the_waiter_compiles_everything() {
        let f = fixture(6, 0, None, &[]);
        let all: Vec<usize> = (0..6).collect();
        f.scheduler.queue(&all, Priority::Batch, None);
        assert_eq!(lock(&f.scheduler.state).urgent_workers, 0, "no thread for a device without threads");
        assert_eq!(f.scheduler.wait(&all, Some("creation")), 0);
        assert!(f.compiled.iter().all(|count| count.load(Ordering::SeqCst) == 1));
        assert_eq!(f.reports.lock().unwrap().len(), 1);
        f.scheduler.close();
    }

    #[test]
    fn a_failed_state_fails_every_waiter_and_stays_failed() {
        let f = fixture(4, 2, None, &[2]);
        assert_eq!(f.scheduler.wait(&[0, 1, 2], Some("first")), -5);
        assert_eq!(f.scheduler.wait(&[2, 3], None), -5, "a later waiter gets the failure too");
        assert_eq!(f.scheduler.wait(&[0, 3], None), 0);
        assert_eq!(f.compiled[2].load(Ordering::SeqCst), 1, "a failed state is not compiled again");
        f.scheduler.close();
        assert_eq!(f.reports.lock().unwrap()[0], ("first".to_string(), 3, -5));
    }

    /// Two threads wait for a state a worker compiles (two decoders created at once on one device): neither
    /// returns while the compile runs, and the state is compiled once.
    #[test]
    fn two_waiters_wait_for_the_state_a_worker_compiles() {
        let (release, gate) = mpsc::channel::<()>();
        let f = fixture(1, 1, Some(Arc::new(Mutex::new(gate))), &[]);
        f.scheduler.queue(&[0], Priority::Batch, None);
        // The worker has taken the state by now and waits at the gate.
        std::thread::sleep(Duration::from_millis(100));
        let wait = |returned: Arc<AtomicBool>| {
            let scheduler = f.scheduler.clone();
            std::thread::spawn(move || {
                let code = scheduler.wait(&[0], None);
                returned.store(true, Ordering::SeqCst);
                code
            })
        };
        let (first_returned, second_returned) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let first = wait(first_returned.clone());
        let second = wait(second_returned.clone());
        std::thread::sleep(Duration::from_millis(200));
        assert!(!first_returned.load(Ordering::SeqCst), "the first waiter returned while the state was compiled");
        assert!(!second_returned.load(Ordering::SeqCst), "the second waiter returned while the state was compiled");
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), 0);
        assert_eq!(second.join().unwrap(), 0);
        assert_eq!(f.compiled[0].load(Ordering::SeqCst), 1);
        f.scheduler.close();
    }

    /// A waiter does not wait behind the background thread: it compiles the queued states it needs
    /// itself, and the background thread compiles the rest.
    #[test]
    fn a_waiter_takes_its_states_from_the_background_queue() {
        let (release, gate) = mpsc::channel::<()>();
        let f = fixture(3, 1, Some(Arc::new(Mutex::new(gate))), &[]);
        f.scheduler.queue(&[0, 1, 2], Priority::Background, Some("later"));
        std::thread::sleep(Duration::from_millis(100));
        // The background thread is in the compile of state 0; the waiter takes state 2 itself (a helper
        // may take it as well: the device has a thread), and passes the gate once.
        let scheduler = f.scheduler.clone();
        let waiter = std::thread::spawn(move || scheduler.wait(&[2], None));
        for _ in 0..3 {
            release.send(()).unwrap();
        }
        assert_eq!(waiter.join().unwrap(), 0);
        f.scheduler.wait(&[0, 1, 2], None);
        assert!(f.compiled.iter().all(|count| count.load(Ordering::SeqCst) == 1));
        f.scheduler.close();
        assert_eq!(*f.reports.lock().unwrap(), vec![("later".to_string(), 3, 0)]);
    }

    #[test]
    fn the_threads_exit_when_nothing_is_left() {
        let f = fixture(8, 4, None, &[]);
        let all: Vec<usize> = (0..8).collect();
        f.scheduler.queue(&all[..4], Priority::Batch, None);
        f.scheduler.queue(&all[4..], Priority::Background, None);
        f.scheduler.wait(&all, None);
        for _ in 0..100 {
            let state = lock(&f.scheduler.state);
            if state.urgent_workers == 0 && !state.background_worker {
                break;
            }
            drop(state);
            std::thread::sleep(Duration::from_millis(10));
        }
        let state = lock(&f.scheduler.state);
        assert_eq!((state.urgent_workers, state.background_worker), (0, false));
        drop(state);
        f.scheduler.close();
    }

    #[test]
    fn a_closed_schedule_compiles_nothing_queued() {
        let (release, gate) = mpsc::channel::<()>();
        let f = fixture(4, 1, Some(Arc::new(Mutex::new(gate))), &[]);
        f.scheduler.queue(&[0, 1, 2, 3], Priority::Batch, None);
        std::thread::sleep(Duration::from_millis(100));
        let scheduler = f.scheduler.clone();
        let closer = std::thread::spawn(move || scheduler.close());
        std::thread::sleep(Duration::from_millis(100));
        release.send(()).unwrap();
        closer.join().unwrap();
        let compiled: usize = f.compiled.iter().map(|count| count.load(Ordering::SeqCst)).sum();
        assert_eq!(compiled, 1, "only the state that was being compiled is finished");
    }
}

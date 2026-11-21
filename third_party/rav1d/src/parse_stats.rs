//! rav1g: the times of each frame of the frame-parallel parse, for `rav1g-parse-check -time -frametimes`.
//! Off unless [`set_enabled`] switched them on: each hook is then one relaxed load of a flag, and the
//! parse is upstream's either way (nothing here decides anything).
//!
//! A frame's times, from its frame context (`Rav1dFrameContextTaskThread::stats`):
//!
//! - admission: `rav1d_submit_frame` waits for the frame context the frame is given (they are taken in
//!   turn, so with two frames in parse a frame waits for the frame two before it to end);
//! - wall: from the context being free to the frame's end (`rav1d_decode_frame_exit`);
//! - init: the `Init` and `InitCdf` tasks; before them the frame waits for a worker (`pre`);
//! - CDF wait: from the end of `Init` to the start of `InitCdf`, which waits for the CDFs the frame loads
//!   (`load_cdfs( ref_frame_idx[ primary_ref_frame ] )` of the uncompressed header, AV1 5.9.2: what the
//!   frame they come from saved at its end, 7.20);
//! - tile work: the time of the frame's entropy tile tasks, summed over its tiles;
//! - reference-row wait: time in which none of the frame's tiles ran after `check_tile` held one of them
//!   back for the rows of a reference it reads; the rest of the time without a tile running is `idle`.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);
static EPOCH: OnceLock<Instant> = OnceLock::new();
static NUMBER: AtomicU32 = AtomicU32::new(0);
static RECORDS: Mutex<Vec<FrameTimes>> = Mutex::new(Vec::new());

/// The times of a frame, in milliseconds, as [`take`] returns them.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTimes {
    /// The frames in the order they were given a frame context (decoding order), from 0 since
    /// [`set_enabled`].
    pub number: u32,
    pub admit_wait: f64,
    pub wall: f64,
    pub pre: f64,
    pub init: f64,
    pub cdf_wait: f64,
    pub tile: f64,
    pub ref_wait: f64,
    pub idle: f64,
}

/// Switches the times on or off; on, the records and the frame numbers start again from nothing.
pub fn set_enabled(on: bool) {
    EPOCH.get_or_init(Instant::now);
    NUMBER.store(0, Ordering::SeqCst);
    RECORDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    ON.store(on, Ordering::SeqCst);
}

/// The records of the frames that ended since [`set_enabled`], in the order they ended.
pub fn take() -> Vec<FrameTimes> {
    std::mem::take(
        &mut *RECORDS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

#[inline]
pub(crate) fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Nanoseconds since the first [`set_enabled`]; 0 while the times are off.
#[inline]
pub(crate) fn now() -> u64 {
    if !enabled() {
        return 0;
    }
    EPOCH
        .get()
        .map_or(0, |epoch| epoch.elapsed().as_nanos() as u64)
}

/// The times of the frame a frame context has.
#[derive(Default)]
pub(crate) struct FrameStats {
    number: AtomicU32,
    submit: AtomicU64,
    admit: AtomicU64,
    init_end: AtomicU64,
    pre_ns: AtomicU64,
    init_ns: AtomicU64,
    cdf_wait_ns: AtomicU64,
    tile_ns: AtomicU64,
    ref_wait_ns: AtomicU64,
    idle_ns: AtomicU64,
    /// Tile tasks of the frame that run now, and since when none has run.
    active: AtomicI32,
    idle_since: AtomicU64,
    /// `check_tile` held a tile back for the rows of a reference since the last tile of the frame ran.
    blocked: AtomicBool,
}

impl FrameStats {
    /// The frame context is free for a frame that asked for it at `submit`.
    pub(crate) fn admitted(&self, submit: u64) {
        if !enabled() {
            return;
        }
        let now = now();
        self.number
            .store(NUMBER.fetch_add(1, Ordering::SeqCst), Ordering::Relaxed);
        self.submit.store(submit, Ordering::Relaxed);
        self.admit.store(now, Ordering::Relaxed);
        for counter in [
            &self.init_end,
            &self.pre_ns,
            &self.init_ns,
            &self.cdf_wait_ns,
            &self.tile_ns,
            &self.ref_wait_ns,
            &self.idle_ns,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        self.active.store(0, Ordering::Relaxed);
        self.idle_since.store(0, Ordering::Relaxed);
        self.blocked.store(false, Ordering::Relaxed);
    }

    /// The `Init` task starts: the time since the frame was admitted is `pre`.
    pub(crate) fn init_start(&self) -> u64 {
        let now = now();
        if now != 0 {
            self.pre_ns.store(
                now.saturating_sub(self.admit.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
        now
    }

    pub(crate) fn init_done(&self, start: u64) {
        let now = now();
        if start != 0 && now != 0 {
            self.init_ns.fetch_add(now - start, Ordering::Relaxed);
            self.init_end.store(now, Ordering::Relaxed);
        }
    }

    /// The `InitCdf` task starts: the time since `Init` ended is the CDF wait.
    pub(crate) fn cdf_start(&self) -> u64 {
        let now = now();
        let end = self.init_end.load(Ordering::Relaxed);
        if now != 0 && end != 0 {
            self.cdf_wait_ns
                .fetch_add(now.saturating_sub(end), Ordering::Relaxed);
        }
        now
    }

    pub(crate) fn cdf_done(&self, start: u64) {
        let now = now();
        if start != 0 && now != 0 {
            self.init_ns.fetch_add(now - start, Ordering::Relaxed);
            self.idle_since.store(now, Ordering::Relaxed);
        }
    }

    /// A tile task of the frame starts: the time without a tile since the last one, as a reference-row
    /// wait or as idle.
    pub(crate) fn tile_start(&self) -> u64 {
        let now = now();
        if now == 0 {
            return 0;
        }
        if self.active.fetch_add(1, Ordering::Relaxed) == 0 {
            let since = self.idle_since.load(Ordering::Relaxed);
            if since != 0 {
                let gap = now.saturating_sub(since);
                if self.blocked.swap(false, Ordering::Relaxed) {
                    self.ref_wait_ns.fetch_add(gap, Ordering::Relaxed);
                } else {
                    self.idle_ns.fetch_add(gap, Ordering::Relaxed);
                }
            }
        }
        now
    }

    pub(crate) fn tile_end(&self, start: u64) {
        let now = now();
        if start == 0 || now == 0 {
            return;
        }
        self.tile_ns.fetch_add(now - start, Ordering::Relaxed);
        if self.active.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.idle_since.store(now, Ordering::Relaxed);
        }
    }

    /// `check_tile` holds a tile of the frame back for the rows of a reference.
    #[inline]
    pub(crate) fn blocked_by_refs(&self) {
        if enabled() {
            self.blocked.store(true, Ordering::Relaxed);
        }
    }

    /// The frame has ended.
    pub(crate) fn finished(&self) {
        let now = now();
        if now == 0 || self.admit.load(Ordering::Relaxed) == 0 {
            return;
        }
        let ms = |ns: u64| ns as f64 / 1e6;
        let admit = self.admit.load(Ordering::Relaxed);
        let since = self.idle_since.load(Ordering::Relaxed);
        let mut idle = self.idle_ns.load(Ordering::Relaxed);
        if since != 0 && self.active.load(Ordering::Relaxed) == 0 {
            idle += now.saturating_sub(since);
        }
        let record = FrameTimes {
            number: self.number.load(Ordering::Relaxed),
            admit_wait: ms(admit.saturating_sub(self.submit.load(Ordering::Relaxed))),
            wall: ms(now.saturating_sub(admit)),
            pre: ms(self.pre_ns.load(Ordering::Relaxed)),
            init: ms(self.init_ns.load(Ordering::Relaxed)),
            cdf_wait: ms(self.cdf_wait_ns.load(Ordering::Relaxed)),
            tile: ms(self.tile_ns.load(Ordering::Relaxed)),
            ref_wait: ms(self.ref_wait_ns.load(Ordering::Relaxed)),
            idle: ms(idle),
        };
        self.admit.store(0, Ordering::Relaxed);
        RECORDS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record);
    }
}

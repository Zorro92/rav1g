#![cfg_attr(target_arch = "arm", feature(stdarch_arm_feature_detection))]
#![cfg_attr(
    any(target_arch = "riscv32", target_arch = "riscv64"),
    feature(stdarch_riscv_feature_detection)
)]
#![allow(clippy::derivable_impls, clippy::erasing_op, clippy::ptr_eq)]
#![expect(
    clippy::arc_with_non_send_sync,
    clippy::borrow_deref_ref,
    clippy::borrowed_box,
    clippy::cast_abs_to_unsigned,
    clippy::clone_on_copy,
    clippy::collapsible_else_if,
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::duplicate_underscore_argument,
    clippy::explicit_auto_deref,
    clippy::identity_op,
    clippy::int_plus_one,
    clippy::into_iter_on_ref,
    clippy::large_const_arrays,
    clippy::large_enum_variant,
    clippy::len_without_is_empty,
    clippy::len_zero,
    clippy::let_and_return,
    clippy::let_underscore_lock,
    clippy::manual_div_ceil,
    clippy::manual_range_contains,
    clippy::manual_saturating_arithmetic,
    clippy::module_inception,
    clippy::misrefactored_assign_op,
    clippy::needless_borrow,
    clippy::needless_late_init,
    clippy::needless_lifetimes,
    clippy::needless_option_as_deref,
    clippy::needless_range_loop,
    clippy::needless_return,
    clippy::neg_multiply,
    clippy::nonminimal_bool,
    clippy::option_map_unit_fn,
    clippy::partialeq_to_none,
    clippy::precedence,
    clippy::redundant_closure,
    clippy::redundant_pattern_matching,
    clippy::redundant_static_lifetimes,
    clippy::search_is_some,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::unit_arg,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_fallible_conversions,
    clippy::unnecessary_map_on_constructor,
    clippy::unnecessary_mut_passed,
    clippy::unnecessary_lazy_evaluations,
    clippy::unneeded_wildcard_pattern,
    clippy::upper_case_acronyms,
    clippy::useless_conversion,
    clippy::zero_prefixed_literal
)]
#![deny(
    unsafe_op_in_unsafe_fn,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

// rav1g: this copy is the parse pass of rav1d only (PATCHES.md). What the removed reconstruction used
// of the files that stay (imports, helpers, a few fields) is left where it is, so that the difference
// to upstream stays the list of PATCHES.md; hence these.
#![allow(
    dead_code,
    unused_assignments,
    unused_imports,
    unused_macros,
    unused_mut,
    unused_variables
)]

#[cfg(not(any(feature = "bitdepth_8", feature = "bitdepth_16")))]
compile_error!("No bitdepths enabled. Enable one or more of the following features: `bitdepth_8`, `bitdepth_16`");

pub mod include {
    pub mod common {
        pub(crate) mod attributes;
        pub(crate) mod bitdepth;
        pub(crate) mod dump;
        pub(crate) mod intops;
        pub(crate) mod validate;
    } // mod common
    pub mod dav1d {
        pub mod common;
        pub mod data;
        pub mod dav1d;
        pub mod headers;
        pub mod picture;
    } // mod dav1d
} // mod include

pub mod align;
pub(crate) mod c_arc;
pub(crate) mod c_box;
mod cdf;
mod const_fn;
pub mod cpu;
mod ctx;
mod cursor;
mod data;
mod decode;
mod dequant_tables;
pub(crate) mod disjoint_mut;
pub(crate) mod enum_map;
mod env;
pub(crate) mod error;
mod ffi_safe;
mod getbits;
pub(crate) mod pixels;
pub(crate) mod relaxed_atomic;
pub mod send_sync_non_null;
pub(crate) mod strided;
pub(crate) mod with_offset;
pub(crate) mod wrap_fn_ptr;
// TODO(kkysen) Temporarily `pub(crate)` due to a `pub use` until TAIT.
mod extensions;
mod in_range;
mod internal;
mod intra_edge;
mod iter;
pub mod levels;
mod lf_mask;
pub(crate) mod log;
mod msac;
mod obu;
/// rav1g: the parse pass behind a sink.
pub mod parse;
mod picture;
mod pool;
mod qm;
mod recon;
mod refmvs;
mod scan;
mod tables;
mod thread_task;
mod warpmv;

use std::ffi::{c_char, c_uint, c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Once};
use std::{cmp, mem, ptr, slice, thread};

use parking_lot::Mutex;
use to_method::To as _;

use crate::c_arc::RawArc;
use crate::c_box::{CBox, CRef, FnFree};
use crate::cpu::rav1d_init_cpu;
use crate::decode::rav1d_decode_frame_exit;
pub use crate::error::{Dav1dResult, Rav1dError, Rav1dResult};
use crate::extensions::OptionError as _;
use crate::in_range::InRange;
#[cfg(feature = "bitdepth_16")]
use crate::include::common::bitdepth::BitDepth16;
#[cfg(feature = "bitdepth_8")]
use crate::include::common::bitdepth::BitDepth8;
use crate::include::common::intops::clip;
use crate::include::common::validate::validate_input;
use crate::include::dav1d::common::{Dav1dDataProps, Rav1dDataProps};
use crate::include::dav1d::data::{Dav1dData, Rav1dData};
use crate::include::dav1d::dav1d::{
    Dav1dContext, Dav1dEventFlags, Dav1dSettings, Rav1dDecodeFrameType, Rav1dInloopFilterType,
    Rav1dSettings, RAV1D_MAX_THREADS,
};
use crate::include::dav1d::headers::{Dav1dSequenceHeader, Rav1dFilmGrainData};
use crate::include::dav1d::picture::{Dav1dPicture, Rav1dPicture};
use crate::internal::{
    Rav1dContext, Rav1dContextFrameThread, Rav1dContextTaskThread,
    Rav1dContextTaskType, Rav1dFrameContext, Rav1dState, Rav1dTaskContext,
    Rav1dTaskContextTaskThread, TaskThreadData,
};
use crate::iter::wrapping_iter;
use crate::log::{Rav1dLog as _, Rav1dLogger};
use crate::obu::{rav1d_parse_obus, rav1d_parse_sequence_header};
use crate::picture::{rav1d_picture_alloc_copy, PictureFlags};
use crate::send_sync_non_null::SendSyncNonNull;
use crate::thread_task::{rav1d_worker_task, FRAME_ERROR};

#[cold]
fn init_internal() {
    rav1d_init_cpu();
}

const DAV1D_VERSION: &CStr = c"966d63c1";
const RAV1D_VERSION: &str = match DAV1D_VERSION.to_str() {
    Ok(version) => version,
    Err(_) => unreachable!(),
};

pub const fn rav1d_version() -> &'static str {
    RAV1D_VERSION
}

pub const DAV1D_API_VERSION_MAJOR: u8 = 7;
pub const DAV1D_API_VERSION_MINOR: u8 = 0;
pub const DAV1D_API_VERSION_PATCH: u8 = 0;

impl Default for Rav1dSettings {
    fn default() -> Self {
        Self {
            n_threads: InRange::new(0).unwrap(),
            max_frame_delay: InRange::new(0).unwrap(),
            apply_grain: true,
            operating_point: InRange::new(0).unwrap(),
            all_layers: true,
            frame_size_limit: 0,
            allocator: Default::default(),
            logger: Some(Rav1dLogger::default()),
            strict_std_compliance: false,
            output_invisible_frames: false,
            inloop_filters: Rav1dInloopFilterType::all(),
            decode_frame_type: Rav1dDecodeFrameType::All,
        }
    }
}

struct NumThreads {
    n_tc: u16,
    n_fc: u16,
}

#[cold]
fn get_num_threads(s: &Rav1dSettings) -> NumThreads {
    // rav1g: the application says how many threads and frames (`ParseSettings`); 0 is 1. Upstream
    // takes the number of processors for 0 threads and its square root for 0 frames, which on Windows
    // are two imports the library does not have otherwise (`GetSystemInfo`, `ceil`).
    let n_tc = cmp::max(s.n_threads.get(), 1); // TODO propagate `InRange`
    let n_fc = cmp::min(cmp::max(s.max_frame_delay.get(), 1), n_tc);
    NumThreads { n_fc, n_tc }
}

#[cold]
pub(crate) fn rav1d_get_frame_delay(s: &Rav1dSettings) -> u16 {
    get_num_threads(s).n_fc
}

#[cold]
pub(crate) fn rav1d_open(
    s: &Rav1dSettings,
    sink: Arc<dyn crate::parse::ParseSink>,
) -> Rav1dResult<Arc<Rav1dContext>> {
    static INITTED: Once = Once::new();
    INITTED.call_once(|| init_internal());

    validate_input!((
        !s.allocator.is_default() || s.allocator.cookie.is_none(),
        Rav1dError::InvalidArgument
    ))?;

    // On 32-bit systems, extremely large frame sizes can cause overflows in
    // `rav1d_decode_frame` alloc size calculations. Prevent that from occuring
    // by enforcing a maximum frame size limit, chosen to roughly correspond to
    // the largest size possible to decode without exhausting virtual memory.
    let frame_size_limit;
    if mem::size_of::<usize>() < 8 && s.frame_size_limit.wrapping_sub(1) >= 8192 * 8192 {
        frame_size_limit = 8192 * 8192;
        if s.frame_size_limit != 0 {
            writeln!(
                s.logger,
                "Frame size limit reduced from {} to {}.",
                s.frame_size_limit, frame_size_limit,
            );
        }
    } else {
        frame_size_limit = s.frame_size_limit;
    }

    let NumThreads { n_tc, n_fc } = get_num_threads(s);

    let ttd = TaskThreadData {
        cur: (n_fc as u32).into(),
        reset_task_cur: AtomicU32::new(u32::MAX),
        ..Default::default()
    };
    // TODO fallible allocation
    let task_thread = Arc::new(ttd);

    let fc = (0..n_fc)
        .map(|i| {
            let mut fc = Rav1dFrameContext::default(i.into());
            fc.task_thread.finished = AtomicBool::new(true);
            fc.task_thread.ttd = Arc::clone(&task_thread);
            let f = fc.data.get_mut();
            f.lf.last_sharpness = u8::MAX;
            fc
        })
        // TODO fallible allocation
        .collect();

    let state = Mutex::new(Rav1dState {
        frame_thread: Rav1dContextFrameThread {
            out_delayed: if n_fc > 1 {
                (0..n_fc).map(|_| Default::default()).collect()
            } else {
                Box::new([])
            },
            ..Default::default()
        },
        ..Default::default()
    });

    let tc = (0..n_tc)
        .map(|n| {
            let task_thread = Arc::clone(&task_thread);
            let thread_data = Arc::new(Rav1dTaskContextTaskThread::new(task_thread));
            let thread_data_copy = Arc::clone(&thread_data);
            let task = if n_tc > 1 {
                let handle = thread::Builder::new()
                    // Don't set stack size like `dav1d` does.
                    // See <https://github.com/memorysafety/rav1d/issues/889>.
                    .name(format!("rav1d-worker-{n}"))
                    .spawn(|| rav1d_worker_task(thread_data_copy))
                    .unwrap();
                Rav1dContextTaskType::Worker(handle)
            } else {
                Rav1dContextTaskType::Single(Mutex::new(Box::new(Rav1dTaskContext::new(
                    thread_data_copy,
                ))))
            };
            Rav1dContextTaskThread { task, thread_data }
        })
        // TODO fallible allocation
        .collect();

    let c = Rav1dContext {
        allocator: s.allocator.clone(),
        logger: s.logger.clone(),
        apply_grain: s.apply_grain,
        operating_point: s.operating_point.get(), // TODO propagate `InRange`
        all_layers: s.all_layers,
        frame_size_limit,
        strict_std_compliance: s.strict_std_compliance,
        output_invisible_frames: s.output_invisible_frames,
        inloop_filters: s.inloop_filters,
        decode_frame_type: s.decode_frame_type,
        fc,
        task_thread,
        state,
        tc,
        sink,
        dsp: Default::default(),
        flush: Default::default(),
        picture_pool: Default::default(),
    };

    // TODO fallible allocation
    let mut c = Arc::new(c);

    if c.allocator.is_default() {
        let c = Arc::get_mut(&mut c).unwrap();
        // SAFETY: When `allocator.is_default()`, `allocator.cookie` should be a `&c.picture_pool`.
        // See `Rav1dPicAllocator::cookie` docs for more, including an analysis of the lifetime.
        // Note also that we must do this after we created the `Arc` so that `c` has a stable address.
        c.allocator.cookie = Some(SendSyncNonNull::from_ref(&c.picture_pool).cast::<c_void>());
    }
    let c = c;

    for tc in c.tc.iter() {
        if let Rav1dContextTaskType::Worker(handle) = &tc.task {
            // Unpark each thread once we set its `thread_data.c`.
            *tc.thread_data.c.lock() = Some(Arc::clone(&c));
            handle.thread().unpark();
        }
    }

    Ok(c)
}

fn output_image(c: &Rav1dContext, state: &mut Rav1dState, out: &mut Rav1dPicture) -> Rav1dResult {
    let res = Ok(());

    let use_cache = !c.all_layers && state.max_spatial_id != 0;
    let r#in = if !use_cache {
        &mut state.out
    } else {
        &mut state.cache
    };
    // rav1g: film grain is not applied here; a picture is a handle without samples.
    *out = mem::take(&mut r#in.p);
    let _ = mem::take(r#in);

    if use_cache && state.out.p.data.is_some() {
        state.cache = mem::take(&mut state.out);
    }
    res
}

fn output_picture_ready(c: &Rav1dContext, state: &mut Rav1dState, drain: bool) -> bool {
    if state.cached_error.is_some() {
        return true;
    }
    if !c.all_layers && state.max_spatial_id != 0 {
        if state.out.p.data.is_some() && state.cache.p.data.is_some() {
            if state.max_spatial_id == state.cache.p.frame_hdr.as_ref().unwrap().spatial_id
                || state.out.flags.contains(PictureFlags::NEW_TEMPORAL_UNIT)
            {
                return true;
            }
            state.cache = mem::take(&mut state.out);
            return false;
        } else {
            if state.cache.p.data.is_some() && drain {
                return true;
            } else {
                if state.out.p.data.is_some() {
                    state.cache = mem::take(&mut state.out);
                    return false;
                }
            }
        }
    }
    state.out.p.data.is_some()
}

fn drain_picture(c: &Rav1dContext, state: &mut Rav1dState, out: &mut Rav1dPicture) -> Rav1dResult {
    let mut drained = false;
    for _ in 0..c.fc.len() {
        let next = state.frame_thread.next;
        let fc = &c.fc[next as usize];
        let mut task_thread_lock = c.task_thread.lock.lock();
        while !fc.task_thread.finished.load(Ordering::SeqCst) {
            fc.task_thread.cond.wait(&mut task_thread_lock);
        }
        let out_delayed = &mut state.frame_thread.out_delayed[next as usize];
        if out_delayed.p.data.is_some() || fc.task_thread.error.load(Ordering::SeqCst) != 0 {
            let first = c.task_thread.first.load(Ordering::SeqCst);
            if first as usize + 1 < c.fc.len() {
                c.task_thread.first.fetch_add(1, Ordering::SeqCst);
            } else {
                c.task_thread.first.store(0, Ordering::SeqCst);
            }
            let _ = c.task_thread.reset_task_cur.compare_exchange(
                first,
                u32::MAX,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            let cur = c.task_thread.cur.get();
            if cur != 0 && (cur as usize) < c.fc.len() {
                c.task_thread.cur.set(cur - 1);
            }
            drained = true;
        } else if drained {
            break;
        }
        state.frame_thread.next = (state.frame_thread.next + 1) % c.fc.len() as u32;
        drop(task_thread_lock);
        mem::take(&mut *fc.task_thread.retval.try_lock().unwrap())
            .err_or(())
            .inspect_err(|_| {
                state.cached_error_props = out_delayed.p.m.clone();
                let _ = mem::take(out_delayed);
            })?;
        if out_delayed.p.data.is_some() {
            let progress = out_delayed.progress.as_ref().unwrap()[1].load(Ordering::Relaxed);
            if (out_delayed.visible || c.output_invisible_frames) && progress != FRAME_ERROR {
                state.out = out_delayed.clone();
                state.event_flags |= out_delayed.flags.into();
            }
            let _ = mem::take(out_delayed);
            if output_picture_ready(c, state, false) {
                return output_image(c, state, out);
            }
        }
    }
    if output_picture_ready(c, state, true) {
        return output_image(c, state, out);
    }
    Err(Rav1dError::TryAgain)
}

fn gen_picture(c: &Rav1dContext, state: &mut Rav1dState) -> Rav1dResult {
    if output_picture_ready(c, state, false) {
        return Ok(());
    }
    // Take so we don't have 2 `&mut`s.
    let Rav1dData {
        data: r#in,
        m: props,
    } = mem::take(&mut state.in_0);
    let Some(mut r#in) = r#in else { return Ok(()) };
    while !r#in.is_empty() {
        // rav1g: where this OBU starts, for the sink.
        state.data_left = r#in.len();
        let len = rav1d_parse_obus(c, state, &r#in, &props);
        if let Ok(len) = len {
            r#in.slice_in_place(len..);
        }
        // Note that [`output_picture_ready`] doesn't read [`Rav1dContext::in_0`].
        if output_picture_ready(c, state, false) {
            // Restore into `c` when there's still data left.
            if !r#in.is_empty() {
                state.in_0 = Rav1dData {
                    data: Some(r#in),
                    m: props,
                }
            }
            break;
        }
        len?;
    }
    Ok(())
}

pub(crate) fn rav1d_send_data(c: &Rav1dContext, in_0: &mut Rav1dData) -> Rav1dResult {
    let state = &mut *c.state.try_lock().unwrap();
    if let Some(data) = in_0.data.as_ref() {
        let sz = data.len();
        validate_input!((sz > 0 && sz <= usize::MAX / 2, Rav1dError::InvalidArgument))?;
        state.drain = false;
    }
    if state.in_0.data.is_some() {
        return Err(Rav1dError::TryAgain);
    }
    state.in_0 = in_0.clone();
    let res = gen_picture(c, state);
    if res.is_ok() {
        let _ = mem::take(in_0);
    }
    res
}

pub(crate) fn rav1d_get_picture(c: &Rav1dContext, out: &mut Rav1dPicture) -> Rav1dResult {
    let state = &mut *c.state.try_lock().unwrap();
    let drain = mem::replace(&mut state.drain, true);
    gen_picture(c, state)?;
    mem::take(&mut state.cached_error).err_or(())?;
    if output_picture_ready(c, state, c.fc.len() == 1) {
        return output_image(c, state, out);
    }
    if c.fc.len() > 1 && drain {
        return drain_picture(c, state, out);
    }
    Err(Rav1dError::TryAgain)
}

pub(crate) fn rav1d_flush(c: &Rav1dContext) {
    let state = &mut *c.state.try_lock().unwrap();

    let old_state = mem::take(state);
    state.tiles = old_state.tiles;
    state.n_tiles = old_state.n_tiles;
    state.frame_thread = old_state.frame_thread;
    state.operating_point_idc = old_state.operating_point_idc;
    state.max_spatial_id = old_state.max_spatial_id;
    state.frame_flags = old_state.frame_flags;
    state.event_flags = old_state.event_flags;

    if c.fc.len() == 1 && c.tc.len() == 1 {
        return;
    }
    c.flush.store(true, Ordering::SeqCst);
    if c.tc.len() > 1 {
        let mut task_thread_lock = c.task_thread.lock.lock();
        for tc in c.tc.iter() {
            while !tc.flushed() {
                tc.thread_data.cond.wait(&mut task_thread_lock);
            }
        }
        for fc in c.fc.iter() {
            fc.task_thread.tasks.clear();
        }
        c.task_thread.first.store(0, Ordering::SeqCst);
        c.task_thread.cur.set(c.fc.len() as u32);
        c.task_thread
            .reset_task_cur
            .store(u32::MAX, Ordering::SeqCst);
        c.task_thread.cond_signaled.store(0, Ordering::SeqCst);
    }
    if c.fc.len() > 1 {
        for fc in wrapping_iter(c.fc.iter(), state.frame_thread.next as usize) {
            let _ = rav1d_decode_frame_exit(c, fc, Err(Rav1dError::Other));
            *fc.task_thread.retval.try_lock().unwrap() = None;
            let out_delayed = &mut state.frame_thread.out_delayed[fc.index];
            if out_delayed.p.frame_hdr.is_some() {
                let _ = mem::take(out_delayed);
            }
        }
        state.frame_thread.next = 0;
    }
    c.flush.store(false, Ordering::SeqCst);
}

#[cold]
pub(crate) fn rav1d_close(c: &Rav1dContext) {
    rav1d_flush(c);
    c.tell_worker_threads_to_die();
}

impl Rav1dContext {
    fn tell_worker_threads_to_die(&self) {
        if self.tc.is_empty() {
            return;
        }
        let ttd = &*self.task_thread;
        let _task_thread_lock = ttd.lock.lock();
        for tc in self.tc.iter() {
            tc.thread_data.die.set(true);
        }
        ttd.cond.notify_all();
    }
}

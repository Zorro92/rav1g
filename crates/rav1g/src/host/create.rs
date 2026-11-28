// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! Creation and release of a decoder (`av1_create_gpu_decoder`, `av1_destroy_gpu_decoder` and
//! `av1_query_memory_requirements` of `av1_core.cpp`): the plan of its buffers, the budget guard, the
//! "MEMORY" lines that say what it holds, and the pool of frame buffers.
//!
//! The decisions and the lines are those of the C++ host, number for number: an application reads them,
//! and the two decoders are compared by them. The exception is the host memory. The C++ decoder lies in
//! the host pool; this one is an allocation of its own ([`Av1Core::new`], with its allocator), and the
//! pool holds what the entropy decoder is given: its state, its buffer pool, its contexts and motion
//! vectors, the tiles of the slots. Both exist before the budget guard asks the application what it has
//! left, so the answer already has them, and the guard counts what comes after it: the device buffers.

use std::ffi::{CStr, c_int, c_void};
use std::fmt::Write as _;
use std::time::Duration;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::core::Interface;

use super::completion::av1_drain_gpu_decoder;
use super::compute::{upload_luts, wait_shader_create_complete};
use super::log::{clear_create_error, log, log_create_error};
use super::memory::{Allocator, Plan, log_video_memory, memory_type_name};
use super::pool::{COEF_CHUNK, KINDS, Kind, MODE_INFO_CHUNK, Pools, TXB_CHUNK};
use super::stage_buffers::{GRID_RUNS, IDCT_BLOCKS, INDEX_LIST, PRED_BLOCKS, PRED_BLOCKS_WARP, RECORD, initial_bytes};
use super::types::{
    Av1Core, DeviceOnly, DeviceOnlyConst, DeviceUpload, FrameThreadDataCount, FrameThreadDataDefault, GpuBufferObject,
    HostRW, ImagePoolSize, MemoryType, OutputCbSize, OutputThreadDataCount, ReadBack, RefFrameBufferCount,
    RefFrameBufferCountSharedBack, UploadUAV, av1_frame_thread_data,
};
use crate::gpu::d3d12::{com_ref, com_release, com_store, wrapped_ref};
use crate::gpu::{self, CommandAllocator, Device, Event, SemiPlanar, VideoMemory, create_hardware_device};
use crate::sys::{
    AV1_LOW_MEMORY_COEFS, AV1_LOW_MEMORY_DEFAULT, AV1_LOW_MEMORY_LATE_FRAME_BUFFER, AV1_LOW_MEMORY_MODE_INFO,
    AV1_LOW_MEMORY_PARTS, AV1_LOW_MEMORY_SHARED_BACK_BUFFER, AV1_MEMORY_GUARD_SETTLE_MS, AV1_MEMORY_GUARD_WAIT_MS,
};
use crate::sys::{MB_MODE_INFO, RestorationUnitInfo, aom_codec_dec_cfg_t, av1_tile_data};

/// Bytes of a transform block record of the inverse transform (`IdctBlockSize`).
const IDCT_BLOCK_SIZE: i32 = 16;

/// `AV1_MEMORY_GUARD_MARGIN` of `aom/aom_decoder.h`: what a creation must leave of the application's
/// memory.
const MEMORY_GUARD_MARGIN: u64 = 48 << 20;
/// `AV1_MEMORY_GUARD_HEADROOM` of `aom/aom_decoder.h`: what a creation should leave of the application's
/// budget so that its frames are not slowed by the memory running out; a decoder that would leave less
/// is created with fewer slots and frames in parse (`aom_codec_dec_cfg_t::memory_headroom`).
const MEMORY_GUARD_HEADROOM: u64 = 128 << 20;
/// How often the budget is asked again while it grows (`PollMs`).
const GUARD_POLL_MS: u32 = 20;

pub(crate) fn to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// A line of at most `capacity - 1` bytes that is put together piece by piece, as `snprintf` into a
/// buffer of `capacity` bytes does: what does not fit is cut, and the pieces behind it are left out.
struct BoundedLine {
    text: String,
    /// The length the line would have without the cut.
    wanted: usize,
    capacity: usize,
}

impl BoundedLine {
    fn new(capacity: usize) -> BoundedLine {
        BoundedLine { text: String::new(), wanted: 0, capacity }
    }

    fn full(&self) -> bool {
        self.wanted >= self.capacity
    }

    fn push(&mut self, piece: &str) {
        let room = (self.capacity - 1).saturating_sub(self.text.len());
        let mut end = piece.len().min(room);
        while !piece.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&piece[..end]);
        self.wanted += piece.len();
    }
}

/// What the buffers of a decoder are sized by (`resource_config` of the C++).
struct ResourceConfig {
    bitdepth: i32,
    width: i32,
    height: i32,
    fb_count: i32,
    ref_fb_count: i32,
    gpu_pipeline_depth: i32,
    enable_superres: i32,
    semi_planar: bool,
    /// `AV1_LOW_MEMORY_*`.
    low_memory: i32,
    /// The parts are the default of the decoder's size ([`default_low_memory`]), not an application's.
    low_memory_default: bool,
    /// `AV1_PARSER_OPTION_TEST_POOL_WAIT`: the test switch of the pools (`pool.rs`).
    pool_test_wait: bool,
}

/// The size up to which a decoder keeps the mode info pool of the worst case by default: the buffers of
/// a decoder for frames up to 1920x1088, which are sized for 1920x1152 (rounded up to 128, as
/// `allocate_buffers` does): 540 cells.
const FULL_MODE_INFO_POOL_SAMPLES: u64 = 1920 * 1152;

/// The low memory parts of a decoder whose configuration asks for the default (`low_memory` 0): the
/// shared back buffer, and for a decoder above 1920x1088 every part: the mode info pool at half, the
/// coefficient pool at four fifths and the frame buffer bound at the recording (`AV1_LOW_MEMORY_DEFAULT`;
/// the last two measured on the console with no cost in time, docs/memory.md). Memory is short at 4K, where half the pool is 55.8 MiB and the frames are
/// film, which uses a fifth of it at most; up to 1080p the whole pool is 29.5 MiB or less, and the
/// streams that come near the worst case (lossless, low quantizers: 58 % in the conformance set) are of
/// such sizes. Parts an application names are taken as they are, at every size.
fn default_low_memory(width: u32, height: u32) -> i32 {
    let rounded = |side: u32| u64::from(side).next_multiple_of(128);
    if rounded(width) * rounded(height) > FULL_MODE_INFO_POOL_SAMPLES {
        AV1_LOW_MEMORY_DEFAULT as i32
    } else {
        AV1_LOW_MEMORY_SHARED_BACK_BUFFER as i32
    }
}

/// Whether the frames of a decoder with these parts take their frame buffer when they are recorded:
/// `AV1_LOW_MEMORY_LATE_FRAME_BUFFER`, in a decoder with the shared back buffer. A decoder whose frames
/// take a back buffer from the pool as well is left as it is (docs/memory.md).
fn late_frame_buffer(low_memory: i32) -> bool {
    let both = (AV1_LOW_MEMORY_LATE_FRAME_BUFFER | AV1_LOW_MEMORY_SHARED_BACK_BUFFER) as i32;
    low_memory & both == both
}

/// The chunks of the pools of a decoder for frames of `cells` 64x64 areas: the chunks the frames in
/// parse share under the rule of `Pools::take_growing`, and all of them.
///
/// A cell takes at most one chunk of records (AV1 5.9.15 and 5.11). The coefficient pool starts
/// with an extra thirty-second outside the rule, since the coefficients of a transform block
/// cannot be split across pieces.
///
/// `AV1_LOW_MEMORY_MODE_INFO` halves the mode info pool (the default of a decoder above 1920x1088,
/// [`default_low_memory`]); `AV1_LOW_MEMORY_COEFS` starts the coefficient pool at four fifths before
/// the extra chunks (12.0 MiB saved at 3840x2176; docs/memory.md, candidate 6). The pools grow
/// only when the oldest frame cannot take another piece (pool_buffers.rs).
fn pool_chunks(cells: usize, low_memory: i32) -> ([usize; KINDS], [usize; KINDS]) {
    let mut rule = [cells.max(1); KINDS];
    if low_memory & AV1_LOW_MEMORY_MODE_INFO as i32 != 0 {
        rule[Kind::ModeInfo as usize] = cells.div_ceil(2).max(1);
    }
    if low_memory & AV1_LOW_MEMORY_COEFS as i32 != 0 {
        rule[Kind::Coefs as usize] = (cells * 4).div_ceil(5).max(1);
    }
    let mut total = rule;
    total[Kind::Coefs as usize] += (cells / 32).max(1);
    (rule, total)
}

/// Pipeline slots asked for by the decoder configuration: 0 is the default, anything else is clamped to
/// what the arrays are sized for.
fn frame_slots_from_cfg(cfg: &aom_codec_dec_cfg_t) -> i32 {
    if cfg.frame_slots == 0 {
        return FrameThreadDataDefault;
    }
    FrameThreadDataDefault.max(FrameThreadDataCount.min(cfg.frame_slots))
}

/// The most a decoder parses at once (`AV1_FRAMES_IN_PARSE_MAX`).
const FRAMES_IN_PARSE_MAX: i32 = crate::sys::AV1_FRAMES_IN_PARSE_MAX as i32;
/// The frame size up to which the defaults are one thread and one frame in parse
/// (docs/phase2-rav1d.md, "How many threads"): 2560x1440. A decoder for larger frames (4K) has two
/// threads and, where its slots fit, two frames in parse.
const SMALL_FRAME_SAMPLES: u64 = 2560 * 1440;

/// How a decoder parses: the threads, the frames in parse and the pipeline slots, with the defaults of
/// `aom_codec_dec_cfg_t` resolved (`threads`, `frames_in_parse`, `frame_slots` of `aom/aom_decoder.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsePlan {
    threads: i32,
    frames: i32,
    slots: i32,
}

impl ParsePlan {
    /// What the configuration asks for, before the budget guard has a say.
    fn from_cfg(cfg: &aom_codec_dec_cfg_t) -> ParsePlan {
        let large = u64::from(cfg.width) * u64::from(cfg.height) > SMALL_FRAME_SAMPLES;
        // Above 2560x1440: 4 threads, the most a decoder uses (AGENTS.md, CPU: no gain beyond 4 threads;
        // on the console 6 threads parse no faster than 4).
        let threads = match cfg.threads {
            0 if large => 4,
            0 => 1,
            threads => threads.min(super::types::EntropyThreadCount as u32) as i32,
        };
        let frames = if cfg.frames_in_parse <= 0 {
            // The default above 2560x1440: as many frames in parse as threads, at most
            // FRAMES_IN_PARSE_MAX, when the slots are not fixed at two (with two slots a third frame
            // begins only when the first one is through on the GPU). The parse of one tile is serial
            // (AV1 7.20: one tile's entropy decoder), and 4K streams are mostly one tile, so frames in
            // parse are what spread it over the cores; most frames take their CDFs from a frame 3 or 4
            // back, not the one before. Console, idle state, 4 threads, 2 -> 4 frames in parse: a 100
            // Mbps 4K60 stream 53.4 -> 63.6 fps, 46 Mbps 4K50 63.0 -> 68.7, 40 Mbps 4K60 75.2 -> 80.4;
            // the device buffers stay 488.4 MiB (the pools and slots do not grow with it). A frame in
            // parse holds a slot: never more of them than slots (four by default above 2560x1440).
            if large && threads >= 2 && (cfg.frame_slots == 0 || cfg.frame_slots >= 3) {
                let slots = if cfg.frame_slots == 0 { FrameThreadDataCount } else { frame_slots_from_cfg(cfg) };
                threads.min(FRAMES_IN_PARSE_MAX).min(slots)
            } else {
                1
            }
        } else {
            cfg.frames_in_parse.min(FRAMES_IN_PARSE_MAX).min(threads)
        };
        let slots = match cfg.frame_slots {
            0 if large => FrameThreadDataCount,
            0 => (frames + 1).clamp(FrameThreadDataDefault, FrameThreadDataCount),
            _ => frame_slots_from_cfg(cfg),
        };
        ParsePlan { threads, frames, slots }
    }

    /// The next smaller plan the budget guard falls back to when a decoder would not leave the
    /// headroom: one slot less, with the frames in parse that slot count has by default at most (one
    /// less than slots); below the two slots every decoder has, one frame in parse. `None` for the
    /// smallest decoder.
    fn smaller(self) -> Option<ParsePlan> {
        if self.slots > FrameThreadDataDefault {
            let slots = self.slots - 1;
            Some(ParsePlan { threads: self.threads, frames: self.frames.min(slots - 1).max(1), slots })
        } else if self.frames > 1 {
            Some(ParsePlan { frames: 1, ..self })
        } else {
            None
        }
    }

    /// "2 frames in parse on 2 threads with 3 pipeline slots", for the log.
    fn words(self) -> String {
        format!(
            "{} frame{} in parse on {} thread{} with {} pipeline slots",
            self.frames,
            if self.frames == 1 { "" } else { "s" },
            self.threads,
            if self.threads == 1 { "" } else { "s" },
            self.slots
        )
    }
}

/// The headroom of a configuration (`aom_codec_dec_cfg_t::memory_headroom`): 0 is the default, and no
/// value is below the margin a creation is refused for.
fn headroom_from_cfg(cfg: &aom_codec_dec_cfg_t) -> u64 {
    if cfg.memory_headroom == 0 { MEMORY_GUARD_HEADROOM } else { (cfg.memory_headroom as u64).max(MEMORY_GUARD_MARGIN) }
}

/// The decoder the budget guard creates: of the device bytes of a plan and of the smaller plans behind
/// it (`sizes`, falling), the index of the first that leaves `headroom` of what the application has
/// `available`, or the last one when none does. The headroom never refuses; the margin does, afterwards.
fn first_with_headroom(sizes: &[u64], available: u64, headroom: u64) -> usize {
    sizes.iter().position(|planned| planned + headroom <= available).unwrap_or(sizes.len() - 1)
}

impl ResourceConfig {
    /// The same for the memory query and for the creation: both must count the same allocations. (The
    /// creation may then be made with fewer slots, when the budget guard says that those of more than
    /// one frame in parse do not fit: it takes less of everything.)
    fn from_cfg(cfg: &aom_codec_dec_cfg_t) -> ResourceConfig {
        Self::with_slots(cfg, ParsePlan::from_cfg(cfg).slots)
    }

    fn with_slots(cfg: &aom_codec_dec_cfg_t, gpu_pipeline_depth: i32) -> ResourceConfig {
        // 0 is the default (the shared back buffer); AV1_LOW_MEMORY_NONE is how an application asks for none.
        let low_memory = if cfg.low_memory != 0 {
            cfg.low_memory & AV1_LOW_MEMORY_PARTS as i32
        } else {
            default_low_memory(cfg.width, cfg.height)
        };
        // Frame buffers in the pool. A buffer is out of the pool while the entropy decoder or a frame in
        // flight holds it:
        //   - the reference map: at most 8 different buffers;
        //   - the frame being decoded: 1, taken while its header is read (av1_reallocate_frame_buffer);
        //   - its back buffer, when the frame is filtered after its reconstruction (CDEF, loop restoration,
        //     superres): 1, taken in av1_setup_frame();
        //   - a frame that failed after av1_setup_frame(): its buffer and its back buffer stay with its slot
        //     until the next av1_setup_frame(), which comes after the next frame has taken its own buffer: 2;
        //   - every other frame in flight, until its GPU work is done: its back buffer, and what the map has
        //     dropped since the frame started (its own buffer when it is not a reference, a reference it
        //     reads that a later frame replaced): 1 + 1 when frames replace one map entry each.
        // Both takes wait until a frame in flight returns a buffer, so what must never be undercut is the
        // count with nothing in flight: 8 + 2 + 1 = 11 (map, failed frame, the next frame's buffer). Below
        // that the decode thread waits for a buffer nobody returns. The 12 of the code before this is 8 for
        // the map, 1 + 1 for the frame being decoded and 1 + 1 for the one frame that is in flight next to it
        // with two slots: no wait in the common case. It is kept, and so is the one buffer more for each slot
        // above two.
        // With the shared back buffer no frame takes a back buffer from the pool: the count with nothing in
        // flight is 8 + 1 + 1 = 10 (map, failed frame, next frame), and the common case with two slots is
        // also 8 + 1 + 1 = 10 (map, frame being decoded, one dropped reference of the frame in flight).
        let base_fb_count = if low_memory & AV1_LOW_MEMORY_SHARED_BACK_BUFFER as i32 != 0 {
            RefFrameBufferCountSharedBack
        } else {
            RefFrameBufferCount
        };
        // With the frame buffer bound at the recording (AV1_LOW_MEMORY_LATE_FRAME_BUFFER, which has an
        // effect only together with the shared back buffer: `late_frame_buffer`) a frame in parse holds
        // no buffer of the pool (the thread that records the frames takes it,
        // `frame::bind_frame_for_record`), so the buffer for each slot above two is left out. What is
        // then out of the pool when the frame at the head of the queue asks for its buffer: the buffers
        // of frames that are recorded and that the map or a frame of the queue refers to, which are at
        // most 8 (every one of them was in the map when the frame at the head began, and the frames that
        // began since have no buffer yet), and what only frames on the GPU hold, which comes back
        // without this thread. So the 8 and the frame's own are always within the 10: the take waits for
        // the GPU at most, never for a frame behind it. The count is not taken below the 10 of a decoder
        // with two slots (docs/memory.md).
        let late = late_frame_buffer(low_memory);
        let ref_fb_count =
            if late { base_fb_count } else { base_fb_count + gpu_pipeline_depth - FrameThreadDataDefault };
        ResourceConfig {
            bitdepth: cfg.bitdepth as i32,
            width: cfg.width as i32,
            height: cfg.height as i32,
            fb_count: ref_fb_count,
            ref_fb_count,
            gpu_pipeline_depth,
            enable_superres: 1,
            semi_planar: cfg.output_kind == 1,
            low_memory,
            low_memory_default: cfg.low_memory == 0,
            pool_test_wait: cfg.parser_options & crate::sys::AV1_PARSER_OPTION_TEST_POOL_WAIT as i32 != 0,
        }
    }
}

/// Who serves the requests of [`allocate_buffers`]: the allocator of the decoder with the decoder's
/// device, or the plan that only counts what the requests would take (`av1_memory_manager_base` of the
/// C++).
enum Requests {
    Allocator(*mut Allocator, ID3D12Device),
    Plan(Plan),
}

impl Requests {
    fn plan() -> Requests {
        Requests::Plan(Plan::default())
    }

    /// The host and device bytes a plan has counted.
    fn planned(&self) -> (usize, usize) {
        match self {
            Requests::Plan(plan) => (plan.host, plan.device),
            Requests::Allocator(..) => (0, 0),
        }
    }

    /// `create_buffer`: null from the plan, and from the allocator when the allocation failed.
    fn buffer(&mut self, size: usize, kind: MemoryType, name: &'static CStr, slot: i32) -> *mut GpuBufferObject {
        self.buffer_for(size, kind, name, slot, false)
    }

    /// [`Requests::buffer`] for the grid of a slot, which with `gpu_writable` a command list may write too
    /// (`Av1Core::gpu_grid`), and for the lists of CDEF (`Av1Core::gpu_cdef_lists`). The plan counts
    /// the same either way.
    fn buffer_for(
        &mut self,
        size: usize,
        kind: MemoryType,
        name: &'static CStr,
        slot: i32,
        gpu_writable: bool,
    ) -> *mut GpuBufferObject {
        match self {
            // SAFETY: the allocator of the decoder that is being created, and its device.
            Requests::Allocator(memory, device) => unsafe {
                (**memory).create_buffer(Some(device), size, kind, name, slot, gpu_writable)
            },
            Requests::Plan(plan) => {
                plan.create_buffer(size);
                std::ptr::null_mut()
            }
        }
    }

    /// `host_allocate`: null from the plan, and from the allocator when the pool is too small.
    fn host(&mut self, size: usize) -> *mut c_void {
        match self {
            // SAFETY: the allocator of the decoder that is being created.
            Requests::Allocator(memory, _) => unsafe { (**memory).host_allocate(size, 0) },
            Requests::Plan(plan) => {
                plan.host_allocate(size, 0);
                std::ptr::null_mut()
            }
        }
    }
}

/// `CHECK_RESULT` of the C++: the request is always made; its result is kept, and a null result ends the
/// creation, only when the requests are served by the allocator.
macro_rules! request {
    ($assign:expr, $dst:expr, $value:expr) => {{
        let value = $value;
        if $assign {
            if value.is_null() {
                return false;
            }
            $dst = value.cast();
        }
    }};
}

/// `av1_allocate_buffers`: every buffer and every piece of host memory of a decoder, in one order for the
/// plan and for the creation. The sizes the decoder remembers are written to `dec` in both. Returns
/// `false` when a request of the allocator failed (it has logged which).
///
/// # Safety
/// `dec` is valid for writing. With the allocator: `dec` is the decoder that is being created, zeroed.
unsafe fn allocate_buffers(dec: *mut Av1Core, mem: &mut Requests, cfg: &ResourceConfig) -> bool {
    let assign = matches!(mem, Requests::Allocator(..));
    // SAFETY: the caller's contract.
    let dec = unsafe { &mut *dec };
    let target_width = (cfg.width + 127) & !127;
    let target_height = (cfg.height + 127) & !127;
    let block_count_4x4 = target_width * target_height * 3 / (2 * 4 * 4);
    let mi_cols = target_width >> 2;
    let mi_rows = target_height >> 2;

    let max_tile_cols = target_width >> 6;
    let max_tile_rows = target_height >> 6;
    dec.block_count4x4 = block_count_4x4;
    let gpu_cdef_lists = dec.gpu_cdef_lists != 0;
    let gpu_grid = dec.gpu_grid != 0;
    dec.max_width = target_width;
    dec.max_height = target_height;
    dec.max_bitdepth = cfg.bitdepth;

    // AV1 5.9.20 allows 64x64 luma restoration units and, with lr_uv_shift,
    // 32x32 chroma units in 4:2:0. Both grids can have one unit per 64x64 luma area.
    let max_rst_units_per_plane = (max_tile_cols * max_tile_rows) as usize;
    let max_rst_units = 3 * max_rst_units_per_plane;
    let unit = size_of::<RestorationUnitInfo>();
    for units in &mut dec.restoration_info_alloc {
        request!(assign, *units, mem.host(unit * max_rst_units_per_plane));
    }

    let [_, (_, u), (_, v)] = super::residual::plane_layout(target_width, target_height);
    request!(
        assign,
        dec.idct_residuals,
        mem.buffer(super::residual::frame_bytes(u, v), DeviceOnly, c"idct_residuals", -1)
    );
    // The buffers that grow with the frames (stage_buffers.rs): a part of the worst case, which is one
    // transform block and two prediction blocks for every 4x4 unit and a warp block for every 8x8.
    // The block index list of a slot (the last one): two entries for every 4x4 unit.
    let mut stage_worst = [
        (block_count_4x4 * IDCT_BLOCK_SIZE) as usize,
        (block_count_4x4 * 16 * 2) as usize,
        ((block_count_4x4 >> 2) * 48) as usize,
        size_of::<i32>() * block_count_4x4 as usize * 2,
        0,
    ];
    request!(
        assign,
        dec.idct_blocks,
        mem.buffer(initial_bytes(stage_worst[IDCT_BLOCKS]), DeviceOnly, c"idct_blocks", -1)
    );
    let (wedge_mask_bytes, warped_filter_bytes) =
        (super::libaom::WEDGE_MASK_BUF_BYTES, size_of_val(&super::libaom::WARPED_FILTER));
    request!(assign, dec.inter_mask_lut, mem.buffer(wedge_mask_bytes * 2, DeviceOnlyConst, c"inter_mask_lut", -1));
    request!(
        assign,
        dec.inter_warp_filter,
        mem.buffer(warped_filter_bytes * 2, DeviceOnlyConst, c"inter_warp_filter", -1)
    );

    // Frame buffer size.
    let border = 16;
    let y_stride = target_width + 2 * border;
    let y_size = y_stride * target_height;
    let uv_stride = (target_width >> 1) + border * 2;
    let uv_size = uv_stride * (target_height >> 1);
    let hbd = cfg.bitdepth > 8;
    let bpp = if hbd { 2 } else { 1 };
    let fb_size = ((y_size + 2 * uv_size) * bpp + 255) & !255;

    // The pool buffer starts with the back buffers that are not handed out: back_buffer1, and behind it the
    // back buffer the frames share when they do not take one from the pool.
    dec.shared_back_buffer = i32::from(cfg.low_memory & AV1_LOW_MEMORY_SHARED_BACK_BUFFER as i32 != 0);
    dec.low_memory = cfg.low_memory;
    dec.late_frame_buffer = i32::from(late_frame_buffer(cfg.low_memory));
    dec.fb_size = fb_size;
    // On narrow frames the output row pitch can be larger than the decoded row pitch.
    // Keep the entire staging layout ahead of the other frames in the pool.
    let staging_size = super::output::staging_size(target_width, target_height, hbd, cfg.semi_planar);
    let back_buffer1_size = fb_size.max(staging_size);
    dec.back_buffer1.size = back_buffer1_size as usize;
    dec.fb_offset = back_buffer1_size + dec.shared_back_buffer * fb_size;
    dec.enable_superres = cfg.enable_superres;
    request!(
        assign,
        dec.frame_buffer_pool,
        mem.buffer(
            fb_size as usize * cfg.fb_count as usize + dec.fb_offset as usize,
            DeviceOnly,
            c"frame_buffer_pool",
            -1
        )
    );

    let grid_w = mi_cols + 2 + 128;
    let grid_h = mi_rows + 2 + 128;
    let cdef_blocks = target_width * target_height / 64 / 64;

    let mi_size = mi_cols * mi_rows;
    let lf_blk_count = (target_width / 64) * (target_height / 4)      // vertical edges, luma
        + (target_width / 128) * (target_height / 8) * 2              // vertical edges, chroma
        + (target_height / 64) * (target_width / 4)                   // horizontal edges, luma
        + (target_height / 128) * (target_width / 8) * 2; // horizontal edges, chroma

    let max_tiles = max_tile_cols * max_tile_rows;
    if gpu_grid {
        stage_worst[GRID_RUNS] = RECORD[GRID_RUNS] * (max_tiles as usize + 1);
    }
    dec.stage_worst = stage_worst;

    dec.pred_map_size = (256 + 32) * max_tiles + (mi_size >> 2) * 10;
    request!(
        assign,
        dec.prediction_blocks,
        mem.buffer(initial_bytes(stage_worst[PRED_BLOCKS]), DeviceOnly, c"prediction_blocks", -1)
    );
    request!(
        assign,
        dec.prediction_blocks_warp,
        mem.buffer(initial_bytes(stage_worst[PRED_BLOCKS_WARP]), DeviceOnly, c"prediction_blocks_warp", -1)
    );
    request!(
        assign,
        dec.loopfilter_blocks,
        mem.buffer((lf_blk_count * 32) as usize, DeviceOnly, c"loopfilter_blocks", -1)
    );
    // The pools (pool.rs; docs/phase2-rav1d.md, step 5): one buffer each for the mode info records, the
    // coefficients and the transform block records of all frames in flight, in chunks of what a 64x64
    // area needs at worst. A frame has at most one mode info record per 4x4 unit and 24 coefficients
    // (16 luma + 2 * 4 chroma) per 4x4 unit, a little more with their counts; a pool holds that for
    // one frame, or less with the low memory parts. No slot has a share of them.
    let (rule, total) = pool_chunks(max_tiles as usize, cfg.low_memory);
    dec.pool_rule = rule;
    dec.pool_total = total;
    request!(
        assign,
        dec.mode_info_pool,
        mem.buffer(
            size_of::<MB_MODE_INFO>() * MODE_INFO_CHUNK * total[Kind::ModeInfo as usize],
            HostRW,
            c"mode_info_pool",
            -1
        )
    );
    request!(
        assign,
        dec.idct_coefs,
        mem.buffer(size_of::<i32>() * COEF_CHUNK * total[Kind::Coefs as usize], UploadUAV, c"idct_coefs", -1)
    );
    request!(
        assign,
        dec.idct_blocks_pool,
        mem.buffer(
            IDCT_BLOCK_SIZE as usize * TXB_CHUNK * total[Kind::Txb as usize],
            DeviceUpload,
            c"idct_blocks_unordered",
            -1
        )
    );
    // The grain buffer of a slot or output context: the templates the CPU makes (37,320 bytes,
    // `film_grain.rs`), then the random offsets of the 32x32 blocks of the largest frame.
    let grain_offsets = super::film_grain::GRAIN_BUFFER_TEMPLATE_BYTES
        + size_of::<i32>() * ((target_width / 32 + 4) * (target_height / 32 + 1)) as usize;
    for i in 0..cfg.gpu_pipeline_depth {
        let td = &mut dec.frame_thread_data[i as usize];
        td.slot_index = i;
        request!(assign, td.command_buffer.cb_alloc, mem.buffer(1024 * 1024, DeviceUpload, c"cb_alloc", i));
        request!(assign, td.tile_data, mem.host(size_of::<av1_tile_data>() * max_tiles as usize));
        request!(
            assign,
            td.gen_mi_block_indexes,
            mem.buffer(initial_bytes(stage_worst[INDEX_LIST]), HostRW, c"gen_mi_block_indexes", i)
        );
        request!(
            assign,
            td.gen_intra_inter_grid,
            mem.buffer((grid_w * grid_h * 6) as usize, HostRW, c"gen_intra_inter_grid", i)
        );
        request!(
            assign,
            td.gen_block_map,
            mem.buffer(dec.pred_map_size as usize * size_of::<i32>(), HostRW, c"gen_block_map", i)
        );
        td.wave_edges = 0;
        td.clear_pending = 0;
        td.clear_iter_bytes = 0;
        td.clean_iter_bytes = 0;
        td.kept_grid = [0; 3];
        td.kept_grid_bytes = 0;
        request!(
            assign,
            td.mode_info_grid,
            mem.buffer_for(
                size_of::<crate::sys::MiGridEntry>() * mi_size as usize,
                HostRW,
                c"mode_info_grid",
                i,
                gpu_grid
            )
        );
        // In a decoder whose grid a pass of the command list builds (`Av1Core::gpu_grid`), the runs of
        // the frame's records, which the thread that records writes and the pass reads: at most one a
        // chunk of the mode info pool.
        if gpu_grid {
            request!(
                assign,
                td.mi_runs,
                mem.buffer(RECORD[GRID_RUNS] * (dec.pool_total[Kind::ModeInfo as usize] + 1), HostRW, c"mi_runs", i)
            );
        }
        // The two lists of CDEF: written by the thread that records a frame, or, in a decoder for it
        // (`Av1Core::gpu_cdef_lists`), by a pass of the frame's command list: then buffers the CPU has a
        // cached mapping of (which nothing but `-poisonbuf` writes through) and a command list may write.
        let (cdef_kind, cdef_writable) = if gpu_cdef_lists { (HostRW, true) } else { (DeviceUpload, false) };
        request!(
            assign,
            td.cdef_indexes,
            mem.buffer_for(size_of::<i32>() * cdef_blocks as usize, cdef_kind, c"cdef_indexes", i, cdef_writable)
        );
        request!(
            assign,
            td.cdef_skips,
            mem.buffer_for(
                size_of::<i32>() * cdef_blocks as usize * 16 * 16,
                cdef_kind,
                c"cdef_skips",
                i,
                cdef_writable
            )
        );
        request!(
            assign,
            td.cdef_masks,
            mem.buffer_for(size_of::<u32>() * 2 * cdef_blocks as usize, cdef_kind, c"cdef_masks", i, cdef_writable)
        );
        request!(
            assign,
            td.loop_rest_types,
            mem.buffer(size_of::<[c_int; 4]>() * max_rst_units, DeviceUpload, c"loop_rest_types", i)
        );
        request!(
            assign,
            td.loop_rest_wiener,
            mem.buffer(size_of::<[c_int; 16]>() * max_rst_units, DeviceUpload, c"loop_rest_wiener", i)
        );
        request!(
            assign,
            td.filmgrain_rand_offset,
            mem.buffer(grain_offsets, DeviceUpload, c"filmgrain_rand_offset", i)
        );

        // No palette buffer here: the frame setup creates the slot's on the first frame that can have
        // palette blocks.
    }
    // Output contexts (av1_decode_sef): the constants of one output copy and the film grain offsets of a
    // frame.
    for i in 0..OutputThreadDataCount as usize {
        let td = &mut dec.output_thread_data[i];
        td.output_only = 1;
        td.slot_index = -1;
        request!(
            assign,
            td.command_buffer.cb_alloc,
            mem.buffer(OutputCbSize as usize, DeviceUpload, c"output_cb_alloc", -1)
        );
        request!(
            assign,
            td.filmgrain_rand_offset,
            mem.buffer(grain_offsets, DeviceUpload, c"output_filmgrain_rand_offset", -1)
        );
    }
    dec.palette_size = fb_size as usize;

    true
}

/// What the application has left for the decoder (`memory_available_cb`), for a creation that needs
/// `needed` bytes: the first answer, the last one and the milliseconds between them; -1 without a
/// callback or when the callback does not know. An answer that is too small is not taken at once: the
/// system gives the memory of released D3D12 buffers back some time after the release (a decoder that was
/// destroyed just before this creation), so the answer is asked again for as long as it keeps growing.
/// Bounded: AV1_MEMORY_GUARD_WAIT_MS, and AV1_MEMORY_GUARD_SETTLE_MS without a gain of 1 MiB ends it.
///
/// # Safety
/// The callback is the application's and `private` what it takes.
unsafe fn settled_budget(
    callback: Option<unsafe extern "C" fn(*mut c_void) -> i64>,
    private: *mut c_void,
    needed: u64,
) -> (i64, i64, u32) {
    let Some(callback) = callback else {
        return (-1, -1, 0);
    };
    // SAFETY: the caller's contract, for every call below.
    let first = unsafe { callback(private) };
    let mut available = first;
    let mut best = available;
    let mut waited_ms = 0u32;
    let mut settle_ms = 0u32;
    while available >= 0
        && needed > available as u64
        && waited_ms < AV1_MEMORY_GUARD_WAIT_MS
        && settle_ms < AV1_MEMORY_GUARD_SETTLE_MS
    {
        std::thread::sleep(Duration::from_millis(u64::from(GUARD_POLL_MS)));
        waited_ms += GUARD_POLL_MS;
        // SAFETY: as above.
        available = unsafe { callback(private) };
        if available < 0 {
            break;
        }
        if available >= best + (1 << 20) {
            best = available;
            settle_ms = 0;
        } else {
            settle_ms += GUARD_POLL_MS;
        }
    }
    (first, available, waited_ms)
}

/// The bytes a decoder may add to its device buffers later, when a stream needs them: a palette buffer
/// for each slot, the stage buffer growth, and the peak while the replacement pools coexist with
/// the initial buffers.
fn deferred_device_bytes(dec: &Av1Core, rcfg: &ResourceConfig) -> usize {
    let slots = rcfg.gpu_pipeline_depth as usize;
    slots * ((dec.palette_size + 255) & !255) + stage_growth_bytes(dec, slots) + super::pool_buffers::growth_bytes(dec)
}

fn configure_parser_buffers(dec: &mut Av1Core, bits: c_int) {
    let options = crate::ParserOptions::from_bits(bits as u32);
    dec.parser_options = bits;
    dec.gpu_grid = c_int::from(options.gpu_grid);
    // A GPU grid is only available to the GPU. CDEF takes its lists there.
    dec.gpu_cdef_lists = c_int::from(options.gpu_cdef_lists || options.gpu_grid);
}

/// What the buffers of the stages that grow with the frames can add to what they are created with: the
/// block index list in each of the `slots`.
fn stage_growth_bytes(dec: &Av1Core, slots: usize) -> usize {
    let aligned = |bytes: usize| (bytes + 255) & !255;
    let growth = |index: usize| {
        let worst = dec.stage_worst[index];
        aligned(worst) - aligned(initial_bytes(worst))
    };
    growth(IDCT_BLOCKS)
        + growth(PRED_BLOCKS)
        + growth(PRED_BLOCKS_WARP)
        + slots
            * (growth(INDEX_LIST)
                + aligned(dec.stage_worst[GRID_RUNS])
                    .saturating_sub(aligned(RECORD[GRID_RUNS] * (dec.pool_total[Kind::ModeInfo as usize] + 1))))
}

/// The device bytes [`allocate_buffers`] creates for `rcfg`, and the bytes of the buffers that are created
/// later. `dec` is only written where the creation writes the same values.
///
/// # Safety
/// `dec` is valid for writing.
unsafe fn planned_device_bytes(dec: *mut Av1Core, rcfg: &ResourceConfig) -> (usize, usize) {
    let mut plan = Requests::plan();
    // SAFETY: the caller's contract; a plan assigns nothing.
    unsafe {
        allocate_buffers(dec, &mut plan, rcfg);
        (plan.planned().1, deferred_device_bytes(&*dec, rcfg))
    }
}

/// What the host pool is aligned to: the allocator starts at the first such address of its memory.
const HOST_POOL_ALIGNMENT: usize = 256;

/// What a decoder takes of the heap of the process besides its host pool: the decoder itself and its
/// allocator ([`Av1Core::own_bytes`]). For the memory query of the Rust interface, which reports all host
/// memory of a decoder.
pub(crate) const fn decoder_own_bytes() -> usize {
    Av1Core::own_bytes()
}

/// `av1_query_memory_requirements` (`dx/av1_core.h`): what a decoder of this configuration takes of the
/// host pool and of device memory. The host pool is what the entropy decoder is given; the decoder itself
/// is not in it ([`decoder_own_bytes`]).
///
/// # Safety
/// `cfg` is the configuration of a decoder.
pub(crate) unsafe extern "C" fn av1_query_memory_requirements(cfg: *mut aom_codec_dec_cfg_t) -> c_int {
    // SAFETY: the caller's contract.
    let cfg = unsafe { &mut *cfg };
    let mut plan = Requests::plan();
    // Room for the pool to start at a multiple of 256 in the memory it is given.
    plan.host(HOST_POOL_ALIGNMENT);
    let rcfg = ResourceConfig::from_cfg(cfg);
    // A decoder for the sizes the plan writes; nothing else of it is used.
    let Some(mut dec) = Av1Core::new() else {
        return -1;
    };
    configure_parser_buffers(&mut dec, cfg.parser_options);
    // SAFETY: a plan assigns nothing.
    unsafe { allocate_buffers(&raw mut *dec, &mut plan, &rcfg) };
    let (host, device) = plan.planned();
    cfg.host_size = host;
    cfg.device_size = device;
    cfg.device_size_deferred = deferred_device_bytes(&dec, &rcfg);
    0
}

/// "MEMORY" lines about what the decoder holds: the buffers by memory type, the largest ones, the host
/// pool, and what one pipeline slot takes. After a creation, and at the point where a creation failed.
///
/// # Safety
/// `dec` is a decoder with its allocator.
unsafe fn log_memory_summary(is_error: bool, label: &str, dec: &Av1Core, rcfg: &ResourceConfig) {
    // SAFETY: the allocator of the decoder (the caller's contract).
    let mem = unsafe { &*dec.memory };
    let buffers = mem.buffers();
    let total = mem.device_bytes();
    let shared = dec.shared_back_buffer != 0;
    let staging_extra = dec.back_buffer1.size.saturating_sub(dec.fb_size as usize);
    let staging_extra =
        if staging_extra == 0 { String::new() } else { format!(", plus {staging_extra} bytes for output staging") };
    log(
        is_error,
        &format!(
            "MEMORY {label}: {}x{} {}-bit, slots {}, frame buffers {} + {} back buffer{} of {} bytes ({:.1} MiB) \
             each{staging_extra}; {} device buffers, {total} bytes ({:.1} MiB); host pool {} of {} bytes ({:.1} MiB) in use",
            rcfg.width as u32,
            rcfg.height as u32,
            rcfg.bitdepth as u32,
            rcfg.gpu_pipeline_depth,
            rcfg.ref_fb_count,
            if shared { 2 } else { 1 },
            if shared { "s" } else { "" },
            dec.fb_size,
            to_mib(dec.fb_size as u64),
            mem.dev_obj_count,
            to_mib(total as u64),
            mem.host_offset,
            mem.host_max_size,
            to_mib(mem.host_offset as u64)
        ),
    );
    // What was left out or made smaller, and what is still to come: a slot's palette buffer is created on
    // the first frame of the slot that allows screen content tools.
    let palette_buffers = dec.frame_thread_data[..rcfg.gpu_pipeline_depth as usize]
        .iter()
        .filter(|td| !td.palette_buffer.is_null())
        .count();
    // How much of the worst case of a frame each pool holds.
    let cells = ((dec.max_width >> 6) * (dec.max_height >> 6)).max(1) as usize;
    // Which mode info pool, and why: the default is by the size of the decoder.
    let why = match (rcfg.low_memory_default, rcfg.low_memory & AV1_LOW_MEMORY_MODE_INFO as i32 != 0) {
        (true, true) => " (the default: half above 1920x1088)",
        (true, false) => " (the default: whole up to 1920x1088)",
        (false, _) => " (as asked)",
    };
    let shares = format!(
        "mode info pool {}/{cells} of the worst case{why}, coefficient pool {}/{cells} and {} chunks for the ends \
         of pieces",
        dec.pool_rule[Kind::ModeInfo as usize],
        dec.pool_rule[Kind::Coefs as usize],
        dec.pool_total[Kind::Coefs as usize] - dec.pool_rule[Kind::Coefs as usize]
    );
    // Said only by a decoder that has the part, so that the line of the others is what it was.
    let late = if late_frame_buffer(rcfg.low_memory) {
        ", frame buffers bound at the recording"
    } else if rcfg.low_memory & AV1_LOW_MEMORY_LATE_FRAME_BUFFER as i32 != 0 {
        ", frame buffers not bound at the recording: that needs the shared back buffer"
    } else {
        ""
    };
    log(
        is_error,
        &format!(
            "MEMORY {label} options: low memory {} (shared back buffer {}, {shares}{late}); palette \
             buffers {palette_buffers} of {} created, {:.1} MiB each, a slot's on its first frame with screen content \
             tools",
            rcfg.low_memory,
            if shared { "yes" } else { "no" },
            rcfg.gpu_pipeline_depth,
            to_mib(dec.palette_size as u64)
        ),
    );
    {
        // The buffers of the stages that grow: what they have, and the worst case they can reach.
        let part = |object: *mut GpuBufferObject, index: usize| {
            // SAFETY: null, or a buffer of the decoder.
            let size = unsafe { object.as_ref() }.map_or(0, |object| object.size);
            format!("{:.1} of {:.1} MiB", to_mib(size as u64), to_mib(dec.stage_worst[index] as u64))
        };
        log(
            is_error,
            &format!(
                "MEMORY {label} stage buffers: idct_blocks {}, prediction_blocks {}, prediction_blocks_warp {}, \
                 gen_mi_block_indexes {} a slot: a part of the worst case, grown before the frame that writes \
                 more is recorded (up to {:.1} MiB more in all, held against the budget with a margin of {:.1} \
                 MiB; a growth that is refused is a decode error of the frame)",
                part(dec.idct_blocks, IDCT_BLOCKS),
                part(dec.prediction_blocks, PRED_BLOCKS),
                part(dec.prediction_blocks_warp, PRED_BLOCKS_WARP),
                part(dec.frame_thread_data[0].gen_mi_block_indexes, INDEX_LIST),
                to_mib(stage_growth_bytes(dec, rcfg.gpu_pipeline_depth as usize) as u64),
                to_mib(super::stage_buffers::MEMORY_GUARD_MARGIN_BUFFER)
            ),
        );
    }
    {
        // The pools: what the frames in flight share.
        let size = |object: *mut GpuBufferObject| {
            // SAFETY: null, or a buffer of the decoder.
            to_mib(unsafe { object.as_ref() }.map_or(0, |object| object.size) as u64)
        };
        log(
            is_error,
            &format!(
                "MEMORY {label} pools: mode info {} chunks of {MODE_INFO_CHUNK} records, {:.1} MiB; coefficients {} \
                 chunks of {COEF_CHUNK}, {:.1} MiB; transform block records {} chunks of {TXB_CHUNK}, {:.1} MiB; a \
                 chunk is what a 64x64 area needs at worst, and the frames in flight take them in pieces as they \
                 are parsed{}",
                dec.pool_total[Kind::ModeInfo as usize],
                size(dec.mode_info_pool),
                dec.pool_total[Kind::Coefs as usize],
                size(dec.idct_coefs),
                dec.pool_total[Kind::Txb as usize],
                size(dec.idct_blocks_pool),
                if rcfg.pool_test_wait {
                    " (the test switch: behind its first piece a frame finds a pool empty while an older frame \
                     holds a piece)"
                } else {
                    ""
                }
            ),
        );
    }

    // Host memory: the pool, and what the decoder allocates next to it.
    log(
        is_error,
        &format!(
            "MEMORY {label} host memory: host pool {} bytes ({:.1} MiB) for the tiles' cursors and the loop restoration units, {} of them \
             in use; the decoder itself {} bytes ({:.1} MiB) on the heap, not in the pool; both allocated before the \
             budget is asked",
            mem.host_max_size,
            to_mib(mem.host_max_size as u64),
            mem.host_offset,
            Av1Core::own_bytes(),
            to_mib(Av1Core::own_bytes() as u64)
        ),
    );

    let type_name = memory_type_name;
    let mut line = BoundedLine::new(640);
    for kind in 0..gpu::MemoryKind::COUNT {
        if line.full() {
            break;
        }
        line.push(&format!(
            "{}{} {} buffers {:.1} MiB",
            if kind != 0 { "; " } else { "" },
            type_name(kind as MemoryType),
            mem.dev_count[kind],
            to_mib(mem.dev_bytes[kind] as u64)
        ));
    }
    // The decoder creates buffers only. The output images are textures of the application.
    log(is_error, &format!("MEMORY {label} by type: {}; textures 0", line.text));

    // One slot: its own buffers. It has nothing of the pools.
    let mut slot_own = 0usize;
    let mut slot_buffers = 0;
    for obj in buffers {
        if obj.slot == 0 {
            slot_own += obj.size;
            slot_buffers += 1;
        }
    }
    log(
        is_error,
        &format!(
            "MEMORY {label} per slot: {slot_own} bytes ({:.1} MiB) in {slot_buffers} own buffers, and nothing of \
             the pools; a slot above {} also adds one frame buffer, {:.1} MiB",
            to_mib(slot_own as u64),
            FrameThreadDataDefault,
            to_mib(dec.fb_size as u64)
        ),
    );

    // The largest buffers, one entry per name: the buffers of the slots are the same in every slot.
    const LARGEST: usize = 10;
    let mut top: Vec<&GpuBufferObject> = Vec::with_capacity(LARGEST + 1);
    for obj in buffers {
        if obj.slot > 0 {
            continue;
        }
        let mut pos = top.len();
        while pos > 0 && top[pos - 1].size < obj.size {
            pos -= 1;
        }
        if pos >= LARGEST {
            continue;
        }
        top.insert(pos, obj);
        top.truncate(LARGEST);
    }
    let mut line = BoundedLine::new(640);
    for (i, obj) in top.iter().enumerate() {
        if line.full() {
            break;
        }
        // SAFETY: the name of a buffer is a static C string (create_buffer).
        let name = unsafe { CStr::from_ptr(obj.name) }.to_string_lossy();
        line.push(&format!(
            "{}{name} {:.1} MiB {}{}",
            if i != 0 { "; " } else { "" },
            to_mib(obj.size as u64),
            type_name(obj.memtype),
            if obj.slot == 0 { " (per slot)" } else { "" }
        ));
    }
    log(is_error, &format!("MEMORY {label} largest: {}", line.text));
}

/// The device of the decoder with a reference of its own, for the lines behind the decoder's release.
///
/// # Safety
/// `dec` is a decoder; its device field is null or holds a device.
unsafe fn device_of(dec: *const Av1Core) -> Option<ID3D12Device> {
    // SAFETY: the caller's contract.
    unsafe { com_ref::<_, ID3D12Device>(&(*dec).compute.device).cloned() }
}

/// A step of the decoder creation failed. Says which one, what was allocated up to there and what the
/// video memory budget is, then releases everything the creation has built, so that nothing of it is left
/// when the application tries again: the caller never gets a decoder to destroy.
///
/// # Safety
/// `dec` is the decoder that is being created.
unsafe fn create_failed(
    dec: *mut Av1Core,
    rcfg: &ResourceConfig,
    before: Option<&VideoMemory>,
    step: &str,
    hr: i32,
) -> c_int {
    // An allocation of the decoder's allocator has logged itself already and is the reason kept for the
    // application; this line is for every other step.
    if hr < 0 {
        log_create_error(&format!("MEMORY FAILED decoder creation at: {step} (hr 0x{:08X})", hr as u32));
    } else {
        log_create_error(&format!("MEMORY FAILED decoder creation at: {step}"));
    }
    // SAFETY: the caller's contract; the decoder has its allocator from the start of the creation.
    unsafe {
        log_memory_summary(true, "FAILED decoder, allocated up to the failure", &*dec, rcfg);
        // Keeps the device for the query after the release.
        let device = device_of(dec);
        if let Some(device) = &device {
            let now = Device::video_memory(Some(device));
            if let Some(before) = before {
                log_video_memory(true, "FAILED, before the creation", before);
            }
            log_video_memory(true, "FAILED, at the failure", &now);
        }
        destroy(dec, false);
        if let Some(device) = &device {
            let released = Device::video_memory(Some(device));
            log_video_memory(true, "FAILED, after releasing the failed decoder", &released);
        }
    }
    -1
}

/// `create_device` of the C++: the decoder's device (the first hardware adapter when the application gave
/// none) and its queue (a direct queue of its own when the application gave none).
///
/// # Safety
/// `dec` is the decoder that is being created; its device and queue fields are null or hold a reference.
unsafe fn create_device(dec: &mut Av1Core) -> Result<(), gpu::Error> {
    // SAFETY: the caller's contract; an empty field gets the reference of what was created.
    unsafe {
        if com_ref::<_, ID3D12Device>(&dec.compute.device).is_none() {
            com_store(&raw mut dec.compute.device, create_hardware_device()?.into_raw());
        }
        let device = com_ref::<_, ID3D12Device>(&dec.compute.device).expect("the device was stored");
        if com_ref::<_, ID3D12CommandQueue>(&dec.compute.queue).is_none() {
            com_store(&raw mut dec.compute.queue, Device::new(device).create_queue()?.into_raw());
        }
        let queue = com_ref::<_, ID3D12CommandQueue>(&dec.compute.queue).expect("the queue was stored");
        com_store(&raw mut dec.compute.queue_direct, queue.clone().into_raw());
    }
    Ok(())
}

/// The command allocator, the fence and the event of a slot or of an output context.
///
/// # Safety
/// `td` is a slot of the decoder that is being created; its fields are empty.
unsafe fn create_command_buffer(
    device: Device<'_>,
    td: &mut av1_frame_thread_data,
    what: [&'static str; 3],
) -> Result<(), (&'static str, i32)> {
    let cb = &mut td.command_buffer;
    let allocator = device.create_command_allocator().map_err(|error| (what[0], error.code))?;
    // SAFETY: an empty field (the caller's contract).
    unsafe { com_store(&raw mut cb.allocator, allocator.into_raw()) };
    cb.fence_value = 0;
    let fence = device.create_fence(false).map_err(|error| (what[1], error.code))?;
    // SAFETY: as above.
    unsafe { com_store(&raw mut cb.fence, fence.into_raw()) };
    cb.event = Event::new().map_err(|error| (what[2], error.code))?.into_raw();
    td.frame_number = 0;
    Ok(())
}

/// `av1_create_gpu_decoder` (`dx/av1_core.h`): a decoder on its device, with every buffer it needs and
/// with the host pool of `cfg` for the entropy decoder. Returns 0 and the decoder, or -1 after a "MEMORY
/// FAILED" or "MEMORY REFUSED" line; then nothing is left of it.
///
/// # Safety
/// `gpu_dec` is valid for writing; `cfg` is the configuration the host pool was sized for.
pub(crate) unsafe extern "C" fn av1_create_gpu_decoder(
    gpu_dec: *mut *mut Av1Core,
    cfg: *mut aom_codec_dec_cfg_t,
) -> c_int {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        *gpu_dec = std::ptr::null_mut();
        clear_create_error();
        let cfg = &*cfg;

        // The decoder and its allocator: allocations of their own, empty, with the queues and the locks
        // made. The release is valid from here on, whichever step below fails.
        let (Some(dec), Some(mem)) = (Av1Core::new(), Allocator::new_boxed(cfg.host_memory.cast(), cfg.host_size))
        else {
            log_create_error(&format!(
                "MEMORY FAILED decoder: the process has no memory for the decoder's {} bytes",
                Av1Core::own_bytes()
            ));
            return -1;
        };
        let dec = Box::into_raw(dec);
        let mem = Box::into_raw(mem);
        (*dec).memory = mem;

        let mut plan = ParsePlan::from_cfg(cfg);
        let mut rcfg = ResourceConfig::with_slots(cfg, plan.slots);
        configure_parser_buffers(&mut *dec, cfg.parser_options);
        (*dec).frame_slots = rcfg.gpu_pipeline_depth;

        // The decoder takes a reference of the device and of the queue the application gave.
        if let Some(device) = ID3D12Device::from_raw_borrowed(&cfg.dx12device) {
            com_store(&raw mut (*dec).compute.device, device.clone().into_raw());
        }
        if let Some(queue) = ID3D12CommandQueue::from_raw_borrowed(&cfg.dx12command_queue) {
            com_store(&raw mut (*dec).compute.queue, queue.clone().into_raw());
        }
        if let Err(error) = create_device(&mut *dec) {
            return create_failed(dec, &rcfg, None, "D3D12 device or command queue", error.code);
        }
        let device = device_of(dec).expect("the decoder has a device");

        (*dec).cb_get_output_image = cfg.out_buffers_cb.get_out_buffer_cb;
        (*dec).cb_release_image = cfg.out_buffers_cb.release_out_buffer_cb;
        (*dec).cb_notify_frame_ready = cfg.out_buffers_cb.notify_frame_ready_cb;
        (*dec).image_alloc_priv = cfg.out_buffers_cb.out_buffers_priv;

        // What the adapter reports before the decoder takes anything.
        let vm_before = Device::video_memory(Some(&device));

        (*dec).shader_lib = cfg.dxPsos.cast();
        if (*dec).shader_lib.is_null() {
            return create_failed(dec, &rcfg, Some(&vm_before), "no pipeline cache (dxPsos) given", E_POINTER.0);
        }
        let shader_errors = wait_shader_create_complete((*dec).shader_lib);
        if shader_errors != 0 {
            return create_failed(dec, &rcfg, Some(&vm_before), "compute pipeline states (shaders)", shader_errors);
        }

        // The output fence: one for the decoder, signalled behind every command list.
        match Device::new(&device).create_fence(cfg.shared_fence != 0) {
            Ok(fence) => com_store(&raw mut (*dec).output_fence, fence.into_raw()),
            Err(error) => {
                let step = if cfg.shared_fence != 0 { "output fence (shared)" } else { "output fence" };
                return create_failed(dec, &rcfg, Some(&vm_before), step, error.code);
            }
        }
        // The semi-planar output: what the device does with the two texture formats, and the views of
        // the planes the output pass writes. A device that lets no shader write such a texture gets
        // its frames through the back buffer (`output.rs`); the log says which it is.
        (*dec).output_kind = c_int::from(cfg.output_kind == 1);
        (*dec).display = if (*dec).output_kind != 0 { cfg.display } else { Default::default() };
        if (*dec).output_kind != 0 {
            let gpu = Device::new(&device);
            let support = [gpu.semi_planar_support(SemiPlanar::Nv12), gpu.semi_planar_support(SemiPlanar::P010)];
            (*dec).semi_planar_support = support;
            let yes = |value: bool| if value { "yes" } else { "no" };
            log(
                false,
                &format!(
                    "OUTPUT semi-planar: NV12 textures {}, written by shaders {}; P010 textures {}, written by \
                     shaders {}",
                    yes(support[0].texture),
                    yes(support[0].shader_writes),
                    yes(support[1].texture),
                    yes(support[1].shader_writes)
                ),
            );
            let display = &(*dec).display;
            if display.width != 0 {
                // The display pass writes its images with a shader: a device that lets no shader write
                // the format cannot have it (the application keeps its own stage).
                let p010 = display.depth > 8;
                if !support[usize::from(p010)].shader_writes {
                    log_create_error(&format!(
                        "DISPLAY the device lets no shader write {} textures: no display pass",
                        if p010 { "P010" } else { "NV12" }
                    ));
                    return create_failed(dec, &rcfg, Some(&vm_before), "display pass", 0);
                }
                log(
                    false,
                    &format!(
                        "DISPLAY {}x{} {}, transform {}, peak {} nits, white {} nits, MaxCLL {} nits, mastering \
                         maximum {} nits, assumed colour {}/{}/{}/{}",
                        display.width,
                        display.height,
                        if p010 { "P010" } else { "NV12" },
                        display.transform,
                        display.source_peak_nits,
                        display.target_white_nits,
                        display.max_cll_nits,
                        display.mastering_max_nits,
                        display.assumed_primaries,
                        display.assumed_transfer,
                        display.assumed_matrix,
                        display.assumed_chroma_sample_position
                    ),
                );
            }
            match gpu.create_plane_views() {
                Ok(views) => com_store(&raw mut (*dec).plane_views, views.into_raw()),
                Err(error) => {
                    let step = "descriptor heap of the semi-planar output";
                    return create_failed(dec, &rcfg, Some(&vm_before), step, error.code);
                }
            }
        }

        // Budget guard: an application over its memory limit loses the D3D12 device for the rest of the
        // process, so with a budget callback the creation is refused before it allocates anything it cannot
        // afford.
        (*dec).memory_available_cb = cfg.memory_available_cb;
        (*dec).memory_available_priv = cfg.memory_available_priv;
        // The headroom: a decoder that fits the hard margin below can still leave the application so
        // little that the passes which read frame buffers run several times slower (docs/phase2-rav1d.md,
        // step 6). That is no reason to fail: the decoder is then created with fewer slots and frames
        // in parse, the largest of the smaller decoders that leaves the headroom, or the smallest one.
        {
            let asked = plan;
            let headroom = headroom_from_cfg(cfg);
            let mut ladder = vec![(plan, planned_device_bytes(dec, &rcfg).0 as u64)];
            while let Some(next) = ladder[ladder.len() - 1].0.smaller() {
                let planned = planned_device_bytes(dec, &ResourceConfig::with_slots(cfg, next.slots)).0 as u64;
                ladder.push((next, planned));
            }
            let (_, available, waited_ms) =
                settled_budget((*dec).memory_available_cb, (*dec).memory_available_priv, ladder[0].1 + headroom);
            let by_default = if cfg.frames_in_parse <= 0 && cfg.frame_slots == 0 {
                format!(" (the default for {}x{})", rcfg.width as u32, rcfg.height as u32)
            } else {
                String::new()
            };
            // Without a budget (no callback, or one that does not know) nothing is compared, and the
            // "MEMORY budget: not checked" line below says so.
            if available >= 0 {
                let available = available as u64;
                let sizes: Vec<u64> = ladder.iter().map(|step| step.1).collect();
                let chosen = first_with_headroom(&sizes, available, headroom);
                let left = |planned: u64| (available as f64 - planned as f64) / f64::from(1 << 20);
                for (index, (step, planned)) in ladder.iter().enumerate().take(chosen) {
                    log(
                        false,
                        &format!(
                            "MEMORY headroom: {}{}: device buffers {:.1} MiB would leave {:.1} MiB of the {:.1} MiB \
                             the application has left for the decoder, less than the headroom of {:.1} MiB{}: not \
                             created so",
                            step.words(),
                            if index == 0 { by_default.as_str() } else { "" },
                            to_mib(*planned),
                            left(*planned),
                            to_mib(available),
                            to_mib(headroom),
                            if index == 0 { format!(" (asked for {waited_ms} ms)") } else { String::new() }
                        ),
                    );
                }
                let (step, planned) = ladder[chosen];
                let instead = if chosen == 0 {
                    String::from("created as asked")
                } else {
                    format!("created in place of {}", asked.words())
                };
                if planned + headroom <= available {
                    log(
                        false,
                        &format!(
                            "MEMORY headroom: {}{}: device buffers {:.1} MiB leave {:.1} MiB of the {:.1} MiB the \
                             application has left for the decoder, the headroom is {:.1} MiB: {instead}",
                            step.words(),
                            if chosen == 0 { by_default.as_str() } else { "" },
                            to_mib(planned),
                            left(planned),
                            to_mib(available),
                            to_mib(headroom)
                        ),
                    );
                } else {
                    log(
                        false,
                        &format!(
                            "MEMORY headroom: {}{}: device buffers {:.1} MiB would leave {:.1} MiB of the {:.1} MiB \
                             the application has left for the decoder, less than the headroom of {:.1} MiB, and \
                             there is no smaller decoder: {instead} if the margin below holds; its frames may be \
                             slow for want of memory",
                            step.words(),
                            if chosen == 0 { by_default.as_str() } else { "" },
                            to_mib(planned),
                            left(planned),
                            to_mib(available),
                            to_mib(headroom)
                        ),
                    );
                }
                if chosen != 0 {
                    plan = step;
                    rcfg = ResourceConfig::with_slots(cfg, plan.slots);
                    (*dec).frame_slots = rcfg.gpu_pipeline_depth;
                }
            }
        }
        (*dec).parser_threads = plan.threads;
        (*dec).frames_in_parse = plan.frames;
        let frame_slots = rcfg.gpu_pipeline_depth as usize;
        {
            let (planned, deferred) = planned_device_bytes(dec, &rcfg);
            (*dec).device_bytes = planned;
            let callback = (*dec).memory_available_cb;
            let private = (*dec).memory_available_priv;
            let needed = planned as u64 + MEMORY_GUARD_MARGIN;
            // Not refused at once: the answer is asked again for as long as it keeps growing
            // (`settled_budget`).
            let (first, available, waited_ms) = settled_budget(callback, private, needed);
            if callback.is_some() && first >= 0 && needed > first as u64 && available >= 0 {
                log(
                    false,
                    &format!(
                        "MEMORY budget: {:.1} MiB needed and {:.1} MiB left for the decoder, waited {waited_ms} \
                         ms for memory the system may still be returning: {:.1} MiB left now ({})",
                        to_mib(needed),
                        to_mib(first as u64),
                        to_mib(available as u64),
                        if needed <= available as u64 {
                            "enough"
                        } else if waited_ms >= AV1_MEMORY_GUARD_WAIT_MS {
                            "still growing at the end of the wait"
                        } else {
                            "no longer growing"
                        }
                    ),
                );
            }
            if available >= 0 && needed > available as u64 {
                log_create_error(&format!(
                    "MEMORY REFUSED decoder {}x{} {}-bit, slots {}: its device buffers take {planned} bytes ({:.1} \
                     MiB) and the creation must leave a margin of {:.1} MiB, {:.1} MiB together; the application \
                     has {:.1} MiB left for the decoder. Nothing was allocated; the D3D12 device is intact",
                    rcfg.width as u32,
                    rcfg.height as u32,
                    rcfg.bitdepth as u32,
                    rcfg.gpu_pipeline_depth,
                    to_mib(planned as u64),
                    to_mib(MEMORY_GUARD_MARGIN),
                    to_mib(needed),
                    to_mib(available as u64)
                ));
                log_video_memory(true, "REFUSED, nothing allocated", &vm_before);
                destroy(dec, false);
                return -1;
            }
            if available >= 0 {
                log(
                    false,
                    &format!(
                        "MEMORY budget: device buffers {:.1} MiB + margin {:.1} MiB = {:.1} MiB, the application \
                         has {:.1} MiB left for the decoder: accepted; palette buffers and growth of pools and stage \
                         buffers can add up to {:.1} MiB",
                        to_mib(planned as u64),
                        to_mib(MEMORY_GUARD_MARGIN),
                        to_mib(needed),
                        to_mib(available as u64),
                        to_mib(deferred as u64)
                    ),
                );
            } else {
                log(
                    false,
                    &format!(
                        "MEMORY budget: not checked ({}); device buffers {:.1} MiB, palette buffers and growth \
                         of pools and stage buffers can add up to {:.1} MiB",
                        if callback.is_some() {
                            "the application's callback does not know what is left"
                        } else {
                            "no memory_available_cb in the decoder configuration"
                        },
                        to_mib(planned as u64),
                        to_mib(deferred as u64)
                    ),
                );
            }
        }

        if !allocate_buffers(dec, &mut Requests::Allocator(mem, device.clone()), &rcfg) {
            let step = "decoder buffers (the MEMORY FAILED line above names the one)";
            return create_failed(dec, &rcfg, Some(&vm_before), step, 0);
        }
        let coefs = &*(*dec).idct_coefs;
        coefs.host_ptr.cast::<u8>().write_bytes(0, coefs.size);
        if (*dec).gpu_grid == 0 {
            let chunks = super::pool_buffers::worst(&*dec) + (*dec).pool_total[Kind::ModeInfo as usize];
            if (*dec).mi_chunks.try_reserve_exact(chunks).is_err() {
                return create_failed(dec, &rcfg, Some(&vm_before), "mode info chunk mappings", 0);
            }
            (*dec).mi_chunks.resize_with(chunks, Default::default);
        }
        let pools = Pools::with_test_wait((*dec).pool_total, (*dec).pool_rule, rcfg.pool_test_wait);
        (*dec).pools = Some(Box::new(pools));

        (*dec).tryhdr10x3 = cfg.tryHDR10x3;

        for i in 0..frame_slots {
            let td = &mut (*dec).frame_thread_data[i];
            let what = ["command allocator of a slot", "fence of a slot", "event of a slot"];
            if let Err((step, hr)) = create_command_buffer(Device::new(&device), td, what) {
                return create_failed(dec, &rcfg, Some(&vm_before), step, hr);
            }
            (*dec).frame_data_pool.push(std::ptr::from_mut(td));
        }
        for i in 0..OutputThreadDataCount as usize {
            let td = &mut (*dec).output_thread_data[i];
            let what =
                ["command allocator of an output context", "fence of an output context", "event of an output context"];
            if let Err((step, hr)) = create_command_buffer(Device::new(&device), td, what) {
                return create_failed(dec, &rcfg, Some(&vm_before), step, hr);
            }
            (*dec).output_data_pool.push(std::ptr::from_mut(td));
        }

        // The command list of the decoder, open on the allocator of the first slot, and closed until a
        // frame is recorded.
        let allocator = wrapped_ref::<_, CommandAllocator>(&(*dec).frame_thread_data[0].command_buffer.allocator);
        let allocator = allocator.expect("the first slot has its allocator");
        let list = match Device::new(&device).create_command_list(allocator) {
            Ok(list) => list,
            Err(error) => return create_failed(dec, &rcfg, Some(&vm_before), "command list", error.code),
        };
        let closed = list.close();
        com_store(&raw mut (*dec).compute.command_list, list.into_raw());
        if let Err(error) = closed {
            return create_failed(dec, &rcfg, Some(&vm_before), "command list Close", error.code);
        }

        let hr = upload_luts(dec);
        if hr < 0 {
            return create_failed(dec, &rcfg, Some(&vm_before), "upload of the lookup tables", hr);
        }

        // Work items (GpuItemPoolSize). The decode thread waits when the pool is empty.
        for i in 0..(*dec).gpu_item_pool_src.len() {
            let item = &raw mut (*dec).gpu_item_pool_src[i];
            (*item).data = std::ptr::null_mut();
            (*item).image = std::ptr::null_mut();
            (*dec).gpu_item_pool.push(item);
        }

        for i in 0..ImagePoolSize as usize {
            let img = &raw mut (*dec).image_pool_src[i];
            (*img).size = 0;
            (*img).fb_ptr = std::ptr::null_mut();
            (*img).is_valid = 0;
            (*dec).image_pool.push(img);
        }

        let fb_size = (*dec).fb_size;
        (*dec).back_buffer1.base_offset = 0;
        // Only used with shared_back_buffer; the pool buffer has the room for it only then.
        (*dec).back_buffer0.size = fb_size as _;
        (*dec).back_buffer0.base_offset = (*dec).back_buffer1.size;
        for i in 0..rcfg.ref_fb_count {
            let offset = (*dec).fb_offset + fb_size * i;
            let fb = &raw mut (*dec).fb_pool_src[i as usize];
            (*fb).size = fb_size as _;
            (*fb).base_offset = offset as _;
            (*fb).ref_cnt = 0;
            (*dec).fb_pool.lock().add(fb);
        }

        log_memory_summary(false, "decoder created", &*dec, &rcfg);
        let vm_after = Device::video_memory(Some(&device));
        log_video_memory(false, "before the decoder's allocations", &vm_before);
        log_video_memory(false, "after the decoder's allocations", &vm_after);

        *gpu_dec = dec;
    }
    0
}

// ---------------------------------------------------------------------------------------------------------
// Release.
// ---------------------------------------------------------------------------------------------------------

/// One "MEMORY destroy <step>: ..." line: what the adapter reports and what the application has left (its
/// budget callback) at a step of the release of a decoder. The differences between the lines say which
/// step gave how much back, to the video memory and to the application.
fn log_destroy_step(
    device: &ID3D12Device,
    available_cb: Option<unsafe extern "C" fn(*mut c_void) -> i64>,
    available_priv: *mut c_void,
    step: &str,
) {
    let vm = Device::video_memory(Some(device));
    let dxgi = if vm.adapter_error >= 0 && vm.local.error >= 0 {
        format!("dxgi local usage {:.1} MiB", to_mib(vm.local.usage))
    } else {
        String::from("dxgi local usage not available")
    };
    // SAFETY: the application's callback with its own pointer.
    let left = available_cb.map_or(-1, |callback| unsafe { callback(available_priv) });
    let app = if left >= 0 {
        format!("the application has {:.1} MiB left for a decoder", to_mib(left as u64))
    } else {
        format!(
            "what the application has left is not known ({})",
            if available_cb.is_some() { "its callback does not know" } else { "no memory_available_cb" }
        )
    };
    log(false, &format!("MEMORY destroy {step}: {dxgi}; {app}"));
}

/// What the destructor of the C++ `Av1Core` released: the interfaces of the slots and of the output
/// contexts (the timing query heaps and their readback buffers, the copies of -dumpstages, fence and
/// allocator), then the command list, the decoder's queue (its own unless the application gave one) and
/// its reference to the device.
///
/// # Safety
/// `dec` is a decoder nothing uses any more.
unsafe fn release_interfaces(dec: *mut Av1Core) {
    // SAFETY: the caller's contract: each field is null or holds a reference of the decoder.
    unsafe {
        let slots = (*dec).frame_thread_data.iter_mut().rev();
        for td in (*dec).output_thread_data.iter_mut().rev().chain(slots) {
            for dump in td.dumps.iter_mut().rev() {
                com_release(&raw mut dump.readback);
            }
            com_release(&raw mut td.perf.readback);
            com_release(&raw mut td.perf.query_heap);
            com_release(&raw mut td.command_buffer.fence);
            com_release(&raw mut td.command_buffer.allocator);
        }
        com_release(&raw mut (*dec).plane_views);
        com_release(&raw mut (*dec).output_fence);
        com_release(&raw mut (*dec).compute.command_list);
        com_release(&raw mut (*dec).compute.queue_direct);
        com_release(&raw mut (*dec).compute.queue);
        com_release(&raw mut (*dec).compute.device);
    }
}

/// `destroy_gpu_decoder` of the C++. Also for a decoder whose creation stopped half way: everything not
/// built yet is zero. `log_steps`: the release of a decoder that was in use, logged step by step
/// ("MEMORY destroy"). The decoder and its allocator are freed at the end.
///
/// # Safety
/// `dec` is a decoder of [`Av1Core::new`] with its allocator, or one whose creation failed; nothing uses
/// it afterwards.
unsafe fn destroy(dec: *mut Av1Core, log_steps: bool) {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        // The device stays with the application; it is held here for the lines after the decoder's own
        // reference is gone. The budget callback is the application's.
        let device = if log_steps { device_of(dec) } else { None };
        let available_cb = (*dec).memory_available_cb;
        let available_priv = (*dec).memory_available_priv;
        if let Some(device) = &device {
            log_destroy_step(device, available_cb, available_priv, "start");
        }

        // A decoder with more than one frame in parse: its frames in parse are submitted, and the thread
        // that submits them ends.
        super::rav1d_host::stop_frames(dec);
        // Joins the GPU completion thread: every frame that was submitted has completed on the GPU (the
        // thread waits for the fence of each), so nothing on the queue uses the buffers that are released
        // below.
        av1_drain_gpu_decoder(dec);
        // The parser of phase 2: its threads are joined before the buffers they write are released.
        drop((*dec).rav1d.take());
        // -memstats: what no drain has written.
        super::mem_stats::summary(dec);
        // What the pools were used for, when the decoder was in use.
        if let (true, Some(pools)) = (log_steps, (*dec).pools.as_deref()) {
            let stats = pools.stats();
            let part = |kind: Kind| {
                let (peak, total) = (stats.peak[kind as usize], stats.total[kind as usize]);
                format!("{peak} of {total} chunks ({:.0} %)", 100.0 * peak as f64 / total.max(1) as f64)
            };
            log(
                false,
                &format!(
                    "MEMORY pools: the most in use at once: mode info {}, coefficients {}, transform block records \
                     {}; a tile waited for a chunk {} times for the GPU to finish an older frame and {} times for a \
                     frame in parse in front of its own",
                    part(Kind::ModeInfo),
                    part(Kind::Coefs),
                    part(Kind::Txb),
                    stats.waits_for_gpu,
                    stats.waits_for_frame
                ),
            );
        }
        // The same for the frame buffers of a decoder whose frames take theirs when they are recorded
        // (the line of the other decoders is what it was).
        if log_steps && (*dec).late_frame_buffer != 0 {
            log(
                false,
                &format!(
                    "MEMORY frame buffers: no buffer for the slots above two; a frame waited {} times for a frame on the GPU \
                     to return one",
                    (*dec).fb_pool.waits()
                ),
            );
        }

        if let Some(release_image) = (*dec).cb_release_image {
            for img in &(*dec).image_pool_src {
                if img.is_valid != 0 {
                    release_image((*dec).image_alloc_priv, img.alloc_priv);
                }
            }
        }

        let frame_slots = usize::try_from((*dec).frame_slots).unwrap_or(0);
        for i in 0..frame_slots {
            let td = &mut (*dec).frame_thread_data[i];
            if !td.command_buffer.event.is_null() {
                Event::close(td.command_buffer.event);
            }
            td.command_buffer.event = std::ptr::null_mut();
        }
        for td in &mut (*dec).output_thread_data {
            if !td.command_buffer.event.is_null() {
                Event::close(td.command_buffer.event);
            }
            td.command_buffer.event = std::ptr::null_mut();
        }

        let memory = (*dec).memory;
        if !memory.is_null() {
            if let Some(device) = &device {
                log_destroy_step(device, available_cb, available_priv, "GPU thread joined, buffers not yet released");
                // The buffers the CPU maps first, the default-heap buffers the CPU never sees last: one line
                // after each memory type.
                for kind in [DeviceUpload, HostRW, UploadUAV, ReadBack, DeviceOnlyConst, DeviceOnly] {
                    let (bytes, count) = (*memory).release_type(kind);
                    if count == 0 {
                        continue;
                    }
                    let mut step = String::new();
                    let _ = write!(
                        step,
                        "released {}, {count} buffers, {:.1} MiB",
                        memory_type_name(kind),
                        to_mib(bytes as u64)
                    );
                    log_destroy_step(device, available_cb, available_priv, &step);
                }
            }
            (*memory).release();
            (*dec).memory = std::ptr::null_mut();
            drop(Box::from_raw(memory));
        }

        release_interfaces(dec);
        // The decoder itself: the queues, the counters of the timing run (-gputimedetail), the struct.
        // Every interface it held is released above, in that order.
        drop(Box::from_raw(dec));

        if let Some(device) = &device {
            log_destroy_step(device, available_cb, available_priv, "end, command queue, list and allocators released");
        }
    }
}

/// `av1_destroy_gpu_decoder` (`dx/av1_core.h`).
///
/// # Safety
/// `dec` is null or a decoder of [`av1_create_gpu_decoder`]; nothing uses it afterwards.
pub(crate) unsafe extern "C" fn av1_destroy_gpu_decoder(dec: *mut Av1Core) {
    if dec.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    unsafe { destroy(dec, true) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bounded_line_cuts_like_snprintf() {
        let mut line = BoundedLine::new(8);
        line.push("abc");
        assert!(!line.full());
        line.push("defghij");
        assert_eq!(line.text, "abcdefg");
        assert!(line.full());
    }

    /// The margin of the budget guard is a `size_t` expression in the header, which the bindings do not
    /// carry over.
    #[test]
    fn the_margin_is_that_of_the_header() {
        let header = concat!(env!("CARGO_MANIFEST_DIR"), "/../../libav1/aom/aom_decoder.h");
        let text = std::fs::read_to_string(header).expect("aom_decoder.h");
        assert!(text.contains("#define AV1_MEMORY_GUARD_MARGIN ((size_t)48 << 20)"));
        assert_eq!(MEMORY_GUARD_MARGIN, 48 << 20);
        assert!(text.contains("#define AV1_MEMORY_GUARD_HEADROOM ((size_t)128 << 20)"));
        assert_eq!(MEMORY_GUARD_HEADROOM, 128 << 20);
    }

    /// The ladder of the budget guard, and the two cases of the console it is calibrated on (package
    /// 0.2.156.0 in 1 GiB, 3840x2160 10-bit; docs/phase2-rav1d.md, step 6): the default must stay, a
    /// fourth slot must not.
    #[test]
    fn the_headroom_keeps_the_default_and_drops_the_fourth_slot() {
        let plan = |frames, slots| ParsePlan { threads: 2, frames, slots };
        assert_eq!(plan(2, 4).smaller(), Some(plan(2, 3)));
        assert_eq!(plan(2, 3).smaller(), Some(plan(1, 2)));
        assert_eq!(plan(4, 4).smaller(), Some(plan(2, 3)));
        assert_eq!(plan(1, 4).smaller(), Some(plan(1, 3)));
        assert_eq!(plan(2, 2).smaller(), Some(plan(1, 2)));
        assert_eq!(plan(1, 2).smaller(), None);

        let tenths = |mib: u64| mib * (1 << 20) / 10;
        let (one, two, four) = (tenths(5976), tenths(6465), tenths(6953));
        // The default: 775.0 MiB left, 897 of 1024 MiB in use while decoding, 45.7 fps. It leaves 128.5
        // MiB, which is what the headroom is taken from: the least that is known to be enough.
        assert_eq!(first_with_headroom(&[two, one], tenths(7750), MEMORY_GUARD_HEADROOM), 0);
        // -slots 4: 772.8 MiB left, 948 of 1024 MiB in use, 19.5 fps where 49.6 are possible. Not
        // created so; and with 2.2 MiB less than the launch above, not the default either.
        assert_eq!(first_with_headroom(&[four, two, one], tenths(7728), MEMORY_GUARD_HEADROOM), 2);
        // The same two with the 19.2 MiB the entropy decoder of libaom had in the host pool, which
        // the application has left since step 6: the default with room to spare, the fourth slot not.
        assert_eq!(first_with_headroom(&[two, one], tenths(7942), MEMORY_GUARD_HEADROOM), 0);
        assert_eq!(first_with_headroom(&[four, two, one], tenths(7920), MEMORY_GUARD_HEADROOM), 1);
        // With the margin as the headroom, which is what the guard did, the fourth slot is accepted.
        assert_eq!(first_with_headroom(&[four, two, one], tenths(7728), MEMORY_GUARD_MARGIN), 0);
        // Nothing leaves the headroom: the smallest decoder, never a refusal.
        assert_eq!(first_with_headroom(&[four, two, one], tenths(6500), MEMORY_GUARD_HEADROOM), 2);
        assert_eq!(first_with_headroom(&[one], 0, MEMORY_GUARD_HEADROOM), 0);
    }

    #[test]
    fn slots_of_a_configuration() {
        // SAFETY: a configuration is numbers and pointers.
        let mut cfg: aom_codec_dec_cfg_t = unsafe { std::mem::zeroed() };
        (cfg.width, cfg.height) = (2560, 1440);
        assert_eq!(ParsePlan::from_cfg(&cfg), ParsePlan { threads: 1, frames: 1, slots: 2 });
        cfg.width += 1;
        let large = ParsePlan::from_cfg(&cfg);
        assert_eq!(large, ParsePlan { threads: 4, frames: 4, slots: 4 });
        assert_eq!(large.smaller().unwrap(), ParsePlan { threads: 4, frames: 2, slots: 3 });
        assert_eq!(large.smaller().unwrap().smaller().unwrap(), ParsePlan { threads: 4, frames: 1, slots: 2 });
        cfg.frame_slots = 3;
        assert_eq!(ParsePlan::from_cfg(&cfg), ParsePlan { threads: 4, frames: 3, slots: 3 });
        cfg.threads = 2;
        assert_eq!(ParsePlan::from_cfg(&cfg), ParsePlan { threads: 2, frames: 2, slots: 3 });
        (cfg.frame_slots, cfg.threads) = (0, 0);
        (cfg.width, cfg.height) = (0, 0);
        assert_eq!(frame_slots_from_cfg(&cfg), 2);
        for (asked, slots) in [(1, 2), (3, 3), (4, 4), (9, 4), (-1, 2)] {
            cfg.frame_slots = asked;
            assert_eq!(frame_slots_from_cfg(&cfg), slots);
        }
        cfg.frame_slots = 3;
        let rcfg = ResourceConfig::from_cfg(&cfg);
        // The default: the shared back buffer, and above 1920x1088 every part.
        assert_eq!((rcfg.ref_fb_count, rcfg.low_memory, rcfg.low_memory_default), (11, 1, true));
        for (width, height, parts) in [
            (1920, 1080, 1),
            (1920, 1088, 1),
            (1920, 1152, 1),
            (1088, 1920, 1),
            (1920, 1153, 39),
            (2560, 1440, 39),
            (3840, 2160, 39),
        ] {
            (cfg.width, cfg.height) = (width, height);
            assert_eq!(ResourceConfig::from_cfg(&cfg).low_memory, parts, "{width}x{height}");
        }
        // What an application names is taken as it is, at every size.
        (cfg.width, cfg.height, cfg.low_memory) = (640, 360, 3);
        let rcfg = ResourceConfig::from_cfg(&cfg);
        assert_eq!((rcfg.low_memory, rcfg.low_memory_default), (3, false));
        (cfg.width, cfg.height, cfg.low_memory) = (3840, 2160, 1);
        assert_eq!(ResourceConfig::from_cfg(&cfg).low_memory, 1);
        cfg.low_memory = 8; // AV1_LOW_MEMORY_NONE
        let rcfg = ResourceConfig::from_cfg(&cfg);
        assert_eq!((rcfg.ref_fb_count, rcfg.low_memory), (13, 0));
        // The frame buffer bound at the recording: no buffer for the slots above two, and never fewer
        // than a decoder with two slots has. Without the shared back buffer the part changes nothing.
        for (parts, slots, buffers) in
            [(35, 3, 10), (35, 4, 10), (35, 2, 10), (3, 4, 12), (32, 3, 13), (32, 4, 14), (39, 3, 10)]
        {
            (cfg.low_memory, cfg.frame_slots) = (parts, slots);
            let rcfg = ResourceConfig::from_cfg(&cfg);
            assert_eq!((rcfg.ref_fb_count, rcfg.low_memory), (buffers, parts), "parts {parts}, {slots} slots");
        }
    }

    #[test]
    fn narrow_output_staging_sits_ahead_of_the_frame_buffers() {
        // SAFETY: a configuration is numbers and pointers.
        let mut cfg: aom_codec_dec_cfg_t = unsafe { std::mem::zeroed() };
        (cfg.width, cfg.height, cfg.bitdepth) = (33, 610, 8);
        for (output_kind, staging) in [(0, 491_520), (1, 245_760)] {
            cfg.output_kind = output_kind;
            for low_memory in [AV1_LOW_MEMORY_SHARED_BACK_BUFFER as i32, crate::sys::AV1_LOW_MEMORY_NONE as i32] {
                cfg.low_memory = low_memory;
                let rcfg = ResourceConfig::from_cfg(&cfg);
                let mut dec = Av1Core::new().unwrap();
                let mut plan = Requests::plan();
                // SAFETY: the plan only computes sizes for this decoder.
                assert!(unsafe { allocate_buffers(&raw mut *dec, &mut plan, &rcfg) });
                assert_eq!(dec.back_buffer1.size, staging);
                assert_eq!(dec.fb_size, 163_840);
                assert_eq!(dec.fb_offset as usize, staging + dec.shared_back_buffer as usize * 163_840);
            }
        }
    }
}

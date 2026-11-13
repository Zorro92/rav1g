// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The host on the sink of rav1d's parse pass (docs/phase2-rav1d.md, step 3): the decoder with the parser
//! of phase 2 in place of the entropy decoder of libaom. One frame is in parse at a time.
//!
//! The stages read the header of a frame through the structs of the entropy decoder (`AV1_COMMON`, the
//! block context of `AV1Decoder`). So this module keeps such structs of its own, cleared, and writes into
//! them what the stages read, from the headers rav1d has parsed ([`fill_header`]); the frame then goes
//! through the functions the entropy decoder calls (`frame.rs`, `completion.rs`): setup, the tiles'
//! cursors, recording, submission. The stage modules do not see which parser ran.
//!
//! What rav1d does not decide here:
//!
//! - **The references.** The host keeps the map of the eight reference slots itself, as libaom's decoder
//!   does (`ref_frame_map`), and updates it where rav1d updates its own: a frame that stands refreshes
//!   its slots, a shown key frame of a `show_existing_frame` takes all of them, a new sequence empties
//!   them. A frame buffer returns to the pool when the map and the frames in flight have let go of it.
//!   rav1d's pictures are handles without samples and are not looked at.
//! - **The output.** Which shown frame of a temporal unit is output is libaom's rule
//!   (`av1_frame_is_output` of `av1/decoder/obu.c`), asked with the place of the frame in the data of the
//!   decode call. rav1d's picture queue is emptied and dropped.
//!
//! A damaged stream: the frame whose parse fails returns its slot and is not submitted, the decode call
//! returns the error, and the rest of its temporal unit is dropped. rav1d leaves the reference slots that
//! frame was to refresh empty, and the host does the same; a later frame that names an empty slot fails
//! before anything of it is set up, and the decode call for it returns an error as well. Decoding is right
//! again from the first frame whose references are all whole, at the latest the next key frame.

use std::cell::UnsafeCell;
use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::Arc;

use rav1g_parse::rav1d::include::dav1d::headers::{
    Rav1dFilmGrainData, Rav1dFrameHeader, Rav1dFrameType, Rav1dMatrixCoefficients, Rav1dPixelLayout,
    Rav1dRestorationType, Rav1dSequenceHeader,
};
use rav1g_parse::rav1d::parse::{
    ParseFrame, ParsePalette, ParseSettings, ParseShowExisting, ParseSuperblock, ParseTile, Parser,
};
use rav1g_parse::sink::{FrameMemory, SecondModeInfo, Sink, SinkHost, TileMemory};

use super::completion::{rav1g_decode_sef_begin, rav1g_decode_sef_submit};
use super::frame::{
    PaletteMap, av1_reallocate_frame_buffer, av1_release_fb_callback, av1_sequence_fits_decoder,
    rav1g_decode_frame_record, rav1g_decode_frame_submit, rav1g_setup_frame_begin, rav1g_setup_frame_coefs,
    rav1g_setup_frame_end, release_unsubmitted_frame, second_coefs, second_mode_info, setup_tile, write_palette,
};
use super::log::log;
use super::perf::{av1_perf_ticks, perf_add};
use super::types::Av1Core;
use crate::sys;
use crate::sys::parser::{
    AV1_COMMON, AV1Decoder, INVALID_IDX, REF_NO_SCALE, RESTORE_NONE, RESTORE_SGRPROJ, RESTORE_SWITCHABLE,
    RESTORE_WIENER, RefCntBuffer, RestorationInfo, TileInfo, WarpedMotionParams, aom_film_grain_t,
    av1_decoded_frame_buffer_t, av1_tile_data,
};

unsafe extern "C" {
    /// `av1/common/av1_loopfilter.c`: the constant parts of the loop filter thresholds.
    fn av1_loop_filter_init(cm: *mut AV1_COMMON);
    /// `av1/common/warped_motion.c`: the shears of a warp model; 0 when the model is not valid.
    fn get_shear_params(wm: *mut WarpedMotionParams) -> c_int;
    /// `av1/common/restoration.c`: the number of loop restoration units of a plane, from the frame size
    /// and the unit size.
    fn av1_alloc_restoration_struct(cm: *mut AV1_COMMON, rsi: *mut RestorationInfo, is_uv: c_int);
}

/// The frame descriptions the reference map can hold at once: eight slots and the frame in parse; twice
/// that, so that a description is never looked for in vain.
const FRAME_DESCRIPTIONS: usize = 18;
/// `SCALE_NUMERATOR` of `av1/common/scale.h`: the superres denominator of a frame that is not scaled.
const SCALE_NUMERATOR: u8 = 8;
/// `WARPEDMODEL_PREC_BITS` of `av1/common/warped_motion.h`.
const WARPEDMODEL_PREC_BITS: u32 = 16;
/// The entropy decoder's limit of tile threads (`decoder_init` of `av1/av1_dx_iface.c`).
const MAX_THREADS: c_int = 8;

/// What a decode call failed with.
struct Failure {
    code: sys::aom_codec_err_t,
    detail: String,
}

/// What the host keeps between the calls of the sink. Written on the thread of the decode call
/// (`sequence`, `frame_begin`, `frame_end`, `show_existing`), while no tile is in parse; the tiles read
/// the header and write the loop restoration units, each unit by one tile.
struct State {
    /// The frame header as the stages read it, and the block context of the frame (`mb`).
    pbi: Box<AV1Decoder>,
    /// The frame descriptions (frame buffer, order hint, film grain) the map and the frame in parse use.
    frames: Box<[RefCntBuffer]>,
    /// The eight reference slots.
    map: [*mut RefCntBuffer; 8],
    /// The frame in parse, and the slots it refreshes.
    cur: *mut RefCntBuffer,
    refresh: u8,
    /// The data of the decode call.
    unit: *const u8,
    unit_len: usize,
    /// The start of the parse of the frame, for its timer.
    parse_start: i64,
    /// The first thing that failed in the decode call.
    failure: Option<Failure>,
}

/// A cleared struct of the entropy decoder on the heap.
fn zeroed_box<T>() -> Box<T> {
    let layout = std::alloc::Layout::new::<T>();
    // SAFETY: used for C structs of numbers and pointers, for which all zero bytes are a value; the
    // allocation has the layout of `T`.
    unsafe {
        let memory = std::alloc::alloc_zeroed(layout);
        if memory.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Box::from_raw(memory.cast::<T>())
    }
}

impl State {
    fn new(dec: *mut Av1Core) -> State {
        let mut pbi = zeroed_box::<AV1Decoder>();
        pbi.gpu_decoder = dec.cast();
        // SAFETY: a cleared frame header; the function writes its loop filter tables.
        unsafe { av1_loop_filter_init(&raw mut pbi.common) };
        pbi.mb.global_motion = pbi.common.global_motion.as_mut_ptr();
        // SAFETY: as for `zeroed_box`: a frame description is a C struct for which all zero is a value.
        let frames = (0..FRAME_DESCRIPTIONS).map(|_| unsafe { std::mem::zeroed::<RefCntBuffer>() }).collect();
        State {
            pbi,
            frames,
            map: [std::ptr::null_mut(); 8],
            cur: std::ptr::null_mut(),
            refresh: 0,
            unit: std::ptr::null(),
            unit_len: 0,
            parse_start: 0,
            failure: None,
        }
    }

    /// Keeps what failed, if nothing has failed before in this decode call.
    fn fail(&mut self, code: sys::aom_codec_err_t, detail: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(Failure { code, detail: detail.into() });
        }
    }

    /// A frame description nothing uses, with one reference.
    fn new_frame(&mut self) -> Option<*mut RefCntBuffer> {
        let frame = self.frames.iter_mut().find(|frame| frame.ref_count == 0)?;
        // SAFETY: a C struct for which all zero is a value.
        *frame = unsafe { std::mem::zeroed() };
        frame.ref_count = 1;
        Some(std::ptr::from_mut(frame))
    }

    /// Gives a reference to a frame description back; with its last one the frame buffer returns to the
    /// pool, and an output image the frame still had goes back to the application.
    ///
    /// # Safety
    /// `frame` is null or a description of this state with a reference; `dec` is the decoder.
    unsafe fn unref(dec: *mut Av1Core, frame: *mut RefCntBuffer) {
        // SAFETY: the caller's contract.
        unsafe {
            let Some(frame) = frame.as_mut() else { return };
            frame.ref_count -= 1;
            if frame.ref_count == 0 {
                av1_release_fb_callback(dec.cast(), &raw mut frame.buf);
            }
        }
    }

    /// Empties the reference slots.
    ///
    /// # Safety
    /// `dec` is the decoder; no frame is in parse.
    unsafe fn clear_map(&mut self, dec: *mut Av1Core) {
        for slot in &mut self.map {
            // SAFETY: the caller's contract; a slot holds a reference to its description.
            unsafe { State::unref(dec, std::mem::replace(slot, std::ptr::null_mut())) };
        }
    }

    /// Where the OBU behind the one in parse starts, and where the data of the decode call ends: what
    /// `av1_frame_is_output` looks through.
    ///
    /// # Safety
    /// The data of the decode call is valid.
    unsafe fn set_next_obu(&mut self, data_left: usize) {
        let start = self.unit_len.saturating_sub(data_left);
        // SAFETY: the data of the decode call (the caller's contract).
        let data = unsafe { std::slice::from_raw_parts(self.unit, self.unit_len) };
        let next = start + obu_size(&data[start..]).unwrap_or(data_left);
        let next = next.min(self.unit_len);
        // SAFETY: inside the data or at its end.
        unsafe {
            self.pbi.next_obu = self.unit.add(next);
            self.pbi.obus_end = self.unit.add(self.unit_len);
            self.pbi.temporal_unit_end = self.pbi.obus_end;
        }
    }
}

/// The bytes of the OBU at the start of `data`, with its header and its size field. `None` when the OBU
/// has no size field (it is the rest of the data) or cannot be read.
fn obu_size(data: &[u8]) -> Option<usize> {
    let header = *data.first()?;
    let has_extension = header & 4 != 0;
    let has_size = header & 2 != 0;
    if !has_size {
        return None;
    }
    let first = 1 + usize::from(has_extension);
    let mut size = 0u64;
    for (shift, at) in (first..first + 8).enumerate() {
        let byte = *data.get(at)?;
        size |= u64::from(byte & 0x7f) << (7 * shift);
        if byte & 0x80 == 0 {
            return usize::try_from(size).ok().map(|size| (at + 1).saturating_add(size));
        }
    }
    None
}

/// The host of the sink: the decoder, and what it keeps between the calls.
struct SlotHost {
    dec: *mut Av1Core,
    state: UnsafeCell<State>,
}

// SAFETY: the calls come as the contract of `SinkHost` says: those of a frame while no tile is in parse,
// those of a tile from one thread at a time. The state is written under that contract (`State`); the
// decoder is used as the entropy decoder's threads use it.
unsafe impl Send for SlotHost {}
// SAFETY: as above.
unsafe impl Sync for SlotHost {}

impl SlotHost {
    /// # Safety
    /// The thread of the decode call, while no tile is in parse.
    #[expect(
        clippy::mut_from_ref,
        reason = "the state is the decode thread's between the tiles, by the contract of the sink"
    )]
    unsafe fn state(&self) -> &mut State {
        // SAFETY: the caller's contract.
        unsafe { &mut *self.state.get() }
    }

    /// The frame header, for a tile.
    ///
    /// # Safety
    /// Between the `frame_begin` and the `frame_end` of a frame.
    unsafe fn common(&self) -> *mut AV1_COMMON {
        // SAFETY: the address of a field of the boxed header, which is not written while tiles are in
        // parse.
        unsafe { &raw mut (*self.state.get()).pbi.common }
    }
}

/// libaom's numbers of the loop restoration types of a frame (`RestorationType`).
fn restoration_type(kind: Rav1dRestorationType) -> u8 {
    (match kind {
        Rav1dRestorationType::None => RESTORE_NONE,
        Rav1dRestorationType::Switchable => RESTORE_SWITCHABLE,
        Rav1dRestorationType::Wiener => RESTORE_WIENER,
        Rav1dRestorationType::SgrProj(_) => RESTORE_SGRPROJ,
    }) as u8
}

/// The film grain parameters a frame header has read, written as `av1_read_film_grain_params` of
/// `av1/decoder/decodeframe.c` writes them: a field that the header did not have keeps what it held.
fn write_film_grain(pars: &mut aom_film_grain_t, data: &Rav1dFilmGrainData) {
    let points = usize::try_from(data.num_y_points).unwrap_or(0).min(14);
    pars.num_y_points = data.num_y_points;
    for i in 0..points {
        pars.scaling_points_y[i] = [c_int::from(data.y_points[i][0]), c_int::from(data.y_points[i][1])];
    }
    pars.chroma_scaling_from_luma = c_int::from(data.chroma_scaling_from_luma);
    pars.num_cb_points = data.num_uv_points[0];
    pars.num_cr_points = data.num_uv_points[1];
    for i in 0..usize::try_from(data.num_uv_points[0]).unwrap_or(0).min(10) {
        pars.scaling_points_cb[i] = [c_int::from(data.uv_points[0][i][0]), c_int::from(data.uv_points[0][i][1])];
    }
    for i in 0..usize::try_from(data.num_uv_points[1]).unwrap_or(0).min(10) {
        pars.scaling_points_cr[i] = [c_int::from(data.uv_points[1][i][0]), c_int::from(data.uv_points[1][i][1])];
    }
    pars.scaling_shift = c_int::from(data.scaling_shift);
    pars.ar_coeff_lag = data.ar_coeff_lag;
    let luma = (2 * data.ar_coeff_lag * (data.ar_coeff_lag + 1)).clamp(0, 24) as usize;
    let chroma = luma + usize::from(data.num_y_points > 0);
    if data.num_y_points != 0 {
        for i in 0..luma {
            pars.ar_coeffs_y[i] = c_int::from(data.ar_coeffs_y[i]);
        }
    }
    if data.num_uv_points[0] != 0 || data.chroma_scaling_from_luma {
        for i in 0..chroma {
            pars.ar_coeffs_cb[i] = c_int::from(data.ar_coeffs_uv[0][i]);
        }
    }
    if data.num_uv_points[1] != 0 || data.chroma_scaling_from_luma {
        for i in 0..chroma {
            pars.ar_coeffs_cr[i] = c_int::from(data.ar_coeffs_uv[1][i]);
        }
    }
    pars.ar_coeff_shift = c_int::from(data.ar_coeff_shift);
    pars.grain_scale_shift = c_int::from(data.grain_scale_shift);
    // libaom keeps the three numbers as they are coded; rav1d has taken their offsets off.
    if data.num_uv_points[0] != 0 {
        pars.cb_mult = data.uv_mult[0] + 128;
        pars.cb_luma_mult = data.uv_luma_mult[0] + 128;
        pars.cb_offset = data.uv_offset[0] + 256;
    }
    if data.num_uv_points[1] != 0 {
        pars.cr_mult = data.uv_mult[1] + 128;
        pars.cr_luma_mult = data.uv_luma_mult[1] + 128;
        pars.cr_offset = data.uv_offset[1] + 256;
    }
    pars.overlap_flag = c_int::from(data.overlap_flag);
    pars.clip_to_restricted_range = c_int::from(data.clip_to_restricted_range);
}

/// The bytes of film grain parameters, to compare two sets.
fn film_grain_bytes(pars: &aom_film_grain_t) -> &[u8] {
    // SAFETY: a struct of numbers that was cleared as a whole before its fields were written.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(pars).cast::<u8>(), size_of::<aom_film_grain_t>()) }
}

/// The film grain parameters of the frame (`read_film_grain` of `av1/decoder/decodeframe.c`), into the
/// header and into the description of the frame.
///
/// # Safety
/// `cur` is the description of the frame; the slots of `map` are null or descriptions.
unsafe fn film_grain(
    cm: &mut AV1_COMMON,
    map: &[*mut RefCntBuffer; 8],
    cur: &mut RefCntBuffer,
    seq: &Rav1dSequenceHeader,
    hdr: &Rav1dFrameHeader,
) {
    let grain = &hdr.film_grain;
    let pars = &mut cm.film_grain_params;
    // SAFETY: all zero is a value of the parameters.
    let none: aom_film_grain_t = unsafe { std::mem::zeroed() };
    if seq.film_grain_present == 0 || (hdr.show_frame == 0 && hdr.showable_frame == 0) || grain.present == 0 {
        *pars = none;
    } else if grain.update == 0 {
        // The parameters of a reference with a new seed. rav1d has copied them from the header of that
        // reference; libaom copies what it kept for the frame, which is found here as the reference of
        // this frame whose kept parameters are the copied ones.
        let seed = grain.data.seed as u16;
        let mut inherited = None;
        for &slot in hdr.refidx.iter() {
            // SAFETY: the caller's contract.
            let Some(reference) = (unsafe { map[slot as usize & 7].as_ref() }) else { continue };
            if reference.film_grain_params_present == 0 {
                continue;
            }
            let mut copy = reference.film_grain_params;
            write_film_grain(&mut copy, &grain.data);
            if film_grain_bytes(&copy) == film_grain_bytes(&reference.film_grain_params) {
                inherited = Some(reference.film_grain_params);
                break;
            }
        }
        *pars = inherited.unwrap_or_else(|| {
            let mut pars = none;
            pars.apply_grain = 1;
            write_film_grain(&mut pars, &grain.data);
            pars
        });
        pars.random_seed = seed;
    } else {
        pars.apply_grain = 1;
        pars.random_seed = grain.data.seed as u16;
        pars.update_parameters = 1;
        write_film_grain(pars, &grain.data);
    }
    pars.bit_depth = 8 + 2 * u32::from(seq.hbd);
    cur.film_grain_params = *pars;
    cur.film_grain_params_present = seq.film_grain_present;
}

/// The header of the frame in the structs of the entropy decoder: what `read_uncompressed_header` of
/// `av1/decoder/decodeframe.c` leaves there of what the host reads. A field libaom does not write for a
/// frame is not written here either, so that it holds what an earlier frame left, as it does there.
///
/// # Safety
/// `cur` is the description of the frame, with its frame buffer; no tile is in parse.
unsafe fn fill_header(state: &mut State, dec: *mut Av1Core, frame: &ParseFrame<'_>, cur: *mut RefCntBuffer) {
    let seq = frame.seq_hdr;
    let hdr = frame.frame_hdr;
    let map = state.map;
    let pbi = &mut *state.pbi;
    let cm = &mut pbi.common;
    let inter = matches!(hdr.frame_type, Rav1dFrameType::Inter | Rav1dFrameType::Switch);
    let bit_depth = 8 + 2 * u32::from(seq.hbd);
    let planes = if seq.monochrome != 0 { 1 } else { 3 };

    // The sequence.
    let sp = &mut cm.seq_params;
    sp.max_frame_width = seq.max_width;
    sp.max_frame_height = seq.max_height;
    sp.monochrome = seq.monochrome;
    sp.subsampling_x = c_int::from(seq.ss_hor);
    sp.subsampling_y = c_int::from(seq.ss_ver);
    sp.bit_depth = bit_depth as _;
    sp.use_highbitdepth = u8::from(seq.hbd != 0);
    // BLOCK_128X128 or BLOCK_64X64, and their sides in 4x4 units.
    sp.sb_size = if seq.sb128 != 0 { 15 } else { 12 };
    sp.mib_size_log2 = if seq.sb128 != 0 { 5 } else { 4 };
    sp.mib_size = 1 << sp.mib_size_log2;
    sp.enable_intra_edge_filter = seq.intra_edge_filter;
    sp.enable_superres = seq.super_res;
    sp.enable_cdef = seq.cdef;
    sp.enable_restoration = seq.restoration;
    sp.film_grain_params_present = seq.film_grain_present;
    sp.order_hint_info.enable_order_hint = c_int::from(seq.order_hint);
    sp.order_hint_info.order_hint_bits_minus_1 = c_int::from(seq.order_hint_n_bits) - 1;
    sp.order_hint_info.enable_dist_wtd_comp = c_int::from(seq.jnt_comp);
    sp.order_hint_info.enable_ref_frame_mvs = c_int::from(seq.ref_frame_mvs);

    // The frame.
    cm.current_frame.frame_type = hdr.frame_type as u8;
    cm.current_frame.order_hint = u32::from(hdr.frame_offset);
    cm.current_frame.refresh_frame_flags = c_int::from(hdr.refresh_frame_flags);
    cm.show_frame = c_int::from(hdr.show_frame);
    cm.showable_frame = c_int::from(hdr.showable_frame);
    cm.show_existing_frame = 0;
    cm.width = hdr.size.width[0];
    cm.height = hdr.size.height;
    cm.superres_upscaled_width = hdr.size.width[1];
    cm.superres_upscaled_height = hdr.size.height;
    cm.superres_scale_denominator =
        if hdr.size.super_res.enabled { hdr.size.super_res.width_scale_denominator } else { SCALE_NUMERATOR };
    cm.mi_cols = frame.bw;
    cm.mi_rows = frame.bh;
    cm.mi_stride = (frame.bw + 31) & !31;
    cm.allow_screen_content_tools = u8::from(hdr.allow_screen_content_tools);
    cm.allow_intrabc = c_int::from(hdr.allow_intrabc);
    cm.cur_frame_force_integer_mv = u8::from(hdr.force_integer_mv);
    cm.base_qindex = c_int::from(hdr.quant.yac);
    // rav1d's `all_lossless` is the lossless of the coded frame, before superres.
    cm.coded_lossless = c_int::from(hdr.all_lossless);
    cm.all_lossless = c_int::from(hdr.all_lossless && !hdr.size.super_res.enabled);
    cm.skip_loop_filter = 0;
    cm.is_annexb = 0;
    cm.temporal_layer_id = c_int::from(hdr.temporal_id);
    cm.spatial_layer_id = c_int::from(hdr.spatial_id);

    // The tiles, in superblocks.
    cm.tile_cols = c_int::from(hdr.tiling.cols);
    cm.tile_rows = c_int::from(hdr.tiling.rows);
    for (dst, start) in cm.tile_col_start_sb.iter_mut().zip(&hdr.tiling.col_start_sb[..=usize::from(hdr.tiling.cols)]) {
        *dst = c_int::from(*start);
    }
    for (dst, start) in cm.tile_row_start_sb.iter_mut().zip(&hdr.tiling.row_start_sb[..=usize::from(hdr.tiling.rows)]) {
        *dst = c_int::from(*start);
    }

    // Segmentation: the features of a segment. rav1d keeps the values, not which features are on; a
    // feature whose value is 0 changes nothing, so the values that are not 0 are the features.
    let seg = &hdr.segmentation;
    cm.seg.enabled = seg.enabled;
    for (i, data) in seg.seg_data.d.iter().enumerate() {
        let mut mask = 0u32;
        let mut values = [0i16; 8];
        if seg.enabled != 0 {
            let deltas = [
                data.delta_q,
                i16::from(data.delta_lf_y_v),
                i16::from(data.delta_lf_y_h),
                i16::from(data.delta_lf_u),
                i16::from(data.delta_lf_v),
            ];
            for (feature, delta) in deltas.into_iter().enumerate() {
                if delta != 0 {
                    mask |= 1 << feature;
                    values[feature] = delta;
                }
            }
            if data.r#ref >= 0 {
                mask |= 1 << 5;
                values[5] = i16::from(data.r#ref);
            }
            if data.skip != 0 {
                mask |= 1 << 6;
            }
            if data.globalmv != 0 {
                mask |= 1 << 7;
            }
        }
        cm.seg.feature_mask[i] = mask;
        cm.seg.feature_data[i] = values;
        pbi.mb.lossless[i] = c_int::from(seg.lossless[i]);
    }
    pbi.mb.cur_frame_force_integer_mv = c_int::from(hdr.force_integer_mv);
    cm.delta_q_info.delta_q_present_flag = c_int::from(hdr.delta.q.present);
    cm.delta_q_info.delta_lf_present_flag = c_int::from(hdr.delta.lf.present);
    cm.delta_q_info.delta_lf_multi = c_int::from(hdr.delta.lf.multi);

    // The loop filter (`setup_loopfilter`).
    let lf = &hdr.loopfilter;
    if hdr.allow_intrabc || hdr.all_lossless {
        cm.lf.filter_level = [0; 2];
    } else {
        cm.lf.filter_level = [c_int::from(lf.level_y[0]), c_int::from(lf.level_y[1])];
        if planes > 1 && (lf.level_y[0] != 0 || lf.level_y[1] != 0) {
            cm.lf.filter_level_u = c_int::from(lf.level_u);
            cm.lf.filter_level_v = c_int::from(lf.level_v);
        }
        cm.lf.sharpness_level = c_int::from(lf.sharpness);
        cm.lf.mode_ref_delta_enabled = lf.mode_ref_delta_enabled;
        cm.lf.mode_ref_delta_update = lf.mode_ref_delta_update;
        cm.lf.ref_deltas = lf.mode_ref_deltas.ref_delta;
        cm.lf.mode_deltas = lf.mode_ref_deltas.mode_delta;
    }

    // CDEF (`setup_cdef`, and what the header clears without it).
    let cdef = &hdr.cdef;
    if hdr.all_lossless || seq.cdef == 0 || hdr.allow_intrabc {
        cm.cdef_info.cdef_bits = 0;
        cm.cdef_info.cdef_strengths[0] = 0;
        cm.cdef_info.cdef_uv_strengths[0] = 0;
        if hdr.allow_intrabc {
            cm.cdef_info.nb_cdef_strengths = 1;
        }
    } else {
        cm.cdef_info.cdef_pri_damping = c_int::from(cdef.damping);
        cm.cdef_info.cdef_sec_damping = c_int::from(cdef.damping);
        cm.cdef_info.cdef_bits = c_int::from(cdef.n_bits);
        cm.cdef_info.nb_cdef_strengths = 1 << cdef.n_bits;
        for i in 0..1usize << cdef.n_bits {
            cm.cdef_info.cdef_strengths[i] = c_int::from(cdef.y_strength[i]);
            cm.cdef_info.cdef_uv_strengths[i] = if planes > 1 { c_int::from(cdef.uv_strength[i]) } else { 0 };
        }
    }

    // Loop restoration (`decode_restoration_mode`, `av1_alloc_restoration_buffers`).
    let restoration = &hdr.restoration;
    let mut restored = false;
    for p in 0..3 {
        let kind = if p < planes { restoration_type(restoration.r#type[p]) } else { RESTORE_NONE as u8 };
        cm.rst_info[p].frame_restoration_type = kind;
        restored |= kind != RESTORE_NONE as u8;
    }
    if restored {
        // SAFETY: the decoder's memory for the units of the largest frame; the function reads the frame
        // size and the unit size and writes the three counts.
        unsafe {
            let cm: *mut AV1_COMMON = cm;
            for p in 0..planes {
                let rsi = &raw mut (*cm).rst_info[p];
                (*rsi).restoration_unit_size = 1 << restoration.unit_size[usize::from(p > 0)];
                (*rsi).unit_info = (*dec).restoration_info_alloc[p].cast();
                av1_alloc_restoration_struct(cm, rsi, c_int::from(p > 0));
            }
        }
    }

    // Global motion (`read_global_motion`): the model of each reference, with its shears.
    let identity = WarpedMotionParams {
        wmmat: [0, 0, 1 << WARPEDMODEL_PREC_BITS, 0, 0, 1 << WARPEDMODEL_PREC_BITS, 0, 0],
        alpha: 0,
        beta: 0,
        gamma: 0,
        delta: 0,
        wmtype: 0,
        invalid: 0,
    };
    cm.global_motion = [identity; 8];
    if inter {
        for (model, gmv) in cm.global_motion[1..].iter_mut().zip(&hdr.gmv) {
            model.wmmat[..6].copy_from_slice(&gmv.matrix);
            model.wmtype = gmv.r#type as u8;
            // SAFETY: a model of the header.
            model.invalid = i16::from(unsafe { get_shear_params(model) } == 0);
        }
    }

    // The references: the slots of the frame, and the scale of each (`ref_scale_factors`).
    for i in 0..8 {
        cm.remapped_ref_idx[i] = INVALID_IDX;
        cm.ref_frame_map[i] = std::ptr::null_mut();
    }
    if inter {
        for i in 0..7 {
            cm.remapped_ref_idx[i] = i as c_int;
            cm.ref_frame_map[i] = map[hdr.refidx[i] as usize & 7];
            let sf = &mut cm.ref_scale_factors[i];
            let no_scale = REF_NO_SCALE as c_int;
            let scaled =
                |scale: c_int, step: c_int| if scale == 0 { (no_scale, (no_scale + 8) >> 4) } else { (scale, step) };
            (sf.x_scale_fp, sf.x_step_q4) = scaled(frame.ref_scale[i][0], frame.ref_step[i][0]);
            (sf.y_scale_fp, sf.y_step_q4) = scaled(frame.ref_scale[i][1], frame.ref_step[i][1]);
        }
    }

    // The description of the frame.
    // SAFETY: the caller's contract.
    let cur_frame = unsafe { &mut *cur };
    cur_frame.order_hint = u32::from(hdr.frame_offset);
    cur_frame.showable_frame = c_int::from(hdr.showable_frame);
    cur_frame.frame_type = hdr.frame_type as u8;
    cur_frame.width = hdr.size.width[0];
    cur_frame.height = hdr.size.height;
    cur_frame.buf.bit_depth = bit_depth;
    cur_frame.buf.monochrome = seq.monochrome;
    cur_frame.buf.matrix_coefficients =
        if seq.mtrx == Rav1dMatrixCoefficients::Identity { sys::parser::AOM_CICP_MC_IDENTITY } else { 2 };
    // SAFETY: the caller's contract.
    unsafe { film_grain(cm, &map, cur_frame, seq, hdr) };
    cm.cur_frame = cur;
}

impl SinkHost for SlotHost {
    fn gpu_decoder(&self) -> *mut c_void {
        self.dec.cast()
    }

    fn sequence(&self, seq: &Rav1dSequenceHeader, new: bool) -> Result<(), ()> {
        // SAFETY: the thread of the decode call, between frames.
        unsafe {
            let state = self.state();
            let bit_depth = 8 + 2 * c_int::from(seq.hbd);
            let (mut width, mut height, mut bits) = (0, 0, 0);
            if av1_sequence_fits_decoder(
                self.dec,
                seq.max_width,
                seq.max_height,
                bit_depth,
                &mut width,
                &mut height,
                &mut bits,
            ) == 0
            {
                state.fail(
                    sys::AOM_CODEC_NEEDS_RECONFIGURE,
                    format!(
                        "Sequence of {}x{} {bit_depth}-bit frames, decoder created for {width}x{height} {bits}-bit",
                        seq.max_width, seq.max_height
                    ),
                );
                return Err(());
            }
            if !matches!(seq.layout, Rav1dPixelLayout::I420 | Rav1dPixelLayout::I400) {
                state.fail(sys::AOM_CODEC_UNSUP_BITSTREAM, "Only 4:2:0 and monochrome sequences are decoded");
                return Err(());
            }
            if new {
                state.clear_map(self.dec);
            }
            // The operating point the application chose, as rav1d takes it: the first one when the
            // sequence does not have that many.
            let point = usize::try_from(state.pbi.operating_point).unwrap_or(0);
            let point = if point < usize::from(seq.num_operating_points) { point } else { 0 };
            state.pbi.current_operating_point = c_int::from(seq.operating_points[point].idc);
        }
        Ok(())
    }

    fn show_existing(&self, show: &ParseShowExisting<'_>) -> Result<(), ()> {
        // SAFETY: the thread of the decode call, between frames; the functions are called as the entropy
        // decoder calls them behind the header of a show_existing_frame.
        unsafe {
            let state = self.state();
            let slot = usize::from(show.frame_hdr.existing_frame_idx) & 7;
            let shown = state.map[slot];
            if shown.is_null() || (*shown).buf.hw_buffer.is_null() {
                state.fail(sys::AOM_CODEC_UNSUP_BITSTREAM, "Buffer does not contain a decoded frame");
                return Err(());
            }
            state.set_next_obu(show.data_left);
            let pbi: *mut AV1Decoder = &raw mut *state.pbi;
            let cm = &raw mut (*pbi).common;
            (*cm).cur_frame = shown;
            (*cm).show_frame = 1;
            (*cm).show_existing_frame = 1;
            (*cm).film_grain_params = (*shown).film_grain_params;
            (*cm).temporal_layer_id = c_int::from(show.frame_hdr.temporal_id);
            (*cm).spatial_layer_id = c_int::from(show.frame_hdr.spatial_id);
            let mut fb: av1_decoded_frame_buffer_t = std::mem::zeroed();
            let image_state = rav1g_decode_sef_begin(pbi, &mut fb);
            if image_state == 2 {
                release_unsubmitted_frame(self.dec);
                (*cm).cur_frame = std::ptr::null_mut();
                state.fail(sys::AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
                return Err(());
            }
            if image_state >= 0 {
                rav1g_decode_sef_submit(pbi, image_state, &fb);
            }
            (*cm).cur_frame = std::ptr::null_mut();
            if show.key {
                // The key frame is the reference in every slot from here on.
                (*shown).showable_frame = 0;
                for other in (0..8).filter(|&other| other != slot) {
                    State::unref(self.dec, state.map[other]);
                    (*shown).ref_count += 1;
                    state.map[other] = shown;
                }
            }
        }
        Ok(())
    }

    fn ticks(&self) -> i64 {
        av1_perf_ticks()
    }

    fn tile_time(&self, start: i64) {
        // SAFETY: the decoder; the timer is added to under its own rule for the tile threads.
        unsafe { perf_add(self.dec, sys::AV1_PERF_CPU_ENTROPY_SUM as usize, start) };
    }

    fn frame_begin(&self, frame: &ParseFrame<'_>) -> Result<FrameMemory, ()> {
        // SAFETY: the thread of the decode call, while no tile is in parse; the functions of the frame
        // setup are called as the entropy decoder calls them, with the header filled in.
        unsafe {
            let state = self.state();
            let hdr = frame.frame_hdr;
            let dec = self.dec;
            if matches!(hdr.frame_type, Rav1dFrameType::Inter | Rav1dFrameType::Switch) {
                let whole = hdr.refidx.iter().all(|&slot| {
                    state.map[slot as usize & 7].as_ref().is_some_and(|reference| !reference.buf.hw_buffer.is_null())
                });
                if !whole {
                    state.fail(sys::AOM_CODEC_CORRUPT_FRAME, "A reference of the frame has no frame buffer");
                    return Err(());
                }
            }
            let Some(cur) = state.new_frame() else {
                state.fail(sys::AOM_CODEC_MEM_ERROR, "No frame description is free");
                return Err(());
            };
            state.set_next_obu(frame.data_left);
            let hbd = c_int::from(frame.seq_hdr.hbd != 0);
            if av1_reallocate_frame_buffer(
                dec.cast(),
                &raw mut (*cur).buf,
                hdr.size.width[0],
                hdr.size.height,
                hdr.size.width[1],
                hbd,
            ) != 0
            {
                (*cur).ref_count = 0;
                state.fail(sys::AOM_CODEC_MEM_ERROR, "Failed to allocate frame buffer");
                return Err(());
            }
            fill_header(state, dec, frame, cur);

            let pbi: *mut AV1Decoder = &raw mut *state.pbi;
            let cm = &raw mut (*pbi).common;
            let mut perf_start = 0;
            let failed = if rav1g_setup_frame_begin(pbi, cm, &mut perf_start) != 0 {
                Some("No palette buffer for a frame that allows screen content tools")
            } else if rav1g_setup_frame_coefs(pbi, cm) != 0 {
                Some("No room for the coefficients of a frame with very small tiles")
            } else if rav1g_setup_frame_end(pbi, cm, perf_start) != 0 {
                Some("Failed to start GPU completion thread.")
            } else {
                None
            };
            if let Some(what) = failed {
                release_unsubmitted_frame(dec);
                (*cm).cur_frame = std::ptr::null_mut();
                State::unref(dec, cur);
                state.fail(sys::AOM_CODEC_MEM_ERROR, what);
                return Err(());
            }
            state.cur = cur;
            state.refresh = hdr.refresh_frame_flags;
            state.parse_start = av1_perf_ticks();
            Ok(FrameMemory { mi_grid: (*cm).mi_grid_base, mi_stride: (*cm).mi_stride })
        }
    }

    fn tile_begin(&self, tile: &ParseTile) -> TileMemory {
        // SAFETY: a call of the tile's thread inside the frame.
        unsafe {
            let cm = &*self.common();
            let index = tile.index as c_int;
            let info = TileInfo {
                mi_row_start: tile.row_start,
                mi_row_end: tile.row_end,
                mi_col_start: tile.col_start,
                mi_col_end: tile.col_end,
                tile_row: index / cm.tile_cols.max(1),
                tile_col: index % cm.tile_cols.max(1),
            };
            let setup = setup_tile(self.dec, cm, &info);
            TileMemory {
                tile_data: setup.tile_data,
                mi_pool: setup.mi_pool,
                mi_count_max: setup.mi_count_max,
                worst_case_coefs: setup.ext_idct_buffer != 0,
            }
        }
    }

    fn second_mode_info(&self, tile: usize, tile_data: *mut av1_tile_data) -> Option<SecondModeInfo> {
        // SAFETY: a call of the tile's thread inside the frame, with the cursors of the tile.
        let (tile_data, mi_pool) = unsafe { second_mode_info(self.dec, tile, &*tile_data) };
        Some(SecondModeInfo { tile_data, mi_pool })
    }

    fn second_coefs(&self, _tile: usize, tile_data: *mut av1_tile_data) {
        // SAFETY: as above.
        unsafe { second_coefs(self.dec, &mut *tile_data) };
    }

    fn no_room(&self, _tile: usize, mode_info: bool) {
        // The line the entropy decoder of libaom logs for the same frame (`decode_mbmi_block`,
        // `av1_read_coeffs_txb`).
        log(
            true,
            if mode_info {
                "MEMORY FAILED frame: the blocks of a tile do not fit its part of the mode info pool (two slot \
                 shares); the frame is a decode error"
            } else {
                "MEMORY FAILED frame: the coefficients of a tile do not fit its part of the coefficient buffer (two \
                 slot shares); the frame is a decode error"
            },
        );
    }

    fn palette(&self, _tile: usize, palette: &ParsePalette<'_>) {
        let mut colors = [0u16; 24];
        for (dst, plane) in colors.as_chunks_mut::<8>().0.iter_mut().zip(palette.pal) {
            *dst = *plane;
        }
        // The rows of the block outside the frame repeat the last one inside, as libaom's colour map
        // has them (`decode_color_map_tokens`).
        let map = PaletteMap { data: palette.map.as_ptr(), stride: palette.stride, rows: palette.h.max(1) };
        // SAFETY: a call of a tile's thread inside the frame; the map has `bw` entries a row.
        unsafe {
            write_palette(
                self.dec,
                palette.y4,
                palette.x4,
                usize::from(palette.plane),
                &colors,
                &map,
                palette.bw,
                palette.bh,
            );
        }
    }

    fn superblock(&self, _tile: usize, superblock: &ParseSuperblock<'_>) {
        // SAFETY: a call of a tile's thread inside the frame; a unit is read with one superblock, so it
        // is written by one thread, inside the memory of its plane (checked against the plane's count).
        unsafe {
            let cm = self.common();
            for unit in superblock.lr {
                let rsi = &raw const (*cm).rst_info[usize::from(unit.plane).min(2)];
                let index = c_int::from(unit.unit_y) * (*rsi).horz_units_per_tile + c_int::from(unit.unit_x);
                if (*rsi).unit_info.is_null() || index >= (*rsi).units_per_tile {
                    continue;
                }
                let info = &mut *(*rsi).unit_info.add(index as usize);
                match unit.r#type {
                    2 => {
                        info.restoration_type = RESTORE_WIENER as u8;
                        for (filter, taps) in [
                            (&mut info.wiener_info.vfilter, unit.filter_v),
                            (&mut info.wiener_info.hfilter, unit.filter_h),
                        ] {
                            let taps = taps.map(i16::from);
                            *filter = [
                                taps[0],
                                taps[1],
                                taps[2],
                                -2 * (taps[0] + taps[1] + taps[2]),
                                taps[2],
                                taps[1],
                                taps[0],
                                0,
                            ];
                        }
                    }
                    3 => {
                        info.restoration_type = RESTORE_SGRPROJ as u8;
                        info.sgrproj_info.ep = c_int::from(unit.sgr_idx);
                        info.sgrproj_info.xqd = [c_int::from(unit.sgr_weights[0]), c_int::from(unit.sgr_weights[1])];
                    }
                    _ => info.restoration_type = RESTORE_NONE as u8,
                }
            }
        }
    }

    fn tile_end(&self, _tile: usize) {}

    fn frame_end(&self, ok: bool) -> bool {
        // SAFETY: the thread of the decode call, with no tile in parse any more; the frame is recorded
        // and submitted as the entropy decoder has it recorded behind its last tile.
        unsafe {
            let state = self.state();
            let dec = self.dec;
            let cur = std::mem::replace(&mut state.cur, std::ptr::null_mut());
            if cur.is_null() {
                return false;
            }
            perf_add(dec, sys::AV1_PERF_CPU_ENTROPY_WALL as usize, state.parse_start);
            let pbi: *mut AV1Decoder = &raw mut *state.pbi;
            if ok {
                let mut fb: av1_decoded_frame_buffer_t = std::mem::zeroed();
                let mut lap_start = 0;
                let mut image_state = rav1g_decode_frame_record(pbi, &mut fb, &mut lap_start);
                if image_state == 2 {
                    // The application has no image for the frame. The frame is decoded all the same: it
                    // is a reference of the frames that follow, and only its output is lost.
                    state.fail(sys::AOM_CODEC_MEM_ERROR, "Failed to allocate output frame.");
                    image_state = 0;
                }
                rav1g_decode_frame_submit(pbi, image_state, &fb, lap_start);
                for slot in (0..8).filter(|slot| state.refresh & (1 << slot) != 0) {
                    State::unref(dec, state.map[slot]);
                    (*cur).ref_count += 1;
                    state.map[slot] = cur;
                }
            } else {
                // The frame returns its slot. The slots it was to refresh are empty from here on, as
                // rav1d leaves its own.
                release_unsubmitted_frame(dec);
                for slot in (0..8).filter(|slot| state.refresh & (1 << slot) != 0) {
                    State::unref(dec, std::mem::replace(&mut state.map[slot], std::ptr::null_mut()));
                }
                state.fail(
                    sys::AOM_CODEC_CORRUPT_FRAME,
                    "Failed to decode tile data: the parse of the frame ended with an error",
                );
            }
            (*pbi).common.cur_frame = std::ptr::null_mut();
            State::unref(dec, cur);
            ok
        }
    }
}

/// The decoder's parser of phase 2: rav1d's parse pass, the sink, and the host of the sink.
pub(crate) struct Rav1dHost {
    // Dropped first: it joins rav1d's threads, which call the sink.
    parser: Parser,
    sink: Arc<Sink<SlotHost>>,
    /// The detail of the last error, for the C interface.
    detail: CString,
    /// A panic left the parser in a state nothing is known about: every further call fails.
    broken: bool,
}

impl Rav1dHost {
    fn new(dec: *mut Av1Core, threads: c_int, operating_point: c_int) -> Option<Box<Rav1dHost>> {
        let mut state = State::new(dec);
        state.pbi.max_threads = threads.clamp(1, MAX_THREADS);
        state.pbi.operating_point = operating_point;
        let sink = Arc::new(Sink::new(SlotHost { dec, state: UnsafeCell::new(state) }));
        let settings = ParseSettings {
            threads: threads.clamp(1, MAX_THREADS) as u16,
            frames: 1,
            operating_point: u8::try_from(operating_point).ok().filter(|&point| point < 32).unwrap_or(0),
            frame_size_limit: 0,
        };
        let parser = Parser::new(&settings, sink.clone()).ok()?;
        Some(Box::new(Rav1dHost { parser, sink, detail: CString::default(), broken: false }))
    }
}

/// The decode call of a decoder whose parser is rav1d's (`decoder_decode` of `av1/av1_dx_iface.c`): the
/// frames of a temporal unit are parsed, recorded and submitted. Returns the error code of the interface
/// and, in `detail`, a text for it that lives until the next call.
///
/// # Safety
/// `dec` is a decoder; `data` is valid for `size` bytes; `detail` is valid for writing; the thread is
/// the one the decoder is used from.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_rav1d_decode(
    dec: *mut Av1Core,
    data: *const u8,
    size: usize,
    user_priv: *mut c_void,
    operating_point: c_int,
    output_all_layers: c_int,
    detail: *mut *const c_char,
) -> sys::aom_codec_err_t {
    // SAFETY: the caller's contract, for all of this function.
    unsafe {
        if (*dec).rav1d.is_none() {
            (*dec).rav1d = Rav1dHost::new(dec, (*dec).parser_threads, operating_point);
        }
        let Some(host) = (*dec).rav1d.as_mut() else {
            *detail = c"The parser could not be created".as_ptr();
            return sys::AOM_CODEC_MEM_ERROR;
        };
        if host.broken {
            *detail = c"The parser stopped with a panic in an earlier call".as_ptr();
            return sys::AOM_CODEC_ERROR;
        }
        {
            let state = host.sink.host().state();
            state.unit = data;
            state.unit_len = size;
            state.failure = None;
            state.pbi.user_priv = user_priv;
            state.pbi.output_all_layers = output_all_layers;
        }
        let unit = std::slice::from_raw_parts(data, size);
        let parser = &host.parser;
        let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.send(unit)));
        let state = host.sink.host().state();
        state.unit = std::ptr::null();
        state.unit_len = 0;
        let failure = match sent {
            Err(_) => {
                host.broken = true;
                log(true, "PARSER the parser of the decoder stopped with a panic; the decoder decodes nothing more");
                Some(Failure { code: sys::AOM_CODEC_ERROR, detail: String::from("The parser stopped with a panic") })
            }
            Ok(result) => state.failure.take().or_else(|| {
                result.err().map(|error| Failure {
                    code: sys::AOM_CODEC_CORRUPT_FRAME,
                    detail: format!(
                        "The temporal unit could not be parsed ({error:?}): a header that cannot be read, or a \
                         frame whose references are missing"
                    ),
                })
            }),
        };
        match failure {
            None => sys::AOM_CODEC_OK,
            Some(failure) => {
                host.detail = CString::new(failure.detail).unwrap_or_default();
                *detail = host.detail.as_ptr();
                failure.code
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_of_an_obu() {
        // A temporal delimiter with a size field of 0; a frame header OBU with an extension and 3 bytes.
        assert_eq!(obu_size(&[0x12, 0x00, 0xff]), Some(2));
        assert_eq!(obu_size(&[0x1e, 0x08, 0x03, 1, 2, 3, 9]), Some(6));
        // Two bytes of size: 0x80 | 1, 1 is 129.
        assert_eq!(obu_size(&[0x32, 0x81, 0x01]), Some(3 + 129));
        // No size field, no data, a size that does not end.
        assert_eq!(obu_size(&[0x30, 1, 2]), None);
        assert_eq!(obu_size(&[]), None);
        assert_eq!(obu_size(&[0x32, 0x80]), None);
    }

    #[test]
    fn film_grain_fields_a_header_did_not_have_keep_their_value() {
        // SAFETY: all zero is a value of the parameters.
        let mut pars: aom_film_grain_t = unsafe { std::mem::zeroed() };
        pars.ar_coeffs_y[3] = 7;
        pars.cb_mult = 99;
        let data = Rav1dFilmGrainData { ar_coeff_lag: 1, scaling_shift: 9, ..Default::default() };
        write_film_grain(&mut pars, &data);
        assert_eq!(pars.ar_coeffs_y[3], 7);
        assert_eq!(pars.cb_mult, 99);
        assert_eq!(pars.scaling_shift, 9);
        let data = Rav1dFilmGrainData {
            num_y_points: 1,
            num_uv_points: [1, 0],
            ar_coeff_lag: 1,
            ar_coeffs_y: [-5; 24],
            uv_mult: [-128, 3],
            ..Default::default()
        };
        write_film_grain(&mut pars, &data);
        assert_eq!(pars.ar_coeffs_y[3], -5);
        assert_eq!(pars.ar_coeffs_y[4], 0);
        assert_eq!((pars.cb_mult, pars.cb_luma_mult, pars.cb_offset), (0, 128, 256));
        assert_eq!(pars.cr_mult, 0);
    }
}

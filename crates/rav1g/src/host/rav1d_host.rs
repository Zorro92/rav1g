// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The host on the sink of rav1d's parse pass (docs/phase2-rav1d.md, steps 3 and 4): the decoder with the
//! parser of phase 2 in place of the entropy decoder of libaom.
//!
//! With one frame in parse at a time (the decoder of step 3, and what every decoder is unless it was
//! created for more: `Av1Core::frames_in_parse`) a frame is "the frame of the decoder" from its setup to
//! its submission, all on the thread of the decode call or under it. With more frames in parse a frame is
//! a value: `rav1d_frames.rs` has what that means, and the functions here that end in `_value` are the
//! calls of the sink for such a decoder.
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
use std::thread::JoinHandle;

use rav1g_parse::options::ParseOptions;
use rav1g_parse::rav1d::include::dav1d::headers::{
    Rav1dFilmGrainData, Rav1dFrameHeader, Rav1dFrameType, Rav1dMatrixCoefficients, Rav1dPixelLayout,
    Rav1dRestorationType, Rav1dSequenceHeader,
};
use rav1g_parse::rav1d::parse::{
    ParseColorCodes, ParseFrame, ParseHdrMetadata, ParsePalette, ParseSettings, ParseShowExisting, ParseSuperblock,
    ParseTile, Parser,
};
use rav1g_parse::sink::{FrameMemory, SecondModeInfo, Sink, SinkHost, SinkTimer, TileMemory};

use super::completion::{rav1g_decode_sef_begin, rav1g_decode_sef_submit, start_completion_thread};
use super::frame::{
    PaletteMap, av1_reallocate_frame_buffer, av1_sequence_fits_decoder, rav1g_decode_frame_record,
    rav1g_decode_frame_submit, rav1g_setup_frame_begin, rav1g_setup_frame_coefs, rav1g_setup_frame_end, release_frame,
    release_unsubmitted_frame, second_coefs, second_mode_info, setup_frame_begin, setup_frame_coefs, setup_frame_end,
    setup_tile, write_palette,
};
use super::log::log;
use super::perf::{av1_perf_ticks, frame_timers, perf_add, perf_add_ticks};
use super::rav1d_frames::{Descriptions, Entry, Failure, Frames, zeroed_box};
use super::thread::spawn_thread;
use super::types::{Av1Core, av1_frame_thread_data};
use crate::sys;
use crate::sys::parser::{
    AV1_COMMON, AV1Decoder, INVALID_IDX, REF_NO_SCALE, RESTORE_NONE, RESTORE_SGRPROJ, RESTORE_SWITCHABLE,
    RESTORE_WIENER, RefCntBuffer, RestorationInfo, TileInfo, WarpedMotionParams, aom_film_grain_t, aom_hdr_metadata_t,
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
    /// `av1/decoder/obu.c`: the frame is the one of its temporal unit that is output.
    fn av1_frame_is_output(pbi: *const AV1Decoder) -> c_int;
}

/// `SCALE_NUMERATOR` of `av1/common/scale.h`: the superres denominator of a frame that is not scaled.
const SCALE_NUMERATOR: u8 = 8;
/// `WARPEDMODEL_PREC_BITS` of `av1/common/warped_motion.h`.
const WARPEDMODEL_PREC_BITS: u32 = 16;
/// The entropy decoder's limit of tile threads (`decoder_init` of `av1/av1_dx_iface.c`).
const MAX_THREADS: c_int = 8;

/// What the host keeps between the calls of the sink. Written on the thread of the decode call
/// (`sequence`, `frame_begin`, `show_existing`, and with one frame in parse `frame_end`). With one frame
/// in parse the tiles read the header and write the loop restoration units, each unit by one tile, and
/// nothing here is written while tiles are in parse. With more, the tiles and the end of a frame do not
/// come here at all: they have the frame's entry (`rav1d_frames::Entry`), and this is the state of the
/// stream as the thread of the decode calls has it, in decode order.
struct State {
    /// The frame header as the stages read it, and the block context of the frame (`mb`). It goes from
    /// frame to frame in decode order: a field a frame does not write holds what an earlier frame left
    /// (`fill_header`). With more than one frame in parse a frame takes its copy of it when it is set
    /// up.
    pbi: Box<AV1Decoder>,
    /// The frame descriptions (frame buffer, order hint, film grain) the map and the frames in parse
    /// use.
    descriptions: Arc<Descriptions>,
    /// The eight reference slots.
    map: [*mut RefCntBuffer; 8],
    /// With one frame in parse: the frame in parse, and the slots it refreshes.
    cur: *mut RefCntBuffer,
    refresh: u8,
    /// The colour description of the sequence, as its header codes it.
    color: ParseColorCodes,
    /// The data of the decode call.
    unit: *const u8,
    unit_len: usize,
    /// The start of the parse of the frame, for its timer.
    parse_start: i64,
    /// The first thing that failed in the decode call.
    failure: Option<Failure>,
}

impl State {
    fn new(dec: *mut Av1Core) -> State {
        let mut pbi = zeroed_box::<AV1Decoder>();
        pbi.gpu_decoder = dec.cast();
        // SAFETY: a cleared frame header; the function writes its loop filter tables.
        unsafe { av1_loop_filter_init(&raw mut pbi.common) };
        pbi.mb.global_motion = pbi.common.global_motion.as_mut_ptr();
        State {
            pbi,
            descriptions: Arc::new(Descriptions::new()),
            map: [std::ptr::null_mut(); 8],
            cur: std::ptr::null_mut(),
            refresh: 0,
            color: ParseColorCodes::default(),
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
        self.descriptions.new_frame()
    }

    /// Gives a reference to a frame description back; with its last one the frame buffer returns to the
    /// pool, and an output image the frame still had goes back to the application.
    ///
    /// # Safety
    /// `frame` is null or a description of this state with a reference; `dec` is the decoder.
    unsafe fn unref(&self, dec: *mut Av1Core, frame: *mut RefCntBuffer) {
        // SAFETY: the caller's contract.
        unsafe { self.descriptions.unref(dec, frame) };
    }

    /// Empties the reference slots. The frames in parse keep the references they have taken.
    ///
    /// # Safety
    /// `dec` is the decoder; the thread of the decode call.
    unsafe fn clear_map(&mut self, dec: *mut Av1Core) {
        for slot in 0..8 {
            // SAFETY: the caller's contract; a slot holds a reference to its description.
            let frame = std::mem::replace(&mut self.map[slot], std::ptr::null_mut());
            unsafe { self.unref(dec, frame) };
        }
    }

    /// The reference slot holds a frame that later frames can use: one with a frame buffer that is not
    /// known to have failed (with more than one frame in parse, the frame of a slot may still be in
    /// parse; if it fails, the frames that took it fail when their turn comes).
    ///
    /// # Safety
    /// The thread of the decode call.
    unsafe fn slot_is_whole(&self, slot: usize) -> bool {
        let reference = self.map[slot & 7];
        // SAFETY: a slot is null or a description with a reference.
        unsafe { !reference.is_null() && !(*reference).buf.hw_buffer.is_null() && !self.descriptions.failed(reference) }
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
    /// A decoder with more than one frame in parse: its frames in parse and the queue of their
    /// submission.
    frames: Option<Arc<Frames>>,
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
    let color = state.color;
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
    // The colour description (`av1_read_color_config`), which no stage reads: the output image of the
    // frame takes it from here for the application.
    sp.color_description_present_flag = u8::from(seq.color_description_present);
    sp.color_primaries = color.primaries.into();
    sp.transfer_characteristics = color.transfer.into();
    sp.matrix_coefficients = color.matrix.into();
    sp.color_range = c_int::from(seq.color_range.is_full());
    sp.chroma_sample_position = seq.chr as _;

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
    // `setup_superres`: a sequence without superres does not write the denominator.
    if seq.super_res != 0 {
        cm.superres_scale_denominator =
            if hdr.size.super_res.enabled { hdr.size.super_res.width_scale_denominator } else { SCALE_NUMERATOR };
    }
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
    cm.hdr_metadata = hdr_metadata(&frame.hdr);
}

/// The HDR metadata rav1d has for a frame, as the output image of the frame carries it.
fn hdr_metadata(hdr: &ParseHdrMetadata<'_>) -> aom_hdr_metadata_t {
    // SAFETY: a C struct of numbers.
    let mut out: aom_hdr_metadata_t = unsafe { std::mem::zeroed() };
    if let Some(cll) = hdr.content_light {
        out.has_cll = 1;
        out.max_cll = cll.max_content_light_level;
        out.max_fall = cll.max_frame_average_light_level;
    }
    if let Some(mdcv) = hdr.mastering_display {
        out.has_mdcv = 1;
        out.mdcv_primaries = mdcv.primaries;
        out.mdcv_white_point = mdcv.white_point;
        out.mdcv_luminance_max = mdcv.max_luminance;
        out.mdcv_luminance_min = mdcv.min_luminance;
    }
    out
}

/// The tile of a frame as the frame setup takes it.
fn tile_info(cm: &AV1_COMMON, tile: &ParseTile) -> TileInfo {
    let index = tile.index as c_int;
    TileInfo {
        mi_row_start: tile.row_start,
        mi_row_end: tile.row_end,
        mi_col_start: tile.col_start,
        mi_col_end: tile.col_end,
        tile_row: index / cm.tile_cols.max(1),
        tile_col: index % cm.tile_cols.max(1),
    }
}

/// The loop restoration units a superblock has read go into the units of the frame of `cm`.
///
/// # Safety
/// `cm` is the header of the frame the superblock belongs to, with the memory of its units; a unit is
/// read with one superblock, so it is written by one thread, inside the memory of its plane (checked
/// against the plane's count).
unsafe fn write_restoration_units(cm: *const AV1_COMMON, superblock: &ParseSuperblock<'_>) {
    // SAFETY: the caller's contract.
    unsafe {
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
                    for (filter, taps) in
                        [(&mut info.wiener_info.vfilter, unit.filter_v), (&mut info.wiener_info.hfilter, unit.filter_h)]
                    {
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

/// The colour index map of a palette block's plane goes into the palette buffer of the slot `td`.
///
/// # Safety
/// `dec` is the decoder while the tiles of the frame on the slot `td` are parsed; a call of a tile's
/// thread; the map has `bw` entries a row.
unsafe fn palette_block(dec: *mut Av1Core, td: *const av1_frame_thread_data, palette: &ParsePalette<'_>) {
    let mut colors = [0u16; 24];
    for (dst, plane) in colors.as_chunks_mut::<8>().0.iter_mut().zip(palette.pal) {
        *dst = *plane;
    }
    // The rows of the block outside the frame repeat the last one inside, as libaom's colour map
    // has them (`decode_color_map_tokens`).
    let map = PaletteMap { data: palette.map.as_ptr(), stride: palette.stride, rows: palette.h.max(1) };
    // SAFETY: the caller's contract.
    unsafe {
        write_palette(
            dec,
            td,
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

/// The calls of the sink for a decoder with more than one frame in parse (`rav1d_frames.rs`).
impl SlotHost {
    /// The entry of the frame with a number of the sink.
    ///
    /// # Safety
    /// Between the `frame_begin` and the `frame_end` of the frame with this number.
    unsafe fn entry(frames: &Frames, sink_frame: usize) -> &Entry {
        // SAFETY: the caller's contract: the entry is the frame's and is not written while its tiles
        // are in parse.
        unsafe { &*frames.entry(frames.entry_of(sink_frame)) }
    }

    /// `frame_begin`: the frame gets an entry, its place in the queue of the submission, a slot and its
    /// buffers, a copy of its header, and its references; the reference slots it refreshes refer to it
    /// from here on.
    ///
    /// # Safety
    /// The thread of the decode call.
    unsafe fn frame_begin_value(
        &self,
        frames: &Frames,
        sink_frame: usize,
        frame: &ParseFrame<'_>,
    ) -> Result<FrameMemory, ()> {
        // SAFETY: the caller's contract; the functions of the frame setup are called as the entropy
        // decoder calls them, with the header filled in, for a frame that is a value.
        unsafe {
            let state = self.state();
            let hdr = frame.frame_hdr;
            let dec = self.dec;
            let inter = matches!(hdr.frame_type, Rav1dFrameType::Inter | Rav1dFrameType::Switch);
            if inter && !hdr.refidx.iter().all(|&slot| state.slot_is_whole(slot as usize)) {
                // A frame that names a frame that failed fails, and is a failed frame for the frames
                // behind it: the slots it was to refresh are empty from here on. That is what becomes of
                // them when the reference fails only after this frame has begun (the frame is then not
                // recorded, and its description has the mark), so what a stream decodes to does not
                // depend on which of the two happened first.
                for slot in (0..8).filter(|slot| hdr.refresh_frame_flags & (1 << slot) != 0) {
                    let frame = std::mem::replace(&mut state.map[slot], std::ptr::null_mut());
                    state.unref(dec, frame);
                }
                state.fail(sys::AOM_CODEC_CORRUPT_FRAME, "A reference of the frame has no frame buffer");
                return Err(());
            }
            let Some(index) = frames.acquire() else {
                state.fail(sys::AOM_CODEC_ERROR, "The decoder has stopped");
                return Err(());
            };
            let Some(cur) = state.new_frame() else {
                frames.release(index);
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
                state.descriptions.abandon(cur);
                frames.release(index);
                state.fail(sys::AOM_CODEC_MEM_ERROR, "Failed to allocate frame buffer");
                return Err(());
            }
            fill_header(state, dec, frame, cur);

            let entry = &mut *frames.entry(index);
            entry.shown = false;
            entry.td = std::ptr::null_mut();
            entry.acc = std::mem::zeroed();
            // The timers of this thread are the frame's until the setup is through.
            let _timers = frame_timers(&raw mut entry.acc);
            // The frame is the next in decode order; its slot, when the frames before it leave one.
            frames.push(index, None);
            frames.gate(index, false);
            let pbi: *mut AV1Decoder = &raw mut *state.pbi;
            let cm = &raw mut (*pbi).common;
            let mut perf_start = 0;
            let (td, no_palette) = setup_frame_begin(pbi, cm, &mut perf_start, false);
            frames.set_slot(index, td);
            let failed = if no_palette != 0 {
                Some("No palette buffer for a frame that allows screen content tools")
            } else if setup_frame_coefs(dec, td, cm) != 0 {
                Some("No room for the coefficients of a frame with very small tiles")
            } else if setup_frame_end(dec, td, cm, perf_start) != 0 {
                Some("Failed to start GPU completion thread.")
            } else {
                None
            };
            if let Some(what) = failed {
                release_frame(dec, td);
                (*cm).cur_frame = std::ptr::null_mut();
                state.unref(dec, cur);
                frames.abort(index);
                state.fail(sys::AOM_CODEC_MEM_ERROR, what);
                return Err(());
            }

            // The frame's own copy of the header, as it is now: the tiles and the recording read it
            // while the header here goes on to the frames that follow.
            entry.td = td;
            entry.is_output = hdr.show_frame != 0 && av1_frame_is_output(pbi) != 0;
            entry.user_priv = (*pbi).user_priv;
            (&raw mut entry.header.mb).copy_from_nonoverlapping(&raw const (*pbi).mb, 1);
            (&raw mut entry.header.common).copy_from_nonoverlapping(cm, 1);
            // The loop restoration units of the frame: memory of its own, which its tiles write.
            let planes = if frame.seq_hdr.monochrome != 0 { 1 } else { 3 };
            for p in 0..planes {
                let rsi = &mut entry.header.common.rst_info[p];
                if rsi.frame_restoration_type == RESTORE_NONE as u8 {
                    continue;
                }
                let units = usize::try_from(rsi.units_per_tile).unwrap_or(0);
                let memory = &mut entry.lr[p];
                if memory.len() < units {
                    memory.resize_with(units, || std::mem::zeroed());
                }
                rsi.unit_info = memory.as_mut_ptr();
            }
            // The references: the frame's own description (the one reference it has since it was
            // taken), and those of the frames it is predicted from.
            entry.cur = cur;
            entry.refs = [std::ptr::null_mut(); 7];
            if inter {
                let map = (*cm).ref_frame_map;
                for (held, &reference) in entry.refs.iter_mut().zip(&map[..7]) {
                    state.descriptions.add_ref(reference);
                    *held = reference;
                }
            }
            // The slots the frame refreshes hold it from here on, for the frames that begin behind it.
            for slot in (0..8).filter(|slot| hdr.refresh_frame_flags & (1 << slot) != 0) {
                state.unref(dec, state.map[slot]);
                state.descriptions.add_ref(cur);
                state.map[slot] = cur;
            }
            (*cm).cur_frame = std::ptr::null_mut();
            frames.bind(sink_frame, index);
            entry.parse_start = av1_perf_ticks();
            Ok(FrameMemory { mi_grid: entry.header.common.mi_grid_base, mi_stride: entry.header.common.mi_stride })
        }
    }

    /// `show_existing`: the output copy of the frame in a reference slot goes into the queue of the
    /// submission, behind the frames that began before this header.
    ///
    /// # Safety
    /// The thread of the decode call.
    unsafe fn show_existing_value(&self, frames: &Frames, show: &ParseShowExisting<'_>) -> Result<(), ()> {
        // SAFETY: the caller's contract.
        unsafe {
            let state = self.state();
            let dec = self.dec;
            let slot = usize::from(show.frame_hdr.existing_frame_idx) & 7;
            if !state.slot_is_whole(slot) {
                // A key frame that is shown is the reference in every slot from here on; one that
                // failed leaves every slot empty, as it does when it fails only behind this header.
                if show.key {
                    state.clear_map(dec);
                }
                state.fail(sys::AOM_CODEC_UNSUP_BITSTREAM, "Buffer does not contain a decoded frame");
                return Err(());
            }
            let shown = state.map[slot];
            state.set_next_obu(show.data_left);
            let pbi: *mut AV1Decoder = &raw mut *state.pbi;
            let cm = &raw mut (*pbi).common;
            (*cm).cur_frame = shown;
            (*cm).show_frame = 1;
            (*cm).show_existing_frame = 1;
            (*cm).film_grain_params = (*shown).film_grain_params;
            (*cm).temporal_layer_id = c_int::from(show.frame_hdr.temporal_id);
            (*cm).spatial_layer_id = c_int::from(show.frame_hdr.spatial_id);
            // The frame is shown with the HDR metadata of now, as rav1d shows it.
            (*cm).hdr_metadata = hdr_metadata(&show.hdr);
            let is_output = av1_frame_is_output(pbi) != 0;
            // The completion thread ends with a drain; a show_existing_frame may be the first thing
            // after one. It is started here, by the thread that starts it for a frame.
            let entry = if start_completion_thread(dec) { frames.acquire() } else { None };
            let Some(index) = entry else {
                (*cm).cur_frame = std::ptr::null_mut();
                state.fail(sys::AOM_CODEC_MEM_ERROR, "Failed to start GPU completion thread.");
                return Err(());
            };
            let entry = &mut *frames.entry(index);
            entry.shown = true;
            entry.td = std::ptr::null_mut();
            entry.acc = std::mem::zeroed();
            entry.is_output = is_output;
            entry.user_priv = (*pbi).user_priv;
            (&raw mut entry.header.mb).copy_from_nonoverlapping(&raw const (*pbi).mb, 1);
            (&raw mut entry.header.common).copy_from_nonoverlapping(cm, 1);
            state.descriptions.add_ref(shown);
            entry.cur = shown;
            entry.refs = [std::ptr::null_mut(); 7];
            (*cm).cur_frame = std::ptr::null_mut();
            frames.push(index, Some(true));
            if show.key {
                // The key frame is the reference in every slot from here on.
                (*shown).showable_frame = 0;
                for other in (0..8).filter(|&other| other != slot) {
                    state.unref(dec, state.map[other]);
                    state.descriptions.add_ref(shown);
                    state.map[other] = shown;
                }
            }
        }
        Ok(())
    }
}

impl SinkHost for SlotHost {
    fn sequence(&self, seq: &Rav1dSequenceHeader, color: ParseColorCodes, new: bool) -> Result<(), ()> {
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
            state.color = color;
            // The operating point the application chose, as rav1d takes it: the first one when the
            // sequence does not have that many.
            let point = usize::try_from(state.pbi.operating_point).unwrap_or(0);
            let point = if point < usize::from(seq.num_operating_points) { point } else { 0 };
            state.pbi.current_operating_point = c_int::from(seq.operating_points[point].idc);
        }
        Ok(())
    }

    fn show_existing(&self, show: &ParseShowExisting<'_>) -> Result<(), ()> {
        if let Some(frames) = &self.frames {
            // SAFETY: the thread of the decode call.
            return unsafe { self.show_existing_value(frames, show) };
        }
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
            // The frame is shown with the HDR metadata of now, as rav1d shows it.
            (*cm).hdr_metadata = hdr_metadata(&show.hdr);
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
                    state.unref(self.dec, state.map[other]);
                    state.descriptions.add_ref(shown);
                    state.map[other] = shown;
                }
            }
        }
        Ok(())
    }

    fn ticks(&self) -> i64 {
        av1_perf_ticks()
    }

    fn tile_time(&self, frame: usize, start: i64) {
        if start == 0 {
            return;
        }
        // SAFETY: the decoder; the timer is added to under its own rule for the tile threads. With more
        // than one frame in parse it is a timer of the frame's entry.
        unsafe {
            let _timers =
                self.frames.as_ref().map(|frames| frame_timers(&raw const Self::entry(frames, frame).acc as *mut _));
            perf_add(self.dec, sys::AV1_PERF_CPU_ENTROPY_SUM as usize, start);
        }
    }

    fn add_time(&self, frame: usize, timer: SinkTimer, ticks: i64) {
        let id = match timer {
            SinkTimer::Calls => sys::AV1_PERF_CPU_SINK_CALLS,
            SinkTimer::Lists => sys::AV1_PERF_CPU_SINK_LISTS,
        };
        // SAFETY: as above.
        unsafe {
            let _timers =
                self.frames.as_ref().map(|frames| frame_timers(&raw const Self::entry(frames, frame).acc as *mut _));
            perf_add_ticks(self.dec, id as usize, ticks);
        }
    }

    fn frame_begin(&self, sink_frame: usize, frame: &ParseFrame<'_>) -> Result<FrameMemory, ()> {
        if let Some(frames) = &self.frames {
            // SAFETY: the thread of the decode call.
            return unsafe { self.frame_begin_value(frames, sink_frame, frame) };
        }
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
                state.descriptions.abandon(cur);
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
                state.unref(dec, cur);
                state.fail(sys::AOM_CODEC_MEM_ERROR, what);
                return Err(());
            }
            state.cur = cur;
            state.refresh = hdr.refresh_frame_flags;
            state.parse_start = av1_perf_ticks();
            Ok(FrameMemory { mi_grid: (*cm).mi_grid_base, mi_stride: (*cm).mi_stride })
        }
    }

    fn tile_begin(&self, frame: usize, tile: &ParseTile) -> TileMemory {
        // SAFETY: a call of the tile's thread inside the frame: with more than one frame in parse with
        // the frame's entry, its header, its slot and its timers.
        unsafe {
            let setup = match &self.frames {
                Some(frames) => {
                    let entry = Self::entry(frames, frame);
                    let _timers = frame_timers(&raw const entry.acc as *mut _);
                    let cm = &entry.header.common;
                    setup_tile(self.dec, entry.td, cm, &tile_info(cm, tile))
                }
                None => {
                    let cm = &*self.common();
                    setup_tile(self.dec, (*self.dec).curr_frame_data, cm, &tile_info(cm, tile))
                }
            };
            TileMemory {
                tile_data: setup.tile_data,
                mi_pool: setup.mi_pool,
                mi_count_max: setup.mi_count_max,
                worst_case_coefs: setup.ext_idct_buffer != 0,
            }
        }
    }

    fn second_mode_info(&self, frame: usize, tile: usize, tile_data: *mut av1_tile_data) -> Option<SecondModeInfo> {
        // SAFETY: a call of the tile's thread inside the frame, with the cursors of the tile.
        let (tile_data, mi_pool) = unsafe {
            match &self.frames {
                Some(frames) => {
                    let entry = Self::entry(frames, frame);
                    let _timers = frame_timers(&raw const entry.acc as *mut _);
                    second_mode_info(self.dec, entry.td, tile, &*tile_data)
                }
                None => second_mode_info(self.dec, (*self.dec).curr_frame_data, tile, &*tile_data),
            }
        };
        Some(SecondModeInfo { tile_data, mi_pool })
    }

    fn second_coefs(&self, frame: usize, _tile: usize, tile_data: *mut av1_tile_data) {
        // SAFETY: as above.
        unsafe {
            match &self.frames {
                Some(frames) => {
                    let entry = Self::entry(frames, frame);
                    let _timers = frame_timers(&raw const entry.acc as *mut _);
                    second_coefs(self.dec, entry.td, &mut *tile_data);
                }
                None => second_coefs(self.dec, (*self.dec).curr_frame_data, &mut *tile_data),
            }
        }
    }

    fn no_room(&self, _frame: usize, _tile: usize, mode_info: bool) {
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

    fn palette(&self, frame: usize, _tile: usize, palette: &ParsePalette<'_>) {
        // SAFETY: a call of a tile's thread inside the frame; the map has `bw` entries a row.
        unsafe {
            let td = match &self.frames {
                Some(frames) => Self::entry(frames, frame).td,
                None => (*self.dec).curr_frame_data,
            };
            palette_block(self.dec, td, palette);
        }
    }

    fn superblock(&self, frame: usize, _tile: usize, superblock: &ParseSuperblock<'_>) {
        if superblock.lr.is_empty() {
            return;
        }
        // SAFETY: a call of a tile's thread inside the frame, with the header of the frame.
        unsafe {
            let cm = match &self.frames {
                Some(frames) => &raw const Self::entry(frames, frame).header.common,
                None => self.common().cast_const(),
            };
            write_restoration_units(cm, superblock);
        }
    }

    fn tile_end(&self, _frame: usize, _tile: usize) {}

    fn frame_end(&self, sink_frame: usize, ok: bool) -> bool {
        if let Some(frames) = &self.frames {
            // The parse of the frame has ended, on whichever thread parsed its last part: the frame
            // waits for its turn at the submission thread. Nothing of the state of the decode calls is
            // touched here.
            let index = frames.entry_of(sink_frame);
            // SAFETY: the entry of the frame, whose tiles are through.
            unsafe {
                let entry = &*frames.entry(index);
                let _timers = frame_timers(&raw const entry.acc as *mut _);
                perf_add(self.dec, sys::AV1_PERF_CPU_ENTROPY_WALL as usize, entry.parse_start);
            }
            frames.parsed(index, ok);
            return ok;
        }
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
                    state.unref(dec, state.map[slot]);
                    state.descriptions.add_ref(cur);
                    state.map[slot] = cur;
                }
            } else {
                // The frame returns its slot. The slots it was to refresh are empty from here on, as
                // rav1d leaves its own.
                release_unsubmitted_frame(dec);
                for slot in (0..8).filter(|slot| state.refresh & (1 << slot) != 0) {
                    let frame = std::mem::replace(&mut state.map[slot], std::ptr::null_mut());
                    state.unref(dec, frame);
                }
                state.fail(
                    sys::AOM_CODEC_CORRUPT_FRAME,
                    "Failed to decode tile data: the parse of the frame ended with an error",
                );
            }
            (*pbi).common.cur_frame = std::ptr::null_mut();
            state.unref(dec, cur);
            ok
        }
    }
}

/// The decoder's parser of phase 2: rav1d's parse pass, the sink, and the host of the sink.
pub(crate) struct Rav1dHost {
    // Dropped first: it joins rav1d's threads, which call the sink.
    parser: Parser,
    sink: Arc<Sink<SlotHost>>,
    /// With more than one frame in parse: the thread that records and submits the frames
    /// (`Frames::run`).
    submitter: Option<JoinHandle<()>>,
    /// The operating point the parser has.
    operating_point: c_int,
    /// The detail of the last error, for the C interface.
    detail: CString,
    /// A panic left the parser in a state nothing is known about: every further call fails.
    broken: bool,
}

/// The queue of a decoder for its submission thread.
struct FramesPtr(Arc<Frames>);

// SAFETY: the queue is made to be shared by the threads of a decoder (`Frames`).
unsafe impl Send for FramesPtr {}

impl Rav1dHost {
    /// `frames`: the frames the decoder may have in parse at once (`Av1Core::frames_in_parse`).
    fn new(
        dec: *mut Av1Core,
        threads: c_int,
        frames: c_int,
        slots: c_int,
        options: ParseOptions,
        operating_point: c_int,
    ) -> Option<Box<Rav1dHost>> {
        let threads = threads.clamp(1, MAX_THREADS);
        // What rav1d makes of the two numbers: at most as many frames as threads.
        let mut frames = frames.clamp(1, threads) as usize;
        let mut state = State::new(dec);
        state.pbi.max_threads = threads;
        state.pbi.operating_point = operating_point;
        // More than one frame in parse: the frames are values, and a thread records and submits them in
        // decode order. A system that gives no thread for that gets the decoder of one frame in parse.
        let mut submitter = None;
        let mut queue = None;
        if frames > 1 {
            let shared =
                Arc::new(Frames::new(dec, usize::try_from(slots).unwrap_or(2), frames, state.descriptions.clone()));
            let thread = FramesPtr(shared.clone());
            let spawned = spawn_thread(move || {
                let thread = thread;
                // SAFETY: the decoder joins this thread before it is released (`stop_frames`), and this
                // is the one thread that records its frames.
                unsafe { thread.0.run() };
            });
            match spawned {
                Ok(thread) => {
                    submitter = Some(thread);
                    queue = Some(shared);
                }
                Err(error) => {
                    log(
                        true,
                        &format!("MEMORY FAILED submission thread: {error}; the decoder parses one frame at a time"),
                    );
                    frames = 1;
                }
            }
        }
        // SAFETY: the decoder; set before a thread of the parser exists.
        unsafe {
            (*dec).frames_in_parse = frames as c_int;
            (*dec).parse_frames = queue.as_ref().map_or(std::ptr::null(), |queue| Arc::as_ptr(queue));
        }
        let host = SlotHost { dec, state: UnsafeCell::new(state), frames: queue };
        let sink = Arc::new(Sink::with_frames(host, options.sink(), frames));
        let mut settings = ParseSettings {
            threads: threads as u16,
            frames: frames as u16,
            operating_point: u8::try_from(operating_point).ok().filter(|&point| point < 32).unwrap_or(0),
            frame_size_limit: 0,
            ..Default::default()
        };
        options.apply(&mut settings);
        let parser = match Parser::new(&settings, sink.clone()) {
            Ok(parser) => parser,
            Err(_) => {
                if let Some(queue) = &sink.host().frames {
                    queue.stop();
                }
                if let Some(thread) = submitter {
                    let _ = thread.join();
                }
                // SAFETY: the decoder; the queue goes with the sink.
                unsafe { (*dec).parse_frames = std::ptr::null() };
                return None;
            }
        };
        Some(Box::new(Rav1dHost {
            parser,
            sink,
            submitter,
            operating_point,
            detail: CString::default(),
            broken: false,
        }))
    }

    /// Waits until every frame that has begun is submitted or has failed. With one frame in parse that
    /// is so when a decode call returns.
    ///
    /// The caller joins the completion thread behind this, so nothing may be submitted after it: when
    /// the parser or the submission thread has ended with a panic, frames stay in the queue for ever
    /// (the one whose parse never ends, and those behind it), and the submission thread is ended here,
    /// with what it is recording. A frame that went to the GPU behind a completion thread that has ended
    /// would be on the queue when the decoder's buffers are released.
    fn drain(&mut self) {
        let Some(frames) = self.sink.host().frames.clone() else {
            return;
        };
        self.parser.wait();
        if self.parser.broken() || frames.broken() {
            self.stop();
        } else {
            frames.drain();
        }
    }

    /// The submission thread ends and is joined: nothing records a frame of the decoder any more.
    fn stop(&mut self) {
        if let Some(frames) = &self.sink.host().frames {
            frames.stop();
        }
        if let Some(thread) = self.submitter.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Rav1dHost {
    fn drop(&mut self) {
        // The submission thread first: the parser's threads end behind it (the fields are dropped in
        // their order), and what they still say about the frames in parse goes nowhere.
        self.stop();
    }
}

/// `av1_drain_gpu_decoder`, its first part for a decoder with more than one frame in parse: the frames
/// that are in parse are parsed, recorded and submitted, so that the drain of the GPU that follows has
/// them all.
///
/// # Safety
/// `dec` is a decoder; the thread the decoder is used from.
pub(crate) unsafe fn drain_frames(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    if let Some(host) = unsafe { (*dec).rav1d.as_mut() } {
        host.drain();
    }
}

/// The release of a decoder with more than one frame in parse, before its completion thread is joined:
/// the frames in parse are waited for as in a drain, and the submission thread ends.
///
/// # Safety
/// `dec` is a decoder that is being released.
pub(crate) unsafe fn stop_frames(dec: *mut Av1Core) {
    // SAFETY: the caller's contract.
    if let Some(host) = unsafe { (*dec).rav1d.as_mut() } {
        host.drain();
        host.stop();
    }
}

/// A frame takes its second slot (`frame::get_sec_data`): with more than one frame in parse it waits
/// until it may (`Frames::gate`). Nothing otherwise.
///
/// # Safety
/// `dec` is the decoder and `td` the slot of a frame of it that is set up or in parse.
pub(crate) unsafe fn gate_second_slot(dec: *mut Av1Core, td: *mut av1_frame_thread_data) {
    // SAFETY: the caller's contract; the queue lives as long as the parser's threads and the setup of
    // a frame do.
    if let Some(frames) = unsafe { (*dec).parse_frames.as_ref() } {
        frames.gate_second(td);
    }
}

/// `av1_decoder_configuration` (`dx/av1_core.h`): what the decoder was created with.
///
/// # Safety
/// `dec` is a decoder; the four out pointers are null or valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_decoder_configuration(
    dec: *mut Av1Core,
    frames_in_parse: *mut c_int,
    threads: *mut c_int,
    frame_slots: *mut c_int,
    device_size: *mut usize,
) {
    // SAFETY: the caller's contract.
    unsafe {
        if let Some(out) = frames_in_parse.as_mut() {
            *out = (*dec).frames_in_parse.max(1);
        }
        if let Some(out) = threads.as_mut() {
            *out = (*dec).parser_threads.max(1);
        }
        if let Some(out) = frame_slots.as_mut() {
            *out = (*dec).frame_slots;
        }
        if let Some(out) = device_size.as_mut() {
            *out = (*dec).device_bytes;
        }
    }
}

/// `rav1g_rav1d_drain` (`dx/av1_core.h`): behind the drain of a decoder, the error of a frame that was
/// still in parse when the last decode call returned, one a call.
///
/// # Safety
/// `dec` is a decoder; `detail` is valid for writing; the thread the decoder is used from.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_rav1d_drain(dec: *mut Av1Core, detail: *mut *const c_char) -> sys::aom_codec_err_t {
    // SAFETY: the caller's contract.
    unsafe {
        let Some(host) = (*dec).rav1d.as_mut() else {
            return sys::AOM_CODEC_OK;
        };
        let failure = host.sink.host().frames.as_ref().and_then(|frames| frames.take_failure());
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

/// The decode call of a decoder whose parser is rav1d's (`decoder_decode` of `av1/av1_dx_iface.c`): the
/// frames of a temporal unit are parsed, recorded and submitted. Returns the error code of the interface
/// and, in `detail`, a text for it that lives until the next call.
///
/// With more than one frame in parse the call returns when the parse of the unit's frames has begun,
/// and an error it returns may be that of a frame of an earlier call (`aom_codec_dec_cfg_t::frames_in_parse`).
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
            let options = ParseOptions::from_bits((*dec).parser_options as u32);
            (*dec).rav1d = Rav1dHost::new(
                dec,
                (*dec).parser_threads,
                (*dec).frames_in_parse,
                (*dec).frame_slots,
                options,
                operating_point,
            );
        }
        let Some(host) = (*dec).rav1d.as_mut() else {
            *detail = c"The parser could not be created".as_ptr();
            return sys::AOM_CODEC_MEM_ERROR;
        };
        if host.broken {
            *detail = c"The parser stopped with a panic in an earlier call".as_ptr();
            return sys::AOM_CODEC_ERROR;
        }
        let frames = host.sink.host().frames.clone();
        {
            let state = host.sink.host().state();
            state.unit = data;
            state.unit_len = size;
            state.failure = None;
            state.pbi.user_priv = user_priv;
            state.pbi.output_all_layers = output_all_layers;
            // The operating point: a change since the last call goes to the parser, which takes it at
            // the next sequence header, where the host takes it too (`sequence`).
            if operating_point != host.operating_point {
                host.operating_point = operating_point;
                state.pbi.operating_point = operating_point;
                host.parser
                    .set_operating_point(u8::try_from(operating_point).ok().filter(|&point| point < 32).unwrap_or(0));
            }
        }
        let unit = std::slice::from_raw_parts(data, size);
        let parser = &host.parser;
        let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.send(unit)));
        let state = host.sink.host().state();
        state.unit = std::ptr::null();
        state.unit_len = 0;
        // A panic on the thread of this call, on a thread of the parser (the call then came back with
        // an error, or the frame it waited for never ended), or on the submission thread.
        let panicked = parser.broken() || frames.as_ref().is_some_and(|frames| frames.broken());
        let sent = if panicked { Err(Box::new(()) as Box<dyn std::any::Any + Send>) } else { sent };
        let failure = match sent {
            Err(_) => {
                host.broken = true;
                log(true, "PARSER the parser of the decoder stopped with a panic; the decoder decodes nothing more");
                Some(Failure { code: sys::AOM_CODEC_ERROR, detail: String::from("The parser stopped with a panic") })
            }
            Ok(result) => {
                let own = state.failure.take().or_else(|| {
                    result.err().map(|error| Failure {
                        code: sys::AOM_CODEC_CORRUPT_FRAME,
                        detail: format!(
                            "The temporal unit could not be parsed ({error:?}): a header that cannot be read, or a \
                             frame whose references are missing"
                        ),
                    })
                });
                // With more than one frame in parse, a call without an error of its own returns the
                // oldest error of a frame of an earlier call, one a call. Its own comes first: it is
                // the one the caller acts on now (a sequence that needs another decoder).
                match (&frames, own) {
                    (Some(frames), None) => frames.take_failure(),
                    (_, own) => own,
                }
            }
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

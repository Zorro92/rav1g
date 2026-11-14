// SPDX-License-Identifier: BSD-2-Clause

//! The host of the entropy decoder of libaom in plain memory: the functions of `libav1/dx/av1_core.h`,
//! which the D3D12 host of the `rav1g` crate implements for a decoder with a GPU. Here a frame's buffers
//! are an [`Arena`](crate::plain::Arena), nothing is recorded or output, and what the entropy decoder
//! asks for of its own (frame buffers with their motion vectors and segment maps, the context arrays) is
//! allocated when it asks.
//!
//! The decoder is created with one thread, so every function here is called on the thread that sends
//! the data.

use std::ffi::{c_char, c_int, c_void};

use crate::host::types::Av1Core;
use crate::plain::{Buffer, FrameSize, LrUnit, Palette, Side, TileBounds};
use crate::sys;
use crate::sys::parser::{
    AV1_COMMON, AV1Decoder, Av1ColorMapParam, BufferPool, HwFrameBuffer, INVALID_IDX, MB_MODE_INFO, MV_REF,
    REF_NO_SCALE, RESTORE_NONE, RESTORE_SGRPROJ, RESTORE_WIENER, RestorationUnitInfo, TPL_MV_REF, ThreadData, TileInfo,
    YV12_BUFFER_CONFIG, aom_image_t, scale_factors,
};

/// `MAX_MB_PLANE` of `av1/common/blockd.h`.
const MAX_MB_PLANE: c_int = 3;
/// `REF_INVALID_SCALE` of `av1/common/scale.h`.
const REF_INVALID_SCALE: c_int = -1;
/// `TRANSLATION` of `av1/common/mv.h`: the last global motion type that is not a warp.
const TRANSLATION: c_int = 1;
/// `YV12_FLAG_HIGHBITDEPTH` of `aom_scale/yv12config.h`.
const YV12_FLAG_HIGHBITDEPTH: c_int = 8;
/// The memory the C interface keeps its own state in (`aom_codec_dec_cfg_t::host_memory`).
const HOST_MEMORY: usize = 16 << 20;

/// A frame buffer of the entropy decoder: its description, the motion vectors and the segment map of
/// the frame in it.
struct FrameBuffer {
    hw: Box<HwFrameBuffer>,
    mvs: Buffer<MV_REF>,
    seg: Buffer<u8>,
    free: bool,
}

/// What the host keeps for the entropy decoder.
#[derive(Default)]
pub struct CState {
    pbi: Option<Buffer<AV1Decoder>>,
    buffer_pool: Option<Buffer<BufferPool>>,
    frame_buffers: Vec<FrameBuffer>,
    /// The context arrays above a tile row: per plane and kind an array of pointers, one per tile row.
    above_rows: Vec<Buffer<*mut u8>>,
    above: Vec<Buffer<u8>>,
    above_cols: usize,
    above_tile_rows: usize,
    restoration: Vec<Buffer<RestorationUnitInfo>>,
    tpl_mvs: Option<Buffer<TPL_MV_REF>>,
    /// The tile in parse.
    tile: usize,
}

/// The side of a decoder.
///
/// # Safety
/// `dec` is the decoder of [`av1_create_gpu_decoder`]; one thread uses it.
unsafe fn side_of<'a>(dec: *mut Av1Core) -> &'a mut Side {
    // SAFETY: the caller's contract: the decoder is the `core` of a boxed side.
    unsafe { &mut *(*dec).side }
}

/// # Safety
/// `pbi` is the entropy decoder's state of a decoder of this host.
unsafe fn side_of_pbi<'a>(pbi: *mut AV1Decoder) -> &'a mut Side {
    // SAFETY: the caller's contract.
    unsafe { side_of((*pbi).gpu_decoder.cast()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_query_memory_requirements(cfg: *mut sys::aom_codec_dec_cfg_t) -> c_int {
    // SAFETY: the configuration of the caller.
    unsafe { (*cfg).host_size = HOST_MEMORY };
    0
}

/// The decoder is the side whose address the configuration carries in `dx12device`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_create_gpu_decoder(dec: *mut *mut Av1Core, cfg: *mut sys::aom_codec_dec_cfg_t) -> c_int {
    // SAFETY: the configuration of `CParser::new`.
    unsafe {
        let side = (*cfg).dx12device.cast::<Side>();
        *dec = &raw mut (*side).core;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_allocate_pbi(dec: *mut Av1Core, ppbi: *mut *mut AV1Decoder, pbp: *mut *mut BufferPool) {
    // SAFETY: the decoder of this host; the out pointers of the caller.
    unsafe {
        let side = side_of(dec);
        let pbi = side.c.pbi.insert(Buffer::new(1)).ptr();
        (*pbi).gpu_decoder = dec.cast();
        *ppbi = pbi;
        *pbp = side.c.buffer_pool.insert(Buffer::new(1)).ptr();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn av1_destroy_gpu_decoder(_dec: *mut Av1Core) {}

#[unsafe(no_mangle)]
pub extern "C" fn av1_drain_gpu_decoder(_dec: *mut Av1Core) {}

/// What a decoder was created with: nothing is asked of this program's.
#[unsafe(no_mangle)]
pub extern "C" fn av1_decoder_configuration(
    _dec: *mut Av1Core,
    _frames_in_parse: *mut c_int,
    _threads: *mut c_int,
    _frame_slots: *mut c_int,
    _device_size: *mut usize,
) {
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_sequence_fits_decoder(
    _dec: *mut Av1Core,
    max_frame_width: c_int,
    max_frame_height: c_int,
    bit_depth: c_int,
    dec_width: *mut c_int,
    dec_height: *mut c_int,
    dec_bitdepth: *mut c_int,
) -> c_int {
    // SAFETY: the out pointers are null or valid.
    unsafe {
        if !dec_width.is_null() {
            *dec_width = max_frame_width;
        }
        if !dec_height.is_null() {
            *dec_height = max_frame_height;
        }
        if !dec_bitdepth.is_null() {
            *dec_bitdepth = bit_depth;
        }
    }
    1
}

/// The frame buffer of the frame whose header is being read: a free one, with room for the motion
/// vectors and the segment map of a frame of this size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_reallocate_frame_buffer(
    private: *mut c_void,
    ybf: *mut YV12_BUFFER_CONFIG,
    width: c_int,
    height: c_int,
    upscaled_width: c_int,
    hbd: c_int,
) -> c_int {
    if ybf.is_null() || private.is_null() {
        return -1;
    }
    // SAFETY: `private` is the decoder (`cb_priv` of the buffer pool), `ybf` the frame's description.
    unsafe {
        let side = side_of(private.cast());
        let ybf = &mut *ybf;
        ybf.hw_show_image = std::ptr::null_mut();
        let buffers = &mut side.c.frame_buffers;
        let index = if ybf.hw_buffer.is_null() {
            let index = buffers.iter().position(|buffer| buffer.free).unwrap_or_else(|| {
                // SAFETY: a description of numbers and pointers.
                let hw = Box::new(std::mem::zeroed::<HwFrameBuffer>());
                buffers.push(FrameBuffer { hw, mvs: Buffer::new(0), seg: Buffer::new(0), free: true });
                buffers.len() - 1
            });
            buffers[index].free = false;
            index
        } else {
            buffers
                .iter()
                .position(|buffer| std::ptr::eq(&raw const *buffer.hw, ybf.hw_buffer))
                .expect("a buffer of the pool")
        };
        let buffer = &mut buffers[index];
        let aligned_width = (width.max(upscaled_width) + 127) & !127;
        let aligned_height = (height + 127) & !127;
        let mvs = (((aligned_width >> 3) + 1) * ((aligned_height >> 3) + 1)) as usize;
        let seg = ((aligned_width >> 2) * (aligned_height >> 2)) as usize;
        if buffer.mvs.len() < mvs {
            buffer.mvs = Buffer::new(mvs);
        }
        if buffer.seg.len() < seg {
            buffer.seg = Buffer::new(seg);
        }
        buffer.hw.mvs_alloc = buffer.mvs.ptr().cast();
        buffer.hw.seg_alloc = buffer.seg.ptr();
        ybf.hw_buffer = &raw mut *buffer.hw;

        // The description of the frame, as the D3D12 host writes it.
        let h_border = 16;
        let y_stride = aligned_width + 2 * h_border;
        let uv_stride = (aligned_width >> 1) + 2 * h_border;
        let fb_size = (aligned_height * y_stride + 2 * (aligned_height >> 1) * uv_stride) * (1 + hbd);
        ybf.buffer_alloc = std::ptr::null_mut();
        ybf.buffer_alloc_sz = 0;
        ybf.__bindgen_anon_3.crop_widths[0] = width;
        ybf.__bindgen_anon_4.crop_heights[0] = height;
        let y_width = (width + 7) & !7;
        let y_height = (height + 7) & !7;
        ybf.__bindgen_anon_1.widths[0] = y_width;
        ybf.__bindgen_anon_2.heights[0] = y_height;
        ybf.__bindgen_anon_5.strides[0] = y_stride;
        ybf.__bindgen_anon_3.crop_widths[1] = (width + 1) >> 1;
        ybf.__bindgen_anon_4.crop_heights[1] = (height + 1) >> 1;
        ybf.__bindgen_anon_1.widths[1] = y_width >> 1;
        ybf.__bindgen_anon_2.heights[1] = y_height >> 1;
        ybf.__bindgen_anon_5.strides[1] = uv_stride;
        ybf.border = h_border;
        ybf.frame_size = fb_size as usize;
        ybf.subsampling_x = 1;
        ybf.subsampling_y = 1;
        ybf.use_external_reference_buffers = 0;
        ybf.flags = if hbd != 0 { YV12_FLAG_HIGHBITDEPTH } else { 0 };
        ybf.__bindgen_anon_6.buffers = [std::ptr::null_mut(); 3];
        let hw = &mut *buffer.hw;
        hw.width = y_width;
        hw.height = y_height;
        hw.hbd = hbd;
        hw.y_crop_width = width;
        hw.y_crop_height = height;
        hw.uv_crop_width = (width + 1) >> 1;
        hw.uv_crop_height = (height + 1) >> 1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_release_fb_callback(private: *mut c_void, ybf: *mut YV12_BUFFER_CONFIG) {
    // SAFETY: as for `av1_reallocate_frame_buffer`.
    unsafe {
        let side = side_of(private.cast());
        let ybf = &mut *ybf;
        if !ybf.hw_buffer.is_null() {
            if let Some(buffer) =
                side.c.frame_buffers.iter_mut().find(|buffer| std::ptr::eq(&raw const *buffer.hw, ybf.hw_buffer))
            {
                buffer.free = true;
            }
            ybf.hw_buffer = std::ptr::null_mut();
        }
        ybf.hw_show_image = std::ptr::null_mut();
    }
}

/// The context arrays of the entropy decoder, for the frame whose header was read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_setup_context_buffers(pbi: *mut AV1Decoder) {
    // SAFETY: the entropy decoder's state of a decoder of this host.
    unsafe {
        let side = side_of_pbi(pbi);
        let c = &mut side.c;
        let cm = &mut (*pbi).common;
        let cols = ((cm.mi_cols + 31) & !31) as usize;
        let rows = cm.tile_rows.max(1) as usize;
        if c.above_cols < cols || c.above_tile_rows < rows {
            c.above.clear();
            c.above_rows.clear();
            for _ in 0..5 {
                let pointers = Buffer::<*mut u8>::new(rows);
                for row in 0..rows {
                    let context = Buffer::<u8>::new(cols + 64);
                    pointers.ptr().add(row).write(context.ptr());
                    c.above.push(context);
                }
                c.above_rows.push(pointers);
            }
            c.above_cols = cols;
            c.above_tile_rows = rows;
        }
        cm.num_allocated_above_contexts = rows as c_int;
        cm.num_allocated_above_context_mi_col = cols as c_int;
        cm.num_allocated_above_context_planes = if cm.seq_params.monochrome != 0 { 1 } else { MAX_MB_PLANE };
        cm.above_context[0] = c.above_rows[0].ptr().cast();
        cm.above_context[1] = c.above_rows[1].ptr().cast();
        cm.above_context[2] = c.above_rows[2].ptr().cast();
        cm.above_seg_context = c.above_rows[3].ptr().cast();
        cm.above_txfm_context = c.above_rows[4].ptr().cast();

        // A loop restoration unit is at least 64 samples wide and high, in the upscaled frame.
        let width = cm.superres_upscaled_width.max(cm.mi_cols * 4);
        let units = (((width + 63) >> 6) + 2) as usize * (((cm.mi_rows * 4 + 63) >> 6) + 2) as usize;
        if c.restoration.first().is_none_or(|buffer| buffer.len() < units) {
            c.restoration = (0..3).map(|_| Buffer::new(units)).collect();
        }
        for (info, buffer) in cm.rst_info.iter_mut().zip(&c.restoration) {
            info.unit_info = buffer.ptr();
        }
        let tpl = (((cm.mi_rows + 32) >> 1) + 1) as usize * (cm.mi_stride >> 1) as usize + 64;
        if c.tpl_mvs.as_ref().is_none_or(|buffer| buffer.len() < tpl) {
            c.tpl_mvs = Some(Buffer::new(tpl));
        }
        cm.tpl_mvs = c.tpl_mvs.as_ref().expect("allocated above").ptr();
    }
}

/// `av1_get_ref_scale_factors` of the host: the scale of a reference of the frame.
fn ref_scale_factors(cm: &AV1_COMMON, ref_frame: usize) -> Option<&scale_factors> {
    let map_idx = cm.remapped_ref_idx[ref_frame - 1];
    (map_idx != INVALID_IDX).then(|| &cm.ref_scale_factors[map_idx as usize])
}

/// `av1_is_scaled` of `av1/common/scale.h`.
fn is_scaled(sf: &scale_factors) -> bool {
    sf.x_scale_fp != REF_INVALID_SCALE
        && sf.y_scale_fp != REF_INVALID_SCALE
        && (sf.x_scale_fp != REF_NO_SCALE as c_int || sf.y_scale_fp != REF_NO_SCALE as c_int)
}

/// The size of the frame in parse.
fn frame_size(cm: &AV1_COMMON) -> FrameSize {
    FrameSize {
        mi_cols: cm.mi_cols,
        mi_rows: cm.mi_rows,
        tile_cols: cm.tile_cols,
        tile_rows: cm.tile_rows,
        has_chroma: cm.seq_params.monochrome == 0,
    }
}

/// The tiles of a frame are about to be parsed: the frame's buffers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_setup_frame(pbi: *mut AV1Decoder, cm: *mut AV1_COMMON) {
    // SAFETY: the entropy decoder's state of a decoder of this host, with a frame's header read.
    unsafe {
        let side = side_of_pbi(pbi);
        let cm = &mut *cm;
        (*cm.cur_frame).buf.hw_show_image = std::ptr::null_mut();
        // A block with global motion is warped from a reference whose model is a warp and that is not
        // scaled (`rav1g_setup_frame_end` of the D3D12 host).
        let mut gm_warp = [0; 8];
        for (reference, warp) in gm_warp.iter_mut().enumerate().skip(1) {
            let scaled = ref_scale_factors(cm, reference).is_some_and(is_scaled);
            let gm = &cm.global_motion[reference];
            *warp = c_int::from(c_int::from(gm.wmtype) > TRANSLATION && gm.invalid == 0 && !scaled);
        }
        let size = frame_size(cm);
        let arena = side.begin_frame(size, gm_warp);
        cm.mi_grid_base = arena.mi_grid.ptr();
        cm.mi_grid_visible = cm.mi_grid_base;
        cm.mi_alloc_size = cm.mi_stride * ((cm.mi_rows + 31) & !31);
        assert_eq!(cm.mi_stride, size.mi_stride(), "the distance of the grid's rows");
        // The loop restoration units of the frame before are not this frame's.
        for buffer in &side.c.restoration {
            buffer.ptr().write_bytes(0, buffer.len());
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_setup_macroblockd(
    pbi: *mut AV1Decoder,
    cm: *mut AV1_COMMON,
    thread_data: *mut ThreadData,
    tile: *mut TileInfo,
) {
    // SAFETY: the entropy decoder's state while it starts the tile of `thread_data`.
    unsafe {
        let side = side_of_pbi(pbi);
        let tile = &*tile;
        let tile_id = tile.tile_col + tile.tile_row * (*cm).tile_cols;
        side.c.tile = tile_id as usize;
        let arena = side.arena.as_ref().expect("a frame is in parse");
        let (tile_data, mi_pool, mi_count_max) = arena.setup_tile(&TileBounds {
            index: tile_id as usize,
            row_start: tile.mi_row_start,
            row_end: tile.mi_row_end,
            col_start: tile.mi_col_start,
            col_end: tile.mi_col_end,
        });
        let thread_data = &mut *thread_data;
        thread_data.tile_data = tile_data;
        thread_data.xd.tile_data = tile_data;
        thread_data.tile_id = tile_id;
        thread_data.mi_count = 0;
        thread_data.mi_count_max = mi_count_max;
        thread_data.mi_pool = mi_pool;
        thread_data.mi_count2 = 0;
        thread_data.mi_pool2 = std::ptr::null_mut();
        thread_data.tile_data2 = std::ptr::null_mut();
        // The coefficients are laid out for the worst case: the tile never goes on in a second share.
        thread_data.ext_idct_buffer = 1;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn av1_setup_sec_data(_pbi: *mut AV1Decoder, _cm: *mut AV1_COMMON, _thread_data: *mut ThreadData) {
    eprintln!("rav1g-parse-check: a tile has more blocks than 4x4 units");
    std::process::abort();
}

#[unsafe(no_mangle)]
pub extern "C" fn av1_setup_ext_coef_buffer(
    _pbi: *mut AV1Decoder,
    _cm: *mut AV1_COMMON,
    _thread_data: *mut ThreadData,
) {
    eprintln!("rav1g-parse-check: a tile asks for a second share of the coefficient buffer");
    std::process::abort();
}

/// A palette block's plane: the colours and the colour index of every sample of the block.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_intra_palette(
    pbi: *mut AV1Decoder,
    mi: *mut MB_MODE_INFO,
    params: *mut Av1ColorMapParam,
    plane: c_int,
) {
    // SAFETY: the entropy decoder's state while it reads a block; the block and its colour map.
    unsafe {
        let side = side_of_pbi(pbi);
        let mi = &*mi;
        let params = &*params;
        let plane = plane as usize;
        let count = mi.palette_mode_info.palette_size[plane] as usize;
        let colors = &mi.palette_mode_info.palette_colors;
        let colors = if plane == 0 {
            colors[..count].to_vec()
        } else {
            [&colors[8..8 + count], &colors[16..16 + count]].concat()
        };
        let (width, height) = (params.plane_width as usize, params.plane_height as usize);
        let map = std::slice::from_raw_parts(params.color_map, width * height).to_vec();
        let arena = side.arena.as_ref().expect("a frame is in parse");
        arena.palettes[side.c.tile].lock().expect("no thread panics with it").push(Palette {
            mi_row: mi.mi_row,
            mi_col: mi.mi_col,
            plane: plane as u8,
            colors,
            width,
            height,
            map,
        });
    }
}

/// The frame is parsed: the D3D12 host records its stages here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_decode_frame_gpu(pbi: *mut AV1Decoder) -> c_int {
    // SAFETY: the entropy decoder's state with a frame whose tiles are parsed.
    unsafe {
        let side = side_of_pbi(pbi);
        if let Some(arena) = &side.arena {
            let cm = &(*pbi).common;
            let mut units = arena.lr_units.lock().expect("no thread panics with it");
            let planes = if arena.size.has_chroma { 3 } else { 1 };
            for (plane, info) in cm.rst_info.iter().enumerate().take(planes) {
                if c_int::from(info.frame_restoration_type) == RESTORE_NONE as c_int {
                    continue;
                }
                for index in 0..info.units_per_tile {
                    let unit = &*info.unit_info.add(index as usize);
                    let kind = c_int::from(unit.restoration_type);
                    let values = if kind == RESTORE_WIENER as c_int {
                        let (v, h) = (&unit.wiener_info.vfilter, &unit.wiener_info.hfilter);
                        [v[0], v[1], v[2], h[0], h[1], h[2]].map(i32::from)
                    } else if kind == RESTORE_SGRPROJ as c_int {
                        [unit.sgrproj_info.ep, unit.sgrproj_info.xqd[0], unit.sgrproj_info.xqd[1], 0, 0, 0]
                    } else {
                        [0; 6]
                    };
                    units.push(LrUnit {
                        plane: plane as u8,
                        unit_x: (index % info.horz_units_per_tile) as u16,
                        unit_y: (index / info.horz_units_per_tile) as u16,
                        r#type: kind as u8,
                        values,
                    });
                }
            }
        }
        side.end_frame(true);
        // A frame with super-resolution is the upscaled frame from here on: the D3D12 host gives its
        // buffer the upscaled width when it records the upscale (`postfilter.rs`), as libaom's
        // `av1_superres_upscale` does, and the entropy decoder takes the size of a reference from there
        // (the scale of a reference, a frame size coded as that of a reference).
        let cm = &mut (*pbi).common;
        if cm.width != cm.superres_upscaled_width {
            let upscaled = cm.superres_upscaled_width;
            let buf = &mut (*cm.cur_frame).buf;
            buf.__bindgen_anon_3.crop_widths[0] = upscaled;
            buf.__bindgen_anon_3.crop_widths[1] = (upscaled + 1) >> 1;
            buf.__bindgen_anon_1.widths[0] = (upscaled + 7) & !7;
            buf.__bindgen_anon_1.widths[1] = ((upscaled + 7) & !7) >> 1;
            if let Some(hw) = buf.hw_buffer.as_mut() {
                hw.y_crop_width = upscaled;
                hw.uv_crop_width = (upscaled + 1) >> 1;
                hw.width = (upscaled + 7) & !7;
            }
        }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn av1_decode_sef(_pbi: *mut AV1Decoder) {}

#[unsafe(no_mangle)]
pub extern "C" fn av1_show_frame(_pbi: *mut AV1Decoder, _buf: *mut YV12_BUFFER_CONFIG, _is_visible: c_int) {}

/// No frame is output.
#[unsafe(no_mangle)]
pub extern "C" fn get_output_frame(_pbi: *mut AV1Decoder, _dst: *mut aom_image_t) -> c_int {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn av1_perf_ticks() -> i64 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn av1_perf_cpu_add(_pbi: *mut AV1Decoder, _id: c_int, _start_ticks: i64) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_log_error(line: *const c_char) {
    if !line.is_null() {
        // SAFETY: a C string of the caller.
        eprintln!("libaom: {}", unsafe { std::ffi::CStr::from_ptr(line) }.to_string_lossy());
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn av1_last_create_error() -> *const c_char {
    c"the plain host does not fail".as_ptr()
}

/// The entropy decoder of libaom with the plain host.
pub struct CParser {
    side: Box<Side>,
    ctx: Box<sys::aom_codec_ctx_t>,
    _host_memory: Buffer<u8>,
}

impl CParser {
    /// `wave_edges`: the frames keep the wavefront in the edges of their tiles (the host's default), not
    /// in the grids.
    pub fn new(wave_edges: bool) -> Result<Self, String> {
        let mut side = Side::new(wave_edges);
        let host_memory = Buffer::<u8>::new(HOST_MEMORY);
        // SAFETY: the configuration and the context are C structs for which zero bytes are a value; the
        // host memory and the side live as long as the decoder.
        unsafe {
            let mut cfg: sys::aom_codec_dec_cfg_t = std::mem::zeroed();
            cfg.threads = 1;
            cfg.host_memory = host_memory.ptr().cast();
            cfg.host_size = HOST_MEMORY;
            cfg.dx12device = (&raw mut *side).cast();
            let mut ctx: Box<sys::aom_codec_ctx_t> = Box::new(std::mem::zeroed());
            let code = sys::rav1g_codec_dec_init(&raw mut *ctx, &raw const cfg, 0);
            if code != sys::AOM_CODEC_OK {
                return Err(format!("the entropy decoder of libaom was not created: error {code}"));
            }
            Ok(Self { side, ctx, _host_memory: host_memory })
        }
    }

    /// Parses the frames of a temporal unit.
    pub fn send(&mut self, data: &[u8]) -> Result<(), String> {
        // SAFETY: the decoder of `new`; the data is valid for the call.
        unsafe {
            let code = sys::aom_codec_decode(&raw mut *self.ctx, data.as_ptr(), data.len(), std::ptr::null_mut());
            // A frame that failed is not a parsed frame.
            self.side.arena = None;
            if code != sys::AOM_CODEC_OK {
                let detail = sys::aom_codec_error_detail(&raw mut *self.ctx);
                let detail = if detail.is_null() {
                    String::new()
                } else {
                    format!(": {}", std::ffi::CStr::from_ptr(detail).to_string_lossy())
                };
                return Err(format!("error {code}{detail}"));
            }
        }
        Ok(())
    }

    /// The frames parsed since the last call.
    pub fn take_frames(&mut self) -> Vec<crate::plain::Arena> {
        std::mem::take(&mut self.side.frames)
    }
}

impl Drop for CParser {
    fn drop(&mut self) {
        // SAFETY: the decoder of `new`.
        unsafe { sys::aom_codec_destroy(&raw mut *self.ctx) };
    }
}

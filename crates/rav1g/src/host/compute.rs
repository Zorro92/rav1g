// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The shader library: the pipeline layouts and the compute pipelines of the decoder on one device, and
//! the constant tables the shaders read (film grain, warp filter, wedge and inter-intra masks). From
//! `libav1/dx/av1_compute.cpp`.
//!
//! The library is the C++ struct `compute_shader_lib`, whose pipeline states and root signatures the
//! frame pipeline reads; it is created here, zeroed as its C++ constructor leaves it, inside a Rust
//! value that also holds the threads that create the pipelines. The bytecode of the shaders is in the FXC
//! headers, which the glue (`crates/rav1g-sys/glue/av1_compute.cpp`) includes and hands over as a table.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::offset_of;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{E_FAIL, E_POINTER};
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
use windows::core::Interface;

use super::log::log;
use super::tables::{BLOCK_SIZE_HIGH, BLOCK_SIZE_WIDE, GAUSSIAN_SEQUENCE, SS_SIZE_420};
use super::types::{Av1Core, ComputeShader, GpuBufferObject, compute_shader_lib};
use crate::gpu::d3d12::{com_ref, com_release, com_store};
use crate::gpu::{self, BufferRef, Device, Layout, LayoutDesc, OwnedDevice, Queue, Upload};
use crate::sys::parser::{
    BLOCK_SIZES_ALL, TX_4X4, TX_4X8, TX_4X16, TX_8X4, TX_8X8, TX_8X16, TX_8X32, TX_16X4, TX_16X8, TX_16X16, TX_16X32,
    TX_16X64, TX_32X8, TX_32X16, TX_32X32, TX_32X64, TX_64X16, TX_64X32, TX_64X64, TX_SIZES_ALL, ii_size_scales,
    ii_weights1d, warped_filter, wedge_params_lookup,
};

/// Threads that create pipelines at most (`MAX_CREATE_SHADER_THREADS` of the C++).
const MAX_CREATE_THREADS: c_int = 6;

unsafe extern "C" {
    /// `av1/common/reconinter.c`: builds the wedge masks `wedge_params_lookup` points to.
    fn av1_init_wedge_masks();
    /// `crates/rav1g-sys/shader_table.c`: the bytecode of the shaders, from the headers of the FXC build.
    fn rav1g_shader_table(count: *mut u32) -> *const ShaderBytecode;
}

#[repr(C)]
struct ShaderBytecode {
    name: *const c_char,
    code: *const u8,
    size: u32,
}

/// The bytecode of a shader by the name of its header.
fn bytecode(name: &str) -> Option<&'static [u8]> {
    let mut count = 0u32;
    // SAFETY: a static table of `count` entries with static names and bytecode.
    unsafe {
        let table = std::slice::from_raw_parts(rav1g_shader_table(&mut count), count as usize);
        table
            .iter()
            .find(|entry| CStr::from_ptr(entry.name).to_bytes() == name.as_bytes())
            .map(|entry| std::slice::from_raw_parts(entry.code, entry.size as usize))
    }
}

/// The library as the pipeline cache handle points to it.
#[repr(C)]
struct ShaderLib {
    /// What the C++ frame pipeline reads. First: the handle is also a `compute_shader_lib *`.
    lib: compute_shader_lib,
    /// The threads that create the pipelines, until they are joined.
    workers: Mutex<Vec<JoinHandle<()>>>,
    /// The codes of the pipelines that could not be created, or'ed.
    errors: Arc<AtomicI32>,
}

// The pipelines and their layouts are the first fields of the C++ struct: `SHADERS` pipelines, then
// `LAYOUTS` root signatures. They are released as two arrays.
const SHADERS: usize = offset_of!(compute_shader_lib, sig_idct) / size_of::<ComputeShader>();
const LAYOUTS: usize =
    (offset_of!(compute_shader_lib, compliled_) - offset_of!(compute_shader_lib, sig_idct)) / size_of::<*mut c_void>();
const _: () = {
    assert!(offset_of!(compute_shader_lib, shader_idct) == 0);
    assert!(SHADERS * size_of::<ComputeShader>() == offset_of!(compute_shader_lib, sig_idct));
    // 20 transforms, 17 shaders that do not depend on the bit depth, 30 that do for 8 bits and for more.
    assert!(SHADERS == 20 + 17 + 2 * 30);
    assert!(LAYOUTS == 16);
    assert!(offset_of!(ShaderLib, lib) == 0);
};

/// One pipeline to create.
struct Task {
    bytecode: &'static [u8],
    layout: Layout,
    dst: *mut ComputeShader,
}
// SAFETY: `dst` is a field of the library that only this task writes, and the library outlives the
// threads (they are joined before it is released).
unsafe impl Send for Task {}
// SAFETY: the same; a task is taken by one thread.
unsafe impl Sync for Task {}

/// `create_shader` of the C++: the pipeline of a task into its field of the library.
fn create_shader(device: Device<'_>, task: &Task) -> Result<(), gpu::Error> {
    // SAFETY: the field belongs to this task (see Task) and its COM pointers are still null.
    unsafe {
        (*task.dst).bytecode = task.bytecode.as_ptr();
        (*task.dst).size = task.bytecode.len();
        com_store(&raw mut (*task.dst).signaturePtr, task.layout.clone().into_raw());
        let pipeline = device.create_pipeline(&task.layout, task.bytecode)?;
        com_store(&raw mut (*task.dst).pso, pipeline.into_raw());
    }
    Ok(())
}

fn log_failed_pipeline(index: usize, task: &Task, error: gpu::Error) {
    log(
        true,
        &format!(
            "MEMORY FAILED compute pipeline state {index} ({} bytes of bytecode): hr 0x{:08X}",
            task.bytecode.len(),
            error.code as u32
        ),
    );
}

/// A thread that creates pipelines: takes the next task until there is none or one has failed.
fn create_shader_thread(device: OwnedDevice, tasks: Arc<Vec<Task>>, next: Arc<AtomicUsize>, errors: Arc<AtomicI32>) {
    // As the threads of the C++ (pthread_create of aom_thread.h).
    // SAFETY: the pseudo handle of this thread.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    loop {
        let index = next.fetch_add(1, Ordering::Relaxed);
        let Some(task) = tasks.get(index) else { break };
        if let Err(error) = create_shader(device.device(), task) {
            log_failed_pipeline(index, task, error);
            errors.fetch_or(error.code, Ordering::Relaxed);
        }
        if errors.load(Ordering::Relaxed) != 0 {
            break;
        }
    }
}

/// The layouts of the library, by the name of their field (`sig_<name>`).
struct Layouts {
    idct: Layout,
    common111: Layout,
    common0102: Layout,
    common0110: Layout,
    copy_plane: Layout,
    pred_blocks: Layout,
    pred_blocks_fast: Layout,
    intra_pred: Layout,
    intra_v2: Layout,
    inter_pred: Layout,
    loop_rest: Layout,
    cdef_filter: Layout,
    film_grain: Layout,
    lf: Layout,
    lf_gen: Layout,
    upscale: Layout,
}

/// Creates the layouts in the order of the C++ and puts each into its field of the library.
///
/// # Safety
/// `lib` is the zeroed library.
unsafe fn create_layouts(device: Device<'_>, lib: *mut compute_shader_lib) -> Result<Layouts, gpu::Error> {
    let layout = |read_buffers, write_buffers, constant_blocks, inline_constants| {
        device.create_layout(LayoutDesc { read_buffers, write_buffers, constant_blocks, inline_constants })
    };
    let layouts = Layouts {
        idct: layout(1, 2, 2, 2)?,
        common111: layout(1, 1, 1, 0)?,
        common0102: layout(0, 1, 0, 2)?,
        common0110: layout(0, 1, 1, 0)?,
        copy_plane: layout(0, 1, 0, 6)?,
        pred_blocks: layout(5, 2, 1, 7)?,
        // gen_pred_blocks_fast.hlsl (-interfast): one more inline constant.
        pred_blocks_fast: layout(5, 2, 1, 8)?,
        intra_pred: layout(3, 1, 1, 10)?,
        // intra_v2.hlsl: t0..t4, u0, b0 and 8 inline constants in b1.
        intra_v2: layout(5, 1, 1, 8)?,
        inter_pred: layout(5, 1, 1, 4)?,
        loop_rest: layout(3, 1, 2, 2)?,
        cdef_filter: layout(2, 1, 1, 0)?,
        film_grain: layout(3, 1, 1, 0)?,
        lf: layout(1, 1, 1, 5)?,
        lf_gen: layout(2, 1, 1, 6)?,
        upscale: layout(0, 1, 1, 3)?,
    };
    // SAFETY: the fields are null COM pointers of root signatures (the caller's contract).
    unsafe {
        com_store(&raw mut (*lib).sig_idct, layouts.idct.clone().into_raw());
        com_store(&raw mut (*lib).sig_common111, layouts.common111.clone().into_raw());
        com_store(&raw mut (*lib).sig_common0102, layouts.common0102.clone().into_raw());
        com_store(&raw mut (*lib).sig_common0110, layouts.common0110.clone().into_raw());
        com_store(&raw mut (*lib).sig_copy_plane, layouts.copy_plane.clone().into_raw());
        com_store(&raw mut (*lib).sig_pred_blocks, layouts.pred_blocks.clone().into_raw());
        com_store(&raw mut (*lib).sig_pred_blocks_fast, layouts.pred_blocks_fast.clone().into_raw());
        com_store(&raw mut (*lib).sig_intra_pred, layouts.intra_pred.clone().into_raw());
        com_store(&raw mut (*lib).sig_intra_v2, layouts.intra_v2.clone().into_raw());
        com_store(&raw mut (*lib).sig_inter_pred, layouts.inter_pred.clone().into_raw());
        com_store(&raw mut (*lib).sig_loop_rest, layouts.loop_rest.clone().into_raw());
        com_store(&raw mut (*lib).sig_cdef_filter, layouts.cdef_filter.clone().into_raw());
        com_store(&raw mut (*lib).sig_film_grain, layouts.film_grain.clone().into_raw());
        com_store(&raw mut (*lib).sig_lf, layouts.lf.clone().into_raw());
        com_store(&raw mut (*lib).sig_lf_gen, layouts.lf_gen.clone().into_raw());
        com_store(&raw mut (*lib).sig_upscale, layouts.upscale.clone().into_raw());
    }
    Ok(layouts)
}

/// The pipelines of the library in the order of the C++ (the number of a pipeline in the log line of a
/// failed creation is its place here): the header of the shader, its layout, its field.
///
/// # Safety
/// `lib` is the library.
unsafe fn create_tasks(lib: *mut compute_shader_lib, layouts: &Layouts) -> Result<Vec<Task>, String> {
    // SAFETY: only addresses of fields of the library are taken (the caller's contract).
    let list: Vec<(&str, &Layout, *mut ComputeShader)> = unsafe {
        let idct = |tx: c_int| &raw mut (*lib).shader_idct[tx as usize];
        vec![
            ("idct4x4", &layouts.idct, idct(TX_4X4)),
            ("idct4x8", &layouts.idct, idct(TX_4X8)),
            ("idct4x16", &layouts.idct, idct(TX_4X16)),
            ("idct8x4", &layouts.idct, idct(TX_8X4)),
            ("idct8x8", &layouts.idct, idct(TX_8X8)),
            ("idct8x16", &layouts.idct, idct(TX_8X16)),
            ("idct8x32", &layouts.idct, idct(TX_8X32)),
            ("idct16x4", &layouts.idct, idct(TX_16X4)),
            ("idct16x8", &layouts.idct, idct(TX_16X8)),
            ("idct16x16", &layouts.idct, idct(TX_16X16)),
            ("idct16x32", &layouts.idct, idct(TX_16X32)),
            ("idct16x64", &layouts.idct, idct(TX_16X64)),
            ("idct32x8", &layouts.idct, idct(TX_32X8)),
            ("idct32x16", &layouts.idct, idct(TX_32X16)),
            ("idct32x32", &layouts.idct, idct(TX_32X32)),
            ("idct32x64", &layouts.idct, idct(TX_32X64)),
            ("idct64x16", &layouts.idct, idct(TX_64X16)),
            ("idct64x32", &layouts.idct, idct(TX_64X32)),
            ("idct64x64", &layouts.idct, idct(TX_64X64)),
            ("idct_lossless", &layouts.idct, idct(TX_SIZES_ALL)),
            ("idct_sort_blocks", &layouts.common111, &raw mut (*lib).shader_idct_sort),
            ("fill_buffer", &layouts.common0102, &raw mut (*lib).shader_fill_buffer),
            ("loop_restoration", &layouts.loop_rest, &raw mut (*lib).shader_loop_rest),
            ("film_grain_filter", &layouts.film_grain, &raw mut (*lib).shader_filmgrain_filter),
            ("film_grain_gen_luma", &layouts.film_grain, &raw mut (*lib).shader_filmgrain_luma_gen),
            ("film_grain_gen_chroma", &layouts.film_grain, &raw mut (*lib).shader_filmgrain_chroma_gen),
            ("cdef_filter", &layouts.cdef_filter, &raw mut (*lib).shader_cdef_filter),
            ("cdef_filter_v2", &layouts.cdef_filter, &raw mut (*lib).shader_cdef_filter_v2),
            ("intra_v2", &layouts.intra_v2, &raw mut (*lib).shader_intra_v2),
            ("intra_v2_hbd", &layouts.intra_v2, &raw mut (*lib).shader_intra_v2_hbd),
            ("gen_pred_blocks", &layouts.pred_blocks, &raw mut (*lib).shader_gen_pred_blocks),
            ("gen_pred_blocks_fast", &layouts.pred_blocks_fast, &raw mut (*lib).shader_gen_pred_blocks_fast),
            ("inter_fast", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_fast),
            ("inter_compound_fast", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_fast),
            ("inter_fast_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_fast),
            ("inter_compound_fast_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_fast),
            ("gen_lf_blocks_vert", &layouts.lf_gen, &raw mut (*lib).shader_gen_lf_vert),
            ("gen_lf_blocks_hor", &layouts.lf_gen, &raw mut (*lib).shader_gen_lf_hor),
            ("copy_plane", &layouts.copy_plane, &raw mut (*lib).shader_copy_plane),
            ("copy_plane_10bit10x3", &layouts.copy_plane, &raw mut (*lib).shader_copy_plane_10bit10x3),
            ("upscaling", &layouts.upscale, &raw mut (*lib).shader_upscale),
            ("inter_2x2chroma", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_2x2),
            ("inter_compound", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp),
            ("inter_compound_2x2", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_2x2),
            ("inter_compound_diff_chroma", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_diff_uv),
            ("inter_compound_diff_luma", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_diff_y),
            ("inter_compound_masked", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_masked),
            ("inter_main", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_base),
            ("inter_obmc_above", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_obmc_above),
            ("inter_obmc_left", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_obmc_left),
            ("inter_warp", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_warp),
            ("inter_scale_warp_compound", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_warp_comp),
            ("intra_filter", &layouts.intra_pred, &raw mut (*lib).shaders_8bit.intra_filter),
            ("intra_main", &layouts.intra_pred, &raw mut (*lib).shaders_8bit.intra_main),
            ("intra_main_opt", &layouts.intra_pred, &raw mut (*lib).shaders_8bit.intra_main_opt),
            ("inter_ext_borders", &layouts.common0110, &raw mut (*lib).shaders_8bit.inter_ext_borders),
            ("reconstruct_block", &layouts.intra_pred, &raw mut (*lib).shaders_8bit.reconstruct_block),
            ("loopfilter_vert", &layouts.lf, &raw mut (*lib).shaders_8bit.loopfilter_v),
            ("loopfilter_hor", &layouts.lf, &raw mut (*lib).shaders_8bit.loopfilter_h),
            ("inter_compound_2x2_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_2x2),
            ("inter_2x2chroma_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_2x2),
            ("inter_compound_diff_chroma_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_diff_uv),
            ("inter_compound_diff_luma_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_diff_y),
            ("inter_compound_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp),
            ("inter_compound_masked_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_masked),
            ("inter_main_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_base),
            ("inter_obmc_above_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_obmc_above),
            ("inter_obmc_left_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_obmc_left),
            ("inter_scale_warp_compound_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_warp_comp),
            ("inter_warp_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_warp),
            ("intra_filter_hbd", &layouts.intra_pred, &raw mut (*lib).shaders_hbd.intra_filter),
            ("intra_main_hbd", &layouts.intra_pred, &raw mut (*lib).shaders_hbd.intra_main),
            ("intra_main_hbd_opt", &layouts.intra_pred, &raw mut (*lib).shaders_hbd.intra_main_opt),
            ("inter_ext_borders_hbd", &layouts.common0110, &raw mut (*lib).shaders_hbd.inter_ext_borders),
            ("reconstruct_block_hbd", &layouts.intra_pred, &raw mut (*lib).shaders_hbd.reconstruct_block),
            ("loopfilter_vert_hbd", &layouts.lf, &raw mut (*lib).shaders_hbd.loopfilter_v),
            ("loopfilter_hor_hbd", &layouts.lf, &raw mut (*lib).shaders_hbd.loopfilter_h),
            ("inter_scale", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_scale),
            ("inter_scale_2x2chroma", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_scale_2x2),
            ("inter_scale_compound", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_scale_comp),
            (
                "inter_scale_compound_diff_chroma",
                &layouts.inter_pred,
                &raw mut (*lib).shaders_8bit.inter_scale_comp_diff_uv,
            ),
            (
                "inter_scale_compound_diff_luma",
                &layouts.inter_pred,
                &raw mut (*lib).shaders_8bit.inter_scale_comp_diff_y,
            ),
            ("inter_scale_compound_masked", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_scale_comp_masked),
            ("inter_scale_obmc_above", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_scale_obmc_above),
            ("inter_scale_obmc_left", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_scale_obmc_left),
            ("inter_scale_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_scale),
            ("inter_scale_2x2chroma_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_scale_2x2),
            ("inter_scale_compound_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_scale_comp),
            (
                "inter_scale_compound_diff_chroma_hbd",
                &layouts.inter_pred,
                &raw mut (*lib).shaders_hbd.inter_scale_comp_diff_uv,
            ),
            (
                "inter_scale_compound_diff_luma_hbd",
                &layouts.inter_pred,
                &raw mut (*lib).shaders_hbd.inter_scale_comp_diff_y,
            ),
            (
                "inter_scale_compound_masked_hbd",
                &layouts.inter_pred,
                &raw mut (*lib).shaders_hbd.inter_scale_comp_masked,
            ),
            ("inter_scale_obmc_above_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_scale_obmc_above),
            ("inter_scale_obmc_left_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_scale_obmc_left),
        ]
    };
    list.into_iter()
        .map(|(name, layout, dst)| {
            let bytecode = bytecode(name).ok_or_else(|| format!("the build has no bytecode of the shader {name}"))?;
            Ok(Task { bytecode, layout: layout.clone(), dst })
        })
        .collect()
}

/// Releases the pipelines and layouts of the library.
///
/// # Safety
/// `lib` is the library and no thread creates pipelines any more.
unsafe fn release(lib: *mut compute_shader_lib) {
    // SAFETY: the first fields of the library are SHADERS pipelines and LAYOUTS root signatures (asserted
    // above); each COM pointer is null or holds the reference the creation put there.
    unsafe {
        let shaders = lib.cast::<ComputeShader>();
        for index in 0..SHADERS {
            com_release(&raw mut (*shaders.add(index)).pso);
            com_release(&raw mut (*shaders.add(index)).signaturePtr);
        }
        let layouts = &raw mut (*lib).sig_idct;
        for index in 0..LAYOUTS {
            com_release(layouts.add(index));
        }
    }
}

/// `wait_shader_create_complete` of the C++: waits for the pipelines. Returns 0 when all were created,
/// -1 without a library. Joined before: the result of the creation stays what it was; a pipeline that
/// could not be created must fail every decoder creation, not only the first one.
///
/// # Safety
/// `lib` is null or a library of [`av1_create_pipeline_cache_handle`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_wait_shader_create_complete(lib: *mut compute_shader_lib) -> c_int {
    let shader_lib = lib.cast::<ShaderLib>();
    if shader_lib.is_null() {
        return -1;
    }
    // SAFETY: the library is the first field of a ShaderLib (the caller's contract); the other fields are
    // made for shared use, and the C++ fields are written through the pointer.
    unsafe {
        let workers =
            std::mem::take(&mut *(*shader_lib).workers.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        let errors = &(*shader_lib).errors;
        for worker in workers {
            if worker.join().is_err() {
                errors.fetch_or(E_FAIL.0, Ordering::Relaxed);
            }
        }
        (*lib).threads = 0;
        (*lib).create_shader_errors = errors.load(Ordering::Relaxed);
        (*lib).create_shader_errors
    }
}

/// `av1_create_pipeline_cache_handle` (`aom/aom_decoder.h`): the layouts and pipelines of the decoder on
/// a device. With `threads` the pipelines are created on that many threads (6 at most) while the call
/// returns; [`av1_wait_pipeline_cache_handle`] waits for them. Returns null when a layout, or without
/// threads a pipeline, could not be created.
///
/// # Safety
/// `d3d12device` is null or an `ID3D12Device *` that lives until the handle is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_create_pipeline_cache_handle(d3d12device: *mut c_void, threads: c_int) -> *mut c_void {
    // SAFETY: the caller's contract.
    let Some(device) = (unsafe { ID3D12Device::from_raw_borrowed(&d3d12device) }) else {
        return std::ptr::null_mut();
    };
    let device = Device::new(device);

    // For the logs of the console runs: what the device says about double precision in shaders. The inter
    // shaders for scaled references used it until the global motion fix, and the console, which accepted
    // those shaders, returned 0 from it (docs/gpu-av1-decoder.md, "Step 4"). No shader uses it any more.
    log(
        false,
        &format!(
            "GPU double precision shader operations: {}",
            match device.double_precision_shader_ops() {
                None => "not known",
                Some(true) => "reported",
                Some(false) => "not reported",
            }
        ),
    );

    let shader_lib = Box::into_raw(Box::new(ShaderLib {
        // SAFETY: the C++ struct is COM pointers, numbers, a queue and thread handles nobody uses; all
        // zero is what its constructor leaves.
        lib: unsafe { std::mem::zeroed() },
        workers: Mutex::new(Vec::new()),
        errors: Arc::new(AtomicI32::new(0)),
    }));
    let handle = shader_lib.cast::<c_void>();
    let lib = shader_lib.cast::<compute_shader_lib>();
    // SAFETY: `lib` is the new, zeroed library.
    let tasks = unsafe {
        create_layouts(device, lib)
            .map_err(|error| format!("pipeline layout: {} hr 0x{:08X}", error.step, error.code as u32))
            .and_then(|layouts| create_tasks(lib, &layouts))
    };
    let tasks = match tasks {
        Ok(tasks) => tasks,
        Err(reason) => {
            log(true, &format!("MEMORY FAILED {reason}"));
            // SAFETY: the handle of this call, with no threads.
            unsafe { av1_destroy_pipeline_cache_handle(handle) };
            return std::ptr::null_mut();
        }
    };

    let threads = threads.clamp(0, MAX_CREATE_THREADS);
    if threads == 0 {
        let mut failed = false;
        for (index, task) in tasks.iter().enumerate() {
            if let Err(error) = create_shader(device, task) {
                log_failed_pipeline(index, task, error);
                failed = true;
            }
        }
        if failed {
            // SAFETY: the handle of this call, with no threads.
            unsafe { av1_destroy_pipeline_cache_handle(handle) };
            return std::ptr::null_mut();
        }
    } else {
        let tasks = Arc::new(tasks);
        let next = Arc::new(AtomicUsize::new(0));
        // SAFETY: the new library; nothing else has the handle yet.
        unsafe {
            (*lib).threads = threads;
            let mut workers = (*shader_lib).workers.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            for _ in 0..threads {
                let (device, tasks, next) = (device.owned(), tasks.clone(), next.clone());
                let errors = (*shader_lib).errors.clone();
                workers.push(std::thread::spawn(move || create_shader_thread(device, tasks, next, errors)));
            }
        }
    }
    handle
}

/// `av1_wait_pipeline_cache_handle` (`aom/aom_decoder.h`): waits for the pipelines; 0 when all were
/// created.
///
/// # Safety
/// `handle` is null or a handle of [`av1_create_pipeline_cache_handle`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_wait_pipeline_cache_handle(handle: *mut c_void) -> c_int {
    // SAFETY: the caller's contract.
    unsafe { rav1g_wait_shader_create_complete(handle.cast()) }
}

/// `av1_destroy_pipeline_cache_handle` (`aom/aom_decoder.h`).
///
/// # Safety
/// `handle` is null or a handle of [`av1_create_pipeline_cache_handle`] that no decoder uses; it is not
/// used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_destroy_pipeline_cache_handle(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the caller's contract: the handle is a ShaderLib from Box::into_raw.
    unsafe {
        rav1g_wait_shader_create_complete(handle.cast());
        release(handle.cast());
        drop(Box::from_raw(handle.cast::<ShaderLib>()));
    }
}

// ---------------------------------------------------------------------------------------------------------
// The constant tables of the shaders.
// ---------------------------------------------------------------------------------------------------------

/// One step of the random register of the film grain synthesis (the specification's get_random_number
/// without the reduction to the bits asked for).
fn random_register_step(value: u32) -> u32 {
    let bit = (value ^ (value >> 1) ^ (value >> 3) ^ (value >> 12)) & 1;
    (value >> 1) | (bit << 15)
}

fn write_ints(bytes: &mut [u8], values: impl Iterator<Item = i32>) {
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        *chunk = value.to_ne_bytes();
    }
}

/// For every 16-bit state of the random register, its state `steps` steps later: the shaders look up
/// where the generator of a grain block stands instead of stepping it.
fn fill_random_lut(bytes: &mut [u8], steps: u32) {
    write_ints(bytes, (0..65536u32).map(|start| (0..steps).fold(start, |value, _| random_register_step(value)) as i32));
}

/// The masks of compound prediction with a wedge and of inter-intra prediction, for luma blocks and then
/// for their chroma blocks of half the size, as `inter_compound_masked.hlsl` and the other mask readers
/// index them. `offsets[bsize]` gets where the masks of a block size start, luma and chroma, in units of
/// 64 bytes.
///
/// # Safety
/// The wedge masks are built (`av1_init_wedge_masks`).
unsafe fn fill_mask_lut(ptr: &mut [u8], offsets: &mut [[c_int; 2]; BLOCK_SIZES_ALL as usize]) {
    let mut offset = 0usize;
    // The inter-intra weights of a block: all 32; by row; by column; by the smaller of row and column.
    let inter_intra = |ptr: &mut [u8], offset: &mut usize, bw: usize, bh: usize, size: usize, scale: usize| {
        // SAFETY: a table of the entropy decoder, constant; a copy of it.
        let weights = unsafe { (&raw const ii_weights1d).read() };
        let patterns: [&dyn Fn(usize, usize) -> u8; 4] =
            [&|_, _| 32, &|_, y| weights[y * scale], &|x, _| weights[x * scale], &|x, y| weights[x.min(y) * scale]];
        for pattern in patterns {
            for y in 0..bh {
                for x in 0..bw {
                    ptr[*offset + x + y * bw] = pattern(x, y);
                }
            }
            *offset += size;
        }
    };
    for bsize in 0..BLOCK_SIZES_ALL as usize {
        // SAFETY: tables of the entropy decoder; the masks of a block size with wedge bits are 2 x 16
        // arrays of bw * bh bytes, built by av1_init_wedge_masks (the caller's contract).
        unsafe {
            let wedge = &wedge_params_lookup[bsize];
            if wedge.bits == 0 {
                continue;
            }
            let bw = BLOCK_SIZE_WIDE[bsize] as usize;
            let bh = BLOCK_SIZE_HIGH[bsize] as usize;
            let size = bw * bh;
            offsets[bsize][0] = ((offset + 63) >> 6) as c_int;
            for index in 0..16 {
                for sign in 0..2 {
                    let mask = std::slice::from_raw_parts((*wedge.masks.add(sign))[index], size);
                    ptr[offset..offset + size].copy_from_slice(mask);
                    offset += size;
                }
            }
            let scale = ii_size_scales[bsize] as usize;
            inter_intra(ptr, &mut offset, bw, bh, size, scale);
        }
    }
    for bsize in 0..BLOCK_SIZES_ALL as usize {
        // SAFETY: as above; a chroma mask averages 2x2 bytes of the luma mask of 2bw * 2bh bytes.
        unsafe {
            let wedge = &wedge_params_lookup[bsize];
            if wedge.bits == 0 {
                continue;
            }
            let bw = (BLOCK_SIZE_WIDE[bsize] >> 1) as usize;
            let bh = (BLOCK_SIZE_HIGH[bsize] >> 1) as usize;
            let size = (bw * bh).max(64);
            offsets[bsize][1] = ((offset + 63) >> 6) as c_int;
            for index in 0..16 {
                for sign in 0..2 {
                    let mask = std::slice::from_raw_parts((*wedge.masks.add(sign))[index], 4 * bw * bh);
                    let at = |x: usize, y: usize| u32::from(mask[x + y * bw * 2]);
                    for y in 0..bh {
                        for x in 0..bw {
                            let sum = at(x * 2, y * 2)
                                + at(x * 2 + 1, y * 2)
                                + at(x * 2, y * 2 + 1)
                                + at(x * 2 + 1, y * 2 + 1);
                            ptr[offset + x + y * bw] = ((sum + 2) >> 2) as u8;
                        }
                    }
                    offset += size;
                }
            }
            let scale = ii_size_scales[SS_SIZE_420[bsize] as usize] as usize;
            inter_intra(ptr, &mut offset, bw, bh, size, scale);
        }
    }
}

/// `av1_upload_luts` of the C++: fills the constant tables of the decoder (random register of the film
/// grain for luma and chroma, the Gaussian sequence, the masks, the warp filter) and the offsets of the
/// masks in `Av1Core`. Returns 0 or the `HRESULT` of the call that failed.
///
/// # Safety
/// `dec` is the decoder with its device, its direct queue and the five buffers, created as
/// `DeviceOnlyConst`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rav1g_upload_luts(dec: *mut Av1Core) -> i32 {
    // SAFETY: the caller's contract.
    let dec = unsafe { &mut *dec };
    // SAFETY: the COM pointers of the decoder's context.
    let (device, queue) = unsafe {
        (com_ref::<_, ID3D12Device>(&dec.compute.device), com_ref::<_, ID3D12CommandQueue>(&dec.compute.queue_direct))
    };
    let (Some(device), Some(queue)) = (device, queue) else {
        return E_POINTER.0;
    };
    // SAFETY: the buffers of the decoder (the caller's contract).
    let buffer = |object: *mut GpuBufferObject| unsafe {
        object.as_ref().and_then(|object| Some((BufferRef::from_raw(&object.dev)?, object.size)))
    };
    let (Some(luma), Some(chroma), Some(gaussian), Some(masks), Some(warp)) = (
        buffer(dec.filmgrain_random_luma),
        buffer(dec.filmgrain_random_chroma),
        buffer(dec.filmgrain_gaus),
        buffer(dec.inter_mask_lut),
        buffer(dec.inter_warp_filter),
    ) else {
        return E_POINTER.0;
    };

    // The register of a luma grain block is read 82 steps behind its seed, that of a chroma block 44.
    let mut fill_luma = |bytes: &mut [u8]| fill_random_lut(bytes, 82);
    let mut fill_chroma = |bytes: &mut [u8]| fill_random_lut(bytes, 44);
    let mut fill_gaussian = |bytes: &mut [u8]| write_ints(bytes, GAUSSIAN_SEQUENCE.iter().copied());
    let mut fill_warp = |bytes: &mut [u8]| {
        // SAFETY: a table of the entropy decoder, constant; a copy of it.
        let filter = unsafe { (&raw const warped_filter).read() };
        write_ints(bytes, filter.iter().flatten().map(|&tap| i32::from(tap)));
    };
    let wedge_offsets = &mut dec.wedge_offsets;
    let mut fill_masks = |bytes: &mut [u8]| {
        // SAFETY: the masks are built right here.
        unsafe {
            av1_init_wedge_masks();
            fill_mask_lut(bytes, wedge_offsets);
        }
    };
    let mut uploads = [
        Upload { dst: luma.0, size: luma.1, fill: &mut fill_luma },
        Upload { dst: chroma.0, size: chroma.1, fill: &mut fill_chroma },
        Upload { dst: gaussian.0, size: gaussian.1, fill: &mut fill_gaussian },
        Upload { dst: masks.0, size: masks.1, fill: &mut fill_masks },
        Upload { dst: warp.0, size: warp.1, fill: &mut fill_warp },
    ];
    match Device::new(device).upload(Queue::new(queue), &mut uploads) {
        Ok(()) => 0,
        Err(error) => error.code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `get_random_number_test` of the C++, as it was written.
    fn get_random_number_test(mut val: i32) -> i32 {
        let bit = (val ^ (val >> 1) ^ (val >> 3) ^ (val >> 12)) & 1;
        val = (val >> 1) | (bit << 15);
        val
    }

    #[test]
    fn random_lut_as_in_cpp() {
        for steps in [82u32, 44] {
            let mut bytes = vec![0u8; 65536 * 4];
            fill_random_lut(&mut bytes, steps);
            for start in (0..65536).step_by(257).chain([1, 2, 65535]) {
                let mut register = start as i32;
                for _ in 0..steps {
                    register = get_random_number_test(register);
                }
                let at = start * 4;
                assert_eq!(
                    i32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap()),
                    register,
                    "{steps} steps from {start}"
                );
            }
        }
    }

    #[test]
    fn every_pipeline_names_its_shader_once() {
        let source = include_str!("compute.rs");
        let list = &source[source.find("let idct = |tx: c_int|").unwrap()..source.find("list.into_iter()").unwrap()];
        let names: Vec<&str> = list
            .lines()
            .filter_map(|line| line.trim().trim_start_matches('(').strip_prefix('"'))
            .map(|rest| &rest[..rest.find('"').unwrap()])
            .collect();
        assert_eq!(names.len(), 93);
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        for name in names {
            assert!(bytecode(name).is_some(), "the glue has the bytecode of {name}");
        }
    }
}

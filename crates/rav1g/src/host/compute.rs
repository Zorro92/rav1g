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
//! The library is the struct `compute_shader_lib`, whose pipeline states and root signatures the frame
//! pipeline reads; it is created here, zeroed, inside a Rust value that also holds the schedule of its
//! pipelines (`pipeline_schedule.rs`). The bytecode of the shaders is the table of
//! `crates/rav1g-sys/src/shaders.rs`: the files of the DXC build, embedded as they are.
//!
//! The layouts are created with the library. The pipeline states are compiled by the device's driver,
//! about 31 s of CPU for all 96 on the console (8.5 s on 4 threads): the library compiles the kinds of
//! states the application asked to prepare (`class`, `Rav1gDeviceSettings::prepare`) right away, and
//! every other state when a decoder creation or a frame needs it, or in the background for a decoder
//! that may need it later (docs/integration.md, "Preparing the pipeline states"). Every dispatch takes
//! its pipeline through `core::pipeline`, which waits for a state that is not ready: no state is used
//! before it exists.

use std::ffi::{c_int, c_void};
use std::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::core::Interface;

use super::libaom::{II_SIZE_SCALES, II_WEIGHTS1D, WARPED_FILTER, WedgeMasks};
use super::log::log;
use super::pipeline_schedule::{Priority, Report, Scheduler};
use super::shader_pick::{self, Set};
use super::tables::{BLOCK_SIZE_HIGH, BLOCK_SIZE_WIDE, SS_SIZE_420};
use super::types::{Av1Core, ComputeShader, GpuBufferObject, compute_shader_lib};
use crate::gpu::d3d12::{com_ref, com_release, com_store};
use crate::gpu::{self, BufferRef, Device, Layout, LayoutDesc, Queue, Upload};
use crate::sys::shaders::Shader;
use crate::sys::{
    BLOCK_SIZES_ALL, TX_4X4, TX_4X8, TX_4X16, TX_8X4, TX_8X8, TX_8X16, TX_8X32, TX_16X4, TX_16X8, TX_16X16, TX_16X32,
    TX_16X64, TX_32X8, TX_32X16, TX_32X32, TX_32X64, TX_64X16, TX_64X32, TX_64X64, TX_SIZES_ALL,
};

/// Threads that create pipelines at most (`MAX_CREATE_SHADER_THREADS` of the C++). The console's 4
/// cores give no gain beyond 4; an application may give up to 6.
const MAX_CREATE_THREADS: c_int = 6;

/// The kinds of pipeline states, as bits: what a device prepares at its creation, what a decoder needs
/// at its creation and a frame at its setup. Every state is of one kind (`class_of`).
pub(crate) mod class {
    /// What every stream needs: the transforms, CDEF and loop restoration, the copies, the block lists.
    pub const CORE: u32 = 1;
    /// Prediction, reconstruction, loop filter and borders of 8-bit frames, and of frames of more bits.
    pub const DEPTH_8: u32 = 1 << 1;
    pub const DEPTH_10: u32 = 1 << 2;
    /// Inter prediction from references of another size than the frame (AV1 7.11.3.3), by bit depth.
    pub const SCALED_8: u32 = 1 << 3;
    pub const SCALED_10: u32 = 1 << 4;
    /// Film grain synthesis (AV1 7.18.3).
    pub const FILM_GRAIN: u32 = 1 << 5;
    /// The super-resolution upscale (AV1 7.16).
    pub const SUPERRES: u32 = 1 << 6;
    /// The display pass by its mode (`display_pass::Choice::mode` 0 PQ, 1 SDR, 2 HLG).
    pub const DISPLAY_PQ: u32 = 1 << 7;
    pub const DISPLAY_SDR: u32 = 1 << 8;
    pub const DISPLAY_HLG: u32 = 1 << 9;
    pub const DISPLAY: u32 = DISPLAY_PQ | DISPLAY_SDR | DISPLAY_HLG;
    pub const ALL: u32 = (1 << 10) - 1;

    pub fn depth(hbd: bool) -> u32 {
        if hbd { DEPTH_10 } else { DEPTH_8 }
    }

    pub fn scaled(hbd: bool) -> u32 {
        if hbd { SCALED_10 } else { SCALED_8 }
    }

    /// The class of the display pass of `mode`.
    pub fn display(mode: u32) -> u32 {
        match mode {
            0 => DISPLAY_PQ,
            2 => DISPLAY_HLG,
            _ => DISPLAY_SDR,
        }
    }

    /// The classes for a log line: "core, 10-bit, display PQ".
    pub fn names(classes: u32) -> String {
        const NAMES: [&str; 10] = [
            "core",
            "8-bit",
            "10-bit",
            "scaled references 8-bit",
            "scaled references 10-bit",
            "film grain",
            "superres",
            "display PQ",
            "display SDR",
            "display HLG",
        ];
        let names: Vec<&str> =
            NAMES.iter().enumerate().filter(|(bit, _)| classes & (1 << bit) != 0).map(|(_, name)| *name).collect();
        if names.is_empty() { "none".to_string() } else { names.join(", ") }
    }
}

/// The class of the state of a shader, by its name (the test `every_class_has_its_states` holds the
/// names to the slots of the library).
fn class_of(name: &str) -> u32 {
    let hbd = name.ends_with("_hbd");
    match name {
        "display_pass" => class::DISPLAY_PQ,
        "display_pass_sdr" => class::DISPLAY_SDR,
        "display_pass_hlg" => class::DISPLAY_HLG,
        _ if name.starts_with("film_grain") => class::FILM_GRAIN,
        _ if name.starts_with("upscaling") => class::SUPERRES,
        // inter_scale_warp_compound is the warp of compound prediction, which every inter frame can have.
        _ if name.starts_with("inter_scale") && !name.starts_with("inter_scale_warp") => class::scaled(hbd),
        _ if ["inter_", "intra_", "reconstruct_block", "loopfilter_"].iter().any(|kind| name.starts_with(kind)) => {
            class::depth(hbd)
        }
        _ => class::CORE,
    }
}

/// The test mode of the on-demand path (`av1_set_pipeline_prepare_test`, `-preparetest`), for the devices
/// created after the call: 0 off; 1: the device prepares nothing, so every state is compiled when a
/// decoder creation or a frame needs it (or in the background); 2: neither the device nor a decoder
/// creation nor a frame prepares anything, so every state is compiled at its first dispatch.
static PREPARE_TEST: AtomicI32 = AtomicI32::new(0);

/// `av1_set_pipeline_prepare_test` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_pipeline_prepare_test(mode: c_int) {
    PREPARE_TEST.store(mode.clamp(0, 2), Ordering::Relaxed);
}

/// The bytecode table for this set. Only shader-pick builds have alternate sets.
fn shader_table(set: Set) -> &'static [Shader] {
    #[cfg(feature = "shader-pick")]
    if set == Set::Od {
        return crate::sys::shaders::SHADERS_OD;
    }
    let _ = set;
    crate::sys::shaders::SHADERS
}

/// The bytecode of a shader by its pipeline name, of one of the sets of the build.
fn bytecode_of(name: &str, set: Set) -> Option<&'static [u8]> {
    shader_table(set).iter().find(|entry| entry.name == name).map(|entry| entry.code)
}

/// The bytecode a pipeline state has without a pick.
#[cfg(test)]
fn bytecode(name: &str) -> Option<&'static [u8]> {
    bytecode_of(name, Set::Dxc)
}

/// The names of the shaders the build has bytecode of, in the order of the table.
fn shader_names() -> Vec<&'static str> {
    shader_table(Set::Dxc).iter().map(|entry| entry.name).collect()
}

/// The shader-pick experiment (`shader_pick.rs`): which of the two bytecodes of the build
/// the pipeline state of a shader is made of, in the devices created after the call. An empty text is
/// the default DXC bytecode for every shader. Only a shader-pick build accepts a nonempty pick.
pub(crate) fn set_shader_pick(text: &str) -> Result<(), String> {
    shader_pick::set(text, &shader_names())
}

/// With the feature `shader-pick`, the log lines about the bytecodes of the build: one per set with its compiler
/// and arguments ("SHADERSET ..."), and the pick as it is set ("SHADERPICK ...": how many shaders are of
/// which set, and the names of the ones that are not of the largest). No lines without the feature.
pub(crate) fn shader_pick_lines() -> Vec<String> {
    if !cfg!(feature = "shader-pick") {
        return Vec::new();
    }
    let names = shader_names();
    let text = shader_pick::get();
    let mut lines: Vec<String> = ["dxc", "od"]
        .iter()
        .zip(crate::sys::SHADER_SETS)
        .map(|(set, line)| format!("SHADERSET {set}: {line}"))
        .collect();
    match shader_pick::resolve(&text, &names) {
        Ok(sets) => lines.extend(shader_pick::wrap(&shader_pick::describe(&text, &names, &sets), 700)),
        Err(error) => lines.push(format!("SHADERPICK {error}")),
    }
    lines
}

/// The library as the pipeline cache handle points to it.
#[repr(C)]
struct ShaderLib {
    /// What the frame pipeline reads. First: the handle is also a `compute_shader_lib *`.
    lib: compute_shader_lib,
    /// The pipelines, in the order of `create_tasks`: the slot of a pipeline in the schedule.
    tasks: Vec<Task>,
    /// Set once the tasks are made.
    scheduler: Option<Arc<Scheduler>>,
    /// The classes all of whose states are ready.
    ready_classes: AtomicU32,
    /// The states the device prepared at its creation (`rav1g_device_wait_pipelines` waits for them).
    batch: Vec<usize>,
    /// `PREPARE_TEST` when the device was created.
    test: c_int,
}

// SAFETY: the library is shared by the decoders of a device and the threads of its schedule. What they
// write of it: a slot's pipeline state, by the one thread that compiles it before its `ready` flag is set
// (release); the other fields are atomics or are written only while the library is made or released.
unsafe impl Send for ShaderLib {}
unsafe impl Sync for ShaderLib {}

/// The library for the compile function of its schedule.
#[derive(Clone, Copy)]
struct LibPtr(*const ShaderLib);
// SAFETY: as for ShaderLib; the schedule is closed (its threads joined) before the library is released.
unsafe impl Send for LibPtr {}
unsafe impl Sync for LibPtr {}

/// The line of a complete request of the schedule: the batch of the device creation ("PIPELINE states:
/// ..."), an on-demand batch of a decoder creation, a frame or a dispatch, or the background states. The
/// states compiled, the wall time from the request, the sum of their times and the slowest ten. The
/// pipeline states are compiled by the device's driver (31 s of CPU for 96 states on the console,
/// docs/integration.md): this says where that time goes.
fn log_report(names: &[&'static str], report: &Report) {
    let wall = report.wall_micros as f64 / 1000.0;
    let mut each: Vec<(u64, &str)> =
        report.micros.iter().copied().zip(report.slots.iter().map(|&slot| names[slot])).collect();
    let sum: u64 = each.iter().map(|(us, _)| us).sum();
    each.sort_by_key(|&(us, _)| std::cmp::Reverse(us));
    let slowest: Vec<String> =
        each.iter().take(10).map(|(us, name)| format!("{name} {:.0}", *us as f64 / 1000.0)).collect();
    let failed = if report.failed != 0 { format!("; FAILED hr 0x{:08X}", report.failed as u32) } else { String::new() };
    log(
        report.failed != 0,
        &format!(
            "PIPELINE {}: {} created in {wall:.0} ms on {} threads, {:.0} ms in all; slowest (ms): {}{failed}",
            report.label,
            each.len(),
            report.threads.max(1),
            sum as f64 / 1000.0,
            slowest.join(", ")
        ),
    );
}

// The pipelines and their layouts are the first fields of the C++ struct: `SHADERS` pipelines, then
// `LAYOUTS` root signatures. They are released as two arrays.
const SHADERS: usize = offset_of!(compute_shader_lib, sig_idct) / size_of::<ComputeShader>();
const LAYOUTS: usize =
    (offset_of!(compute_shader_lib, compliled_) - offset_of!(compute_shader_lib, sig_idct)) / size_of::<*mut c_void>();
const _: () = {
    assert!(offset_of!(compute_shader_lib, shader_idct) == 0);
    assert!(SHADERS * size_of::<ComputeShader>() == offset_of!(compute_shader_lib, sig_idct));
    // 20 transform pipelines, 32 shared shaders and 22 per bit depth: one slot per pipeline of
    // `create_tasks` (the test `every_pipeline_names_its_shader_once`).
    assert!(SHADERS == 20 + 32 + 2 * 22);
    assert!(LAYOUTS == 22);
    assert!(offset_of!(ShaderLib, lib) == 0);
};

/// One pipeline to create.
struct Task {
    /// The shader, and which of the bytecodes of the build it is (the shader pick), for the log.
    name: &'static str,
    set: Set,
    bytecode: &'static [u8],
    layout: Layout,
    dst: *mut ComputeShader,
    /// Its kind (`class_of`).
    class: u32,
}

/// The pipeline of a task into its field of the library, whose layout and bytecode are there already;
/// then its `ready` flag.
///
/// # Safety
/// The field belongs to the task, and only the thread that compiles the task writes its pipeline state.
unsafe fn create_shader(device: Device<'_>, task: &Task) -> Result<(), gpu::Error> {
    let pipeline = device.create_pipeline(&task.layout, task.bytecode)?;
    // SAFETY: the caller's contract; the state is null until here, and nothing reads it before `ready`.
    unsafe {
        com_store(&raw mut (*task.dst).pso, pipeline.into_raw());
        (*task.dst).ready.store(true, Ordering::Release);
    }
    Ok(())
}

fn log_failed_pipeline(index: usize, task: &Task, error: gpu::Error) {
    let set = if cfg!(feature = "shader-pick") {
        format!(", the bytecode of the set {}", shader_pick::label(task.set))
    } else {
        String::new()
    };
    log(
        true,
        &format!(
            "MEMORY FAILED compute pipeline state {index} ({}{set}, {} bytes of bytecode): hr 0x{:08X}",
            task.name,
            task.bytecode.len(),
            error.code as u32
        ),
    );
}

/// The compile function of the schedule of a library: the pipeline of a slot; when it was the last of
/// its class that was not ready, the class is ready.
///
/// # Safety
/// `lib` is the library, alive while its schedule runs; the schedule gives a slot to one thread.
unsafe fn compile_slot(lib: LibPtr, device: Device<'_>, slot: usize) -> Result<(), i32> {
    // SAFETY: the caller's contract.
    unsafe {
        let lib = &*lib.0;
        let task = &lib.tasks[slot];
        if let Err(error) = create_shader(device, task) {
            log_failed_pipeline(slot, task, error);
            return Err(error.code);
        }
        let class_ready = lib
            .tasks
            .iter()
            .filter(|other| other.class == task.class)
            .all(|other| (*other.dst).ready.load(Ordering::Acquire));
        if class_ready {
            lib.ready_classes.fetch_or(task.class, Ordering::Release);
        }
        Ok(())
    }
}

/// The slots of the states of `classes`.
fn slots_of(lib: &ShaderLib, classes: u32) -> Vec<usize> {
    lib.tasks.iter().enumerate().filter(|(_, task)| task.class & classes != 0).map(|(slot, _)| slot).collect()
}

/// The slots of `slots` whose states are not ready.
fn not_ready(lib: &ShaderLib, slots: &[usize]) -> usize {
    // SAFETY: the fields of the library's tasks.
    slots.iter().filter(|&&slot| !unsafe { (*lib.tasks[slot].dst).ready.load(Ordering::Acquire) }).count()
}

/// The layouts of the library, by the name of their field (`sig_<name>`).
struct Layouts {
    idct: Layout,
    common111: Layout,
    common0102: Layout,
    common0110: Layout,
    copy_plane: Layout,
    pred_blocks: Layout,
    intra_pred: Layout,
    inter_pred: Layout,
    loop_rest: Layout,
    loop_rest_sp_tex: Layout,
    cdef_filter: Layout,
    cdef_filter_sp_tex: Layout,
    film_grain: Layout,
    lf: Layout,
    lf_gen: Layout,
    upscale: Layout,
    upscale_sp_tex: Layout,
    copy_plane_sp: Layout,
    copy_plane_sp_tex: Layout,
    display_pass: Layout,
    film_grain_tex: Layout,
    cdef_lists: Layout,
    mi_grid_build: Layout,
}

/// Creates the layouts in the order of the C++ and puts each into its field of the library.
///
/// # Safety
/// `lib` is the zeroed library.
unsafe fn create_layouts(device: Device<'_>, lib: *mut compute_shader_lib) -> Result<Layouts, gpu::Error> {
    let layout_planes = |read_buffers, write_buffers, constant_blocks, inline_constants, write_planes| {
        device.create_layout(LayoutDesc {
            read_buffers,
            write_buffers,
            constant_blocks,
            inline_constants,
            write_planes,
        })
    };
    let layout = |read_buffers, write_buffers, constant_blocks, inline_constants| {
        layout_planes(read_buffers, write_buffers, constant_blocks, inline_constants, 0)
    };
    let layouts = Layouts {
        idct: layout(1, 2, 2, 2)?,
        common111: layout(1, 1, 1, 0)?,
        common0102: layout(0, 1, 0, 2)?,
        common0110: layout(0, 1, 1, 0)?,
        copy_plane: layout(0, 1, 0, 6)?,
        pred_blocks: layout(5, 2, 1, 7)?,
        intra_pred: layout(3, 1, 1, 10)?,
        inter_pred: layout(5, 1, 1, 4)?,
        loop_rest: layout(3, 1, 2, 2)?,
        loop_rest_sp_tex: layout_planes(3, 1, 2, 3, 2)?,
        cdef_filter: layout(2, 1, 1, 0)?,
        cdef_filter_sp_tex: layout_planes(2, 1, 1, 0, 2)?,
        film_grain: layout(3, 1, 1, 0)?,
        lf: layout(1, 1, 1, 5)?,
        lf_gen: layout(2, 1, 1, 6)?,
        upscale: layout(0, 1, 1, 3)?,
        upscale_sp_tex: layout_planes(0, 1, 1, 5, 2)?,
        // The copy layouts bind u0 and the inline constants. The direct layout also binds both texture planes.
        copy_plane_sp: layout(0, 1, 0, 11)?,
        copy_plane_sp_tex: layout_planes(0, 1, 0, 11, 2)?,
        // display_pass.hlsl: t0 the tone map's tables, u0 the frame pool, its inline constants, the two planes.
        display_pass: layout_planes(1, 1, 0, super::display_pass::CONSTANTS as u32, 2)?,
        // film_grain_filter_sp_tex_v2.hlsl: the layout of film grain with the two planes in place of u0.
        film_grain_tex: layout_planes(3, 0, 1, 0, 2)?,
        // cdef_lists.hlsl / cdef_lists_v2.hlsl: the mode info pool and grid, the two lists, 6 inline constants.
        cdef_lists: layout(2, 2, 0, 6)?,
        // mi_grid_build.hlsl: the mode info pool and the runs of the frame's records, the grid, 7 inline
        // constants.
        mi_grid_build: layout(2, 1, 0, 7)?,
    };
    // SAFETY: the fields are null COM pointers of root signatures (the caller's contract).
    unsafe {
        com_store(&raw mut (*lib).sig_idct, layouts.idct.clone().into_raw());
        com_store(&raw mut (*lib).sig_common111, layouts.common111.clone().into_raw());
        com_store(&raw mut (*lib).sig_common0102, layouts.common0102.clone().into_raw());
        com_store(&raw mut (*lib).sig_common0110, layouts.common0110.clone().into_raw());
        com_store(&raw mut (*lib).sig_copy_plane, layouts.copy_plane.clone().into_raw());
        com_store(&raw mut (*lib).sig_pred_blocks, layouts.pred_blocks.clone().into_raw());
        com_store(&raw mut (*lib).sig_intra_pred, layouts.intra_pred.clone().into_raw());
        com_store(&raw mut (*lib).sig_inter_pred, layouts.inter_pred.clone().into_raw());
        com_store(&raw mut (*lib).sig_loop_rest, layouts.loop_rest.clone().into_raw());
        com_store(&raw mut (*lib).sig_loop_rest_sp_tex, layouts.loop_rest_sp_tex.clone().into_raw());
        com_store(&raw mut (*lib).sig_cdef_filter, layouts.cdef_filter.clone().into_raw());
        com_store(&raw mut (*lib).sig_cdef_filter_sp_tex, layouts.cdef_filter_sp_tex.clone().into_raw());
        com_store(&raw mut (*lib).sig_film_grain, layouts.film_grain.clone().into_raw());
        com_store(&raw mut (*lib).sig_lf, layouts.lf.clone().into_raw());
        com_store(&raw mut (*lib).sig_lf_gen, layouts.lf_gen.clone().into_raw());
        com_store(&raw mut (*lib).sig_upscale, layouts.upscale.clone().into_raw());
        com_store(&raw mut (*lib).sig_upscale_sp_tex, layouts.upscale_sp_tex.clone().into_raw());
        com_store(&raw mut (*lib).sig_copy_plane_sp, layouts.copy_plane_sp.clone().into_raw());
        com_store(&raw mut (*lib).sig_copy_plane_sp_tex, layouts.copy_plane_sp_tex.clone().into_raw());
        com_store(&raw mut (*lib).sig_film_grain_tex, layouts.film_grain_tex.clone().into_raw());
        com_store(&raw mut (*lib).sig_cdef_lists, layouts.cdef_lists.clone().into_raw());
        com_store(&raw mut (*lib).sig_mi_grid_build, layouts.mi_grid_build.clone().into_raw());
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
    let list: Vec<(&'static str, &Layout, *mut ComputeShader)> = unsafe {
        let idct = |tx: c_int| &raw mut (*lib).shader_idct[tx as usize];
        vec![
            ("idct4x16", &layouts.idct, idct(TX_4X16)),
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
            ("idct4x4_v2", &layouts.idct, idct(TX_4X4)),
            ("idct4x8_v2", &layouts.idct, idct(TX_4X8)),
            ("idct8x4_v2", &layouts.idct, idct(TX_8X4)),
            ("idct8x8_v2", &layouts.idct, idct(TX_8X8)),
            ("idct_sort_blocks", &layouts.common111, &raw mut (*lib).shader_idct_sort),
            ("fill_buffer", &layouts.common0102, &raw mut (*lib).shader_fill_buffer),
            ("loop_restoration", &layouts.loop_rest, &raw mut (*lib).shader_loop_rest),
            ("loop_restoration_wiener", &layouts.loop_rest, &raw mut (*lib).shader_loop_rest_wiener),
            ("loop_restoration_sp_tex", &layouts.loop_rest_sp_tex, &raw mut (*lib).shader_loop_rest_sp_tex),
            (
                "loop_restoration_wiener_sp_tex",
                &layouts.loop_rest_sp_tex,
                &raw mut (*lib).shader_loop_rest_wiener_sp_tex,
            ),
            ("film_grain_filter", &layouts.film_grain, &raw mut (*lib).shader_filmgrain_filter),
            ("cdef_filter_v3", &layouts.cdef_filter, &raw mut (*lib).shader_cdef_filter_v3),
            ("cdef_filter_v3_sp_tex", &layouts.cdef_filter_sp_tex, &raw mut (*lib).shader_cdef_filter_v3_sp_tex),
            ("gen_pred_blocks", &layouts.pred_blocks, &raw mut (*lib).shader_gen_pred_blocks),
            ("gen_lf_blocks_vert", &layouts.lf_gen, &raw mut (*lib).shader_gen_lf_vert),
            ("gen_lf_blocks_hor", &layouts.lf_gen, &raw mut (*lib).shader_gen_lf_hor),
            ("copy_plane", &layouts.copy_plane, &raw mut (*lib).shader_copy_plane),
            ("copy_plane_10bit10x3", &layouts.copy_plane, &raw mut (*lib).shader_copy_plane_10bit10x3),
            ("upscaling", &layouts.upscale, &raw mut (*lib).shader_upscale),
            ("upscaling_sp_tex", &layouts.upscale_sp_tex, &raw mut (*lib).shader_upscale_sp_tex),
            ("inter_2x2chroma", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_2x2),
            ("inter_compound_v2", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp),
            ("inter_compound_diff_chroma", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_diff_uv),
            ("inter_compound_diff_luma", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_diff_y),
            ("inter_compound_masked", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_comp_masked),
            ("inter_main_v2", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_base),
            ("inter_obmc_above", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_obmc_above),
            ("inter_obmc_left", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_obmc_left),
            ("inter_warp", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_warp),
            ("inter_scale_warp_compound", &layouts.inter_pred, &raw mut (*lib).shaders_8bit.inter_warp_comp),
            ("inter_ext_borders", &layouts.common0110, &raw mut (*lib).shaders_8bit.inter_ext_borders),
            ("reconstruct_block", &layouts.intra_pred, &raw mut (*lib).shaders_8bit.reconstruct_block),
            ("loopfilter_vert", &layouts.lf, &raw mut (*lib).shaders_8bit.loopfilter_v),
            ("loopfilter_hor", &layouts.lf, &raw mut (*lib).shaders_8bit.loopfilter_h),
            ("inter_2x2chroma_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_2x2),
            ("inter_compound_diff_chroma_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_diff_uv),
            ("inter_compound_diff_luma_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_diff_y),
            ("inter_compound_v2_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp),
            ("inter_compound_masked_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_comp_masked),
            ("inter_main_v2_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_base),
            ("inter_obmc_above_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_obmc_above),
            ("inter_obmc_left_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_obmc_left),
            ("inter_scale_warp_compound_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_warp_comp),
            ("inter_warp_hbd", &layouts.inter_pred, &raw mut (*lib).shaders_hbd.inter_warp),
            ("intra_main_prefetch_hbd", &layouts.intra_pred, &raw mut (*lib).shader_intra_main_prefetch_hbd),
            ("intra_main_prefetch", &layouts.intra_pred, &raw mut (*lib).shader_intra_main_prefetch),
            ("intra_main_single_edge", &layouts.intra_pred, &raw mut (*lib).shader_intra_main_single_edge),
            ("intra_main_single_edge_hbd", &layouts.intra_pred, &raw mut (*lib).shader_intra_main_single_edge_hbd),
            ("intra_filter_prefetch", &layouts.intra_pred, &raw mut (*lib).shader_intra_filter_prefetch),
            ("intra_filter_prefetch_hbd", &layouts.intra_pred, &raw mut (*lib).shader_intra_filter_prefetch_hbd),
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
            ("copy_plane_sp", &layouts.copy_plane_sp, &raw mut (*lib).shader_copy_plane_sp),
            ("copy_plane_sp_tex_v2", &layouts.copy_plane_sp_tex, &raw mut (*lib).shader_copy_plane_sp_tex_v2),
            ("display_pass", &layouts.display_pass, &raw mut (*lib).shader_display_pass[0]),
            ("display_pass_sdr", &layouts.display_pass, &raw mut (*lib).shader_display_pass[1]),
            ("display_pass_hlg", &layouts.display_pass, &raw mut (*lib).shader_display_pass[2]),
            ("film_grain_filter_sp", &layouts.film_grain, &raw mut (*lib).shader_filmgrain_filter_sp),
            ("film_grain_filter_sp_tex_v2", &layouts.film_grain_tex, &raw mut (*lib).shader_filmgrain_filter_sp_tex_v2),
            ("cdef_lists", &layouts.cdef_lists, &raw mut (*lib).shader_cdef_lists),
            ("cdef_lists_v2", &layouts.cdef_lists, &raw mut (*lib).shader_cdef_lists_v2),
            ("mi_grid_build", &layouts.mi_grid_build, &raw mut (*lib).shader_mi_grid_build),
        ]
    };
    // Resolve the bytecode set per shader. In the normal build every shader uses the DXC set.
    let names = shader_names();
    let sets = shader_pick::resolve(&shader_pick::get(), &names)?;
    list.into_iter()
        .map(|(name, layout, dst)| {
            let set = names.iter().position(|other| *other == name).map_or(Set::Dxc, |index| sets[index]);
            let bytecode =
                bytecode_of(name, set).ok_or_else(|| format!("the build has no bytecode of the shader {name}"))?;
            Ok(Task { name, set, bytecode, layout: layout.clone(), dst, class: class_of(name) })
        })
        .collect()
}

/// The schedule of a library.
fn scheduler(lib: &ShaderLib) -> &Arc<Scheduler> {
    lib.scheduler.as_ref().expect("the library has its schedule")
}

/// `wait_shader_create_complete` of the C++: waits for the states the device prepared at its creation
/// (the calling thread compiles those no thread has begun). Returns 0 when all were created, -1 without
/// a library, else the codes of the failures or'ed. A state that could not be created stays failed: every
/// later wait for it fails too.
///
/// # Safety
/// `lib` is null or a library of [`create_pipeline_library`].
unsafe fn wait_shader_create_complete(lib: *mut compute_shader_lib) -> c_int {
    // SAFETY: the library is the first field of a ShaderLib (the caller's contract).
    let Some(shader_lib) = (unsafe { lib.cast::<ShaderLib>().as_ref() }) else {
        return -1;
    };
    scheduler(shader_lib).wait(&shader_lib.batch, None)
}

/// What a decoder needs of the states of its device (`aom_codec_dec_cfg::pipelines_needed`,
/// `pipelines_later`): the classes its creation waits for, and those it may need later, which are
/// compiled in the background. The creation also waits for the states the device prepared, as every
/// creation did before interface version 4: none of them is still being compiled, on the device's
/// threads above normal priority, while the decoder's first frames are parsed. One line says what the
/// creation needed and waited for, one per batch what was compiled for it, and one what goes to the
/// background. Returns 0, or the codes of the states that could not be created or'ed.
///
/// # Safety
/// `lib` is a library of [`create_pipeline_library`].
pub(crate) unsafe fn prepare_for_decoder(lib: *mut compute_shader_lib, needed: u32, later: u32) -> c_int {
    // SAFETY: the caller's contract.
    let lib = unsafe { &*lib.cast::<ShaderLib>() };
    if lib.test == 2 {
        log(
            false,
            "PIPELINE decoder creation: nothing prepared (-preparetest 2): every state is compiled at its first \
             dispatch",
        );
        return 0;
    }
    let start = Instant::now();
    let needed_slots = slots_of(lib, needed);
    let missing = not_ready(lib, &needed_slots);
    // The states of the stream first (at the priority of a waiter, with the device's threads), then
    // every one of them and of the prepared states: this thread helps with what is left of both.
    let label = format!("on demand for the decoder creation ({})", class::names(needed & !lib.ready_classes()));
    scheduler(lib).queue(&needed_slots, Priority::Needed, Some(&label));
    let mut slots = lib.batch.clone();
    slots.extend(needed_slots.iter().filter(|slot| !lib.batch.contains(slot)));
    let prepared_missing = not_ready(lib, &lib.batch);
    let code = scheduler(lib).wait(&slots, None);
    log(
        false,
        &format!(
            "PIPELINE decoder creation: needs {} states ({}), {missing} were not ready ({prepared_missing} of the \
             {} prepared ones); waited {:.0} ms",
            needed_slots.len(),
            class::names(needed),
            lib.batch.len(),
            start.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let later = later & !needed & !lib.ready_classes();
    if later != 0 {
        let background: Vec<usize> = slots_of(lib, later);
        let label = format!("in the background ({})", class::names(later));
        let queued = scheduler(lib).queue(&background, Priority::Background, Some(&label));
        if !queued.is_empty() {
            log(false, &format!("PIPELINE background: {} states ({}) queued", queued.len(), class::names(later)));
        }
    }
    code
}

/// The states of `classes` for a frame at its setup: nothing when they are ready, else waits for them
/// (the calling thread and the device's threads compile them) with a line about the batch. 0 or the
/// codes of the states that could not be created, or'ed.
///
/// # Safety
/// `lib` is the library of a decoder.
pub(crate) unsafe fn prepare_for_frame(lib: *mut compute_shader_lib, classes: u32, frame_number: c_int) -> c_int {
    // SAFETY: the caller's contract.
    let lib = unsafe { &*lib.cast::<ShaderLib>() };
    let missing = classes & !lib.ready_classes();
    if missing == 0 || lib.test == 2 {
        return 0;
    }
    let label = format!("on demand for frame {frame_number} ({})", class::names(missing));
    scheduler(lib).wait(&slots_of(lib, missing), Some(&label))
}

/// A dispatch takes a pipeline that is not ready (`core::pipeline`): waits for it, the calling thread
/// compiling it when no thread has begun. A line names it: with the frame's classes prepared at the
/// frame's setup this happens only with `-preparetest 2`. Returns whether the pipeline exists.
///
/// # Safety
/// `shader` is a slot of a library of [`create_pipeline_library`].
pub(crate) unsafe fn wait_for_pipeline(shader: *const ComputeShader) -> bool {
    // SAFETY: the caller's contract: the slot knows its library.
    unsafe {
        let lib = &*(*shader).library.cast::<ShaderLib>();
        let slot = (*shader).index as usize;
        let label = format!("at the first dispatch of {}", lib.tasks[slot].name);
        scheduler(lib).wait(&[slot], Some(&label)) == 0 && (*shader).ready.load(Ordering::Acquire)
    }
}

impl ShaderLib {
    fn ready_classes(&self) -> u32 {
        self.ready_classes.load(Ordering::Acquire)
    }
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

/// `av1_create_pipeline_cache_handle` (`aom/aom_decoder.h`): the layouts and pipelines of the decoder on
/// a device, every state prepared. See [`create_pipeline_library`].
///
/// # Safety
/// `d3d12device` is null or an `ID3D12Device *` that lives until the handle is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_create_pipeline_cache_handle(d3d12device: *mut c_void, threads: c_int) -> *mut c_void {
    // SAFETY: the caller's contract.
    unsafe { create_pipeline_library(d3d12device, threads, class::ALL) }
}

/// The layouts of the decoder on a device, and its pipelines: the states of the classes `prepare` are
/// compiled on `threads` threads (6 at most) while the call returns, or by the call with 0
/// ([`av1_wait_pipeline_cache_handle`] waits for them); every other state when a decoder needs it.
/// Returns null when a layout, or without threads a prepared pipeline, could not be created.
///
/// # Safety
/// `d3d12device` is null or an `ID3D12Device *` that lives until the handle is destroyed.
pub(crate) unsafe fn create_pipeline_library(d3d12device: *mut c_void, threads: c_int, prepare: u32) -> *mut c_void {
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

    // The shader-pick experiment: log the compiler options of both sets and the device's pick.
    for line in shader_pick_lines() {
        log(false, &line);
    }

    let test = PREPARE_TEST.load(Ordering::Relaxed);
    let shader_lib = Box::into_raw(Box::new(ShaderLib {
        // SAFETY: the library is interfaces (none), pointers, numbers and flags; all zero is the empty one.
        lib: unsafe { std::mem::zeroed() },
        tasks: Vec::new(),
        scheduler: None,
        ready_classes: AtomicU32::new(0),
        batch: Vec::new(),
        test,
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
            // SAFETY: the handle of this call, without a schedule.
            unsafe { av1_destroy_pipeline_cache_handle(handle) };
            return std::ptr::null_mut();
        }
    };

    let threads = threads.clamp(0, MAX_CREATE_THREADS);
    let prepare = if test != 0 { 0 } else { prepare & class::ALL };
    // SAFETY: the new library; nothing else has the handle yet. The slots get their layout, bytecode and
    // place now, so a dispatch can find a state that is compiled later.
    unsafe {
        for (index, task) in tasks.iter().enumerate() {
            (*task.dst).bytecode = task.bytecode.as_ptr();
            (*task.dst).size = task.bytecode.len();
            com_store(&raw mut (*task.dst).signaturePtr, task.layout.clone().into_raw());
            (*task.dst).index = index as u32;
            (*task.dst).library = handle;
        }
        let names: Vec<&'static str> = tasks.iter().map(|task| task.name).collect();
        let count = tasks.len();
        (*shader_lib).tasks = tasks;
        (*shader_lib).batch = slots_of(&*shader_lib, prepare);
        // The device for the threads of the schedule.
        let (target, owned): (LibPtr, gpu::OwnedDevice) = (LibPtr(shader_lib), device.owned());
        // SAFETY (of the call in the closure): the library outlives its schedule, which is closed before
        // it is released, and the schedule gives a slot to one thread.
        let compile = Box::new(move |slot: usize| compile_slot(target, owned.device(), slot));
        let report = Box::new(move |report: &Report| log_report(&names, report));
        (*shader_lib).scheduler = Some(Scheduler::new(count, threads as usize, compile, report));
        if prepare != class::ALL {
            log(
                false,
                &format!(
                    "PIPELINE prepare: {} of {count} states ({}){}; the others when a decoder needs them",
                    (*shader_lib).batch.len(),
                    class::names(prepare),
                    match test {
                        0 => "",
                        1 => ", nothing (-preparetest 1)",
                        _ => ", nothing (-preparetest 2: every state at its first dispatch)",
                    }
                ),
            );
        }
        let shader_lib = &*shader_lib;
        if shader_lib.batch.is_empty() {
            return handle;
        }
        if threads == 0 {
            if scheduler(shader_lib).wait(&shader_lib.batch, Some("states")) != 0 {
                av1_destroy_pipeline_cache_handle(handle);
                return std::ptr::null_mut();
            }
        } else {
            scheduler(shader_lib).queue(&shader_lib.batch, Priority::Batch, Some("states"));
        }
    }
    handle
}

/// `av1_wait_pipeline_cache_handle` (`aom/aom_decoder.h`): waits for the states the device prepared; 0
/// when all were created.
///
/// # Safety
/// `handle` is null or a handle of [`av1_create_pipeline_cache_handle`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn av1_wait_pipeline_cache_handle(handle: *mut c_void) -> c_int {
    // SAFETY: the caller's contract.
    unsafe { wait_shader_create_complete(handle.cast()) }
}

/// `av1_destroy_pipeline_cache_handle` (`aom/aom_decoder.h`): the schedule stops (a state that is being
/// compiled is finished, nothing queued is begun), then the states and layouts are released.
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
        if let Some(scheduler) = &(*handle.cast::<ShaderLib>()).scheduler {
            scheduler.close();
        }
        release(handle.cast());
        drop(Box::from_raw(handle.cast::<ShaderLib>()));
    }
}

// ---------------------------------------------------------------------------------------------------------
// The constant tables of the shaders.
// ---------------------------------------------------------------------------------------------------------

fn write_ints(bytes: &mut [u8], values: impl Iterator<Item = i32>) {
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        *chunk = value.to_ne_bytes();
    }
}

/// The masks of compound prediction with a wedge and of inter-intra prediction, for luma blocks and then
/// for their chroma blocks of half the size, as `inter_compound_masked.hlsl` and the other mask readers
/// index them. `offsets[bsize]` gets where the masks of a block size start, luma and chroma, in units of
/// 64 bytes.
fn fill_mask_lut(ptr: &mut [u8], offsets: &mut [[c_int; 2]; BLOCK_SIZES_ALL as usize]) {
    let mut offset = 0usize;
    // The inter-intra weights of a block: all 32; by row; by column; by the smaller of row and column.
    let inter_intra = |ptr: &mut [u8], offset: &mut usize, bw: usize, bh: usize, size: usize, scale: usize| {
        let weights = &II_WEIGHTS1D;
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
    // The masks of a block size with wedge bits: 16 wedges with two signs, bw * bh bytes each.
    let wedges = WedgeMasks::get();
    for bsize in 0..BLOCK_SIZES_ALL as usize {
        if WedgeMasks::bits(bsize) == 0 {
            continue;
        }
        let bw = BLOCK_SIZE_WIDE[bsize] as usize;
        let bh = BLOCK_SIZE_HIGH[bsize] as usize;
        let size = bw * bh;
        offsets[bsize][0] = ((offset + 63) >> 6) as c_int;
        for index in 0..16 {
            for sign in 0..2 {
                ptr[offset..offset + size].copy_from_slice(&wedges.mask(bsize, sign, index));
                offset += size;
            }
        }
        let scale = II_SIZE_SCALES[bsize] as usize;
        inter_intra(ptr, &mut offset, bw, bh, size, scale);
    }
    // Chroma: a mask averages 2x2 bytes of the luma mask of 2bw * 2bh bytes.
    for bsize in 0..BLOCK_SIZES_ALL as usize {
        if WedgeMasks::bits(bsize) == 0 {
            continue;
        }
        let bw = (BLOCK_SIZE_WIDE[bsize] >> 1) as usize;
        let bh = (BLOCK_SIZE_HIGH[bsize] >> 1) as usize;
        let size = (bw * bh).max(64);
        offsets[bsize][1] = ((offset + 63) >> 6) as c_int;
        for index in 0..16 {
            for sign in 0..2 {
                let mask = wedges.mask(bsize, sign, index);
                let at = |x: usize, y: usize| u32::from(mask[x + y * bw * 2]);
                for y in 0..bh {
                    for x in 0..bw {
                        let sum =
                            at(x * 2, y * 2) + at(x * 2 + 1, y * 2) + at(x * 2, y * 2 + 1) + at(x * 2 + 1, y * 2 + 1);
                        ptr[offset + x + y * bw] = ((sum + 2) >> 2) as u8;
                    }
                }
                offset += size;
            }
        }
        let scale = II_SIZE_SCALES[SS_SIZE_420[bsize] as usize] as usize;
        inter_intra(ptr, &mut offset, bw, bh, size, scale);
    }
}

/// `av1_upload_luts` of the C++: fills the prediction masks and warp filter tables and the offsets of
/// the masks in `Av1Core`. Returns 0 or the `HRESULT` of the call that failed.
///
/// # Safety
/// `dec` is the decoder with its device, its direct queue and both buffers, created as
/// `DeviceOnlyConst`.
pub(crate) unsafe fn upload_luts(dec: *mut Av1Core) -> i32 {
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
    let (Some(masks), Some(warp)) = (buffer(dec.inter_mask_lut), buffer(dec.inter_warp_filter)) else {
        return E_POINTER.0;
    };

    let mut fill_warp = |bytes: &mut [u8]| {
        write_ints(bytes, WARPED_FILTER.iter().flatten().map(|&tap| i32::from(tap)));
    };
    let wedge_offsets = &mut dec.wedge_offsets;
    let mut fill_masks = |bytes: &mut [u8]| {
        fill_mask_lut(bytes, wedge_offsets);
    };
    let mut uploads = [
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

    /// The names of the pipelines of `create_tasks`, in its order.
    fn listed_names() -> Vec<&'static str> {
        let source = include_str!("compute.rs");
        let list = &source[source.find("let idct = |tx: c_int|").unwrap()..source.find("list.into_iter()").unwrap()];
        list.lines()
            .filter_map(|line| line.trim().trim_start_matches('(').strip_prefix('"'))
            .map(|rest| &rest[..rest.find('"').unwrap()])
            .collect()
    }

    /// The classes of the states: every state is of one, a bit depth's states have their twin of the
    /// other depth, and the counts are those docs/integration.md gives ("Preparing the pipeline states").
    #[test]
    fn every_class_has_its_states() {
        let names = listed_names();
        let count = |class: u32| names.iter().filter(|name| class_of(name) == class).count();
        assert_eq!(count(class::CORE), 38);
        assert_eq!((count(class::DEPTH_8), count(class::DEPTH_10)), (17, 17));
        assert_eq!((count(class::SCALED_8), count(class::SCALED_10)), (8, 8));
        assert_eq!((count(class::FILM_GRAIN), count(class::SUPERRES)), (3, 2));
        assert_eq!((count(class::DISPLAY_PQ), count(class::DISPLAY_SDR), count(class::DISPLAY_HLG)), (1, 1, 1));
        let classes = [
            class::CORE,
            class::DEPTH_8,
            class::DEPTH_10,
            class::SCALED_8,
            class::SCALED_10,
            class::FILM_GRAIN,
            class::SUPERRES,
            class::DISPLAY_PQ,
            class::DISPLAY_SDR,
            class::DISPLAY_HLG,
        ];
        assert_eq!(classes.iter().map(|&class| count(class)).sum::<usize>(), names.len());
        assert_eq!(classes.iter().fold(0, |all, class| all | class), class::ALL);
        for name in &names {
            let class = class_of(name);
            if class == class::DEPTH_8 || class == class::SCALED_8 {
                let twin = format!("{name}_hbd");
                assert!(names.contains(&twin.as_str()), "{name} has its twin {twin}");
                assert_eq!(class_of(&twin), class << 1, "{twin} is of the other depth");
            }
        }
        assert_eq!(class::names(class::DEPTH_10 | class::DISPLAY_PQ), "10-bit, display PQ");
        assert_eq!(class::names(0), "none");
    }

    #[test]
    fn every_pipeline_names_its_shader_once() {
        let names = listed_names();
        let embedded = shader_names();
        assert_eq!(names.len(), embedded.len());
        assert_eq!(names.len(), SHADERS, "one pipeline per slot of the library");
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        for name in names {
            assert!(bytecode(name).is_some(), "the table has the bytecode of {name}");
        }
    }
}

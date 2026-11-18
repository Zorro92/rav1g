// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! GPU prediction lists (-parseopt gpublists). The group rows and rank entries follow tile
//! and parse order, even when a tile's mode info occupies several pieces of the pool.

use super::core::{buffer, commands, layout, perf_gpu_mark, pipeline};
use super::types::{
    Av1Core, InterCountsAll, InterFastClasses, InterFastLists, InterSizesAllCommon, ReconstructBlockSizes,
    av1_frame_thread_data, compute_shader_lib,
};
use crate::gpu::d3d12::DispatchSignature;
use crate::gpu::{BufferRef, BufferState, Commands, Device, Error};
use crate::sys::{AV1_PERF_GPU_GEN_BLOCKS, AV1_PERF_GPU_PRED_COUNT, AV1_PERF_GPU_PRED_SCAN, av1_tile_data};

// pred_list_common.h
pub(super) const GROUP_SIZE: u32 = 64;
pub(super) const WARP_LIST: u32 = (InterCountsAll + ReconstructBlockSizes) as u32;
pub(super) const FAST_LIST: u32 = WARP_LIST + InterSizesAllCommon as u32;
const RANK_BYTES: u32 = 40;
const HEADER_BYTES: u32 = 16;
const TEMPLATE_BYTES: u32 = 16;
const MAX_BATCHES: u32 = 12 + (InterFastClasses + InterFastClasses * InterFastClasses) as u32;

pub(super) fn enabled(parser_options: i32) -> bool {
    parser_options as u32 & crate::sys::AV1_PARSER_OPTION_GPU_PRED_LISTS != 0
}

pub(super) fn inter_fast(td: &av1_frame_thread_data, tiles: &[av1_tile_data]) -> bool {
    td.inter_fast != 0 && tiles.first().is_some_and(|tile| !tile.gen_block_map_fast.is_null())
}

#[derive(Clone, Copy)]
pub(super) struct FrameLayout {
    pub groups: u32,
    pub lists: u32,
    batches: u32,
    templates: u32,
}

impl FrameLayout {
    fn new(groups: u32, fast: bool, intra_dispatches: u32) -> Self {
        let lists = FAST_LIST + if fast { InterFastLists as u32 } else { 0 };
        Self { groups, lists, batches: MAX_BATCHES + intra_dispatches, templates: lists + intra_dispatches }
    }

    pub(super) fn for_frame(td: &av1_frame_thread_data, tiles: &[av1_tile_data]) -> Option<Self> {
        if !tiles.iter().any(|tile| tile.gpu_pred_lists != 0 && (tile.have_inter != 0 || tile.have_recon != 0)) {
            return None;
        }
        // A nonfinal piece is full and its length is a multiple of GROUP_SIZE.
        // Joining adjacent pieces therefore gives the same group count.
        let groups = td
            .pieces
            .iter()
            .take(tiles.len())
            .flat_map(|pieces| &pieces.mode_info)
            .map(|run| (run.count as u32).div_ceil(GROUP_SIZE))
            .sum();
        let iters = tiles.iter().map(|t| t.gen_intra_iter_y.max(t.gen_intra_iter_uv)).max().unwrap_or(-1);
        let intra_dispatches = if td.intra_v2 != 0 { td.intra_v2_list_count as u32 } else { 2 * (iters + 1) as u32 };
        Some(Self::new(groups, inter_fast(td, tiles), intra_dispatches))
    }

    pub(super) fn worst(mi: u32, max_tiles: u32, fast: bool, intra_v2: bool) -> Self {
        // Pieces except the last of a tile contain whole groups (mode info chunks of 256).
        // For the wavefront use the maximum size of the block map. A V2 dispatch consumes
        // at least one region entry, and there are at most as many of those as cells.
        let intra = if intra_v2 { mi + mi / 4 } else { 2 * (mi / 4 + 1) };
        Self::new(mi.div_ceil(GROUP_SIZE) + max_tiles, fast, intra)
    }

    pub(super) fn rank_offset(self) -> u32 {
        self.groups * self.lists * 4
    }

    pub(super) fn starts_offset(self) -> u32 {
        self.rank_offset() + self.groups * GROUP_SIZE * RANK_BYTES
    }

    fn counts_offset(self) -> u32 {
        self.starts_offset() + HEADER_BYTES + self.lists * 4
    }

    fn args_offset(self) -> u32 {
        self.counts_offset() + self.batches * 4
    }

    fn template_offset(self) -> u32 {
        (1 + self.batches) * TEMPLATE_BYTES
    }

    pub(super) fn scratch_bytes(self) -> usize {
        (self.args_offset() + self.templates * DispatchSignature::BYTES as u32) as usize
    }

    pub(super) fn template_bytes(self) -> usize {
        (self.template_offset() + self.templates * TEMPLATE_BYTES) as usize
    }
}

/// Root constants written by the scan shader before each indirect dispatch. For intra the
/// word to patch with the intra base is relative to the four words in the argument.
#[derive(Clone, Copy)]
pub(super) enum DispatchKind {
    Inter,
    Recon,
    Intra,
    Filter,
    IntraV2,
}

pub(super) struct Signatures {
    inter: DispatchSignature,
    recon: DispatchSignature,
    intra: DispatchSignature,
    filter: DispatchSignature,
    intra_v2: DispatchSignature,
}

impl Signatures {
    pub(super) fn new(device: Device<'_>, lib: &compute_shader_lib) -> Result<Self, Error> {
        // SAFETY: the pipeline cache has successfully created these layouts.
        unsafe {
            let inter = layout(&lib.sig_inter_pred);
            let intra = layout(&lib.sig_intra_pred);
            let intra_v2 = layout(&lib.sig_intra_v2);
            Ok(Self {
                inter: device.create_dispatch_signature(inter, 7, 0)?,
                recon: device.create_dispatch_signature(intra, 5, 0)?,
                intra: device.create_dispatch_signature(intra, 5, 6)?,
                filter: device.create_dispatch_signature(intra, 5, 0)?,
                intra_v2: device.create_dispatch_signature(intra_v2, 7, 4)?,
            })
        }
    }
}

/// The scan is recorded before `gen_pred_blocks`. The host fills the templates as it
/// records prediction, before the list can be submitted. Each template buffer belongs to
/// the slot. Scratch and the indirect argument buffer are shared on the decode queue.
pub(super) struct Recording<'a> {
    frame: FrameLayout,
    scratch: BufferRef<'a>,
    upload: &'a mut [u32],
    signatures: &'a Signatures,
    batches: u32,
    used: u32,
}

impl<'a> Recording<'a> {
    /// # Safety
    /// The decoder is recording the parsed frame, whose stage buffers were sized for `frame`.
    /// The slot's mapped template memory is exclusive to this recording until submission.
    pub(super) unsafe fn new(dec: *mut Av1Core, frame: FrameLayout) -> Self {
        // SAFETY: the caller's contract.
        unsafe {
            let object = &*(*(*dec).curr_frame_data).gpu_pred_templates;
            let upload = std::slice::from_raw_parts_mut(object.host_ptr.cast(), frame.template_bytes() / 4);
            upload[0] = 0;
            Self {
                frame,
                scratch: buffer((*dec).gpu_pred_scratch),
                upload,
                signatures: (*dec).gpu_pred_signatures.as_ref().expect("GPU prediction command signatures"),
                batches: 0,
                used: 0,
            }
        }
    }

    /// # Safety
    /// As for `new`; the group counts and ranks are already on the command list.
    pub(super) unsafe fn scan(&self, dec: *mut Av1Core, intra_records: u32) {
        // SAFETY: the caller's contract.
        unsafe {
            let commands = commands(dec);
            commands.write_barrier(self.scratch);
            perf_gpu_mark(dec, AV1_PERF_GPU_PRED_COUNT);
            let lib = (*dec).shader_lib;
            let shader = pipeline(&raw const (*lib).shader_pred_list_scan);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_read(0, buffer((*(*dec).curr_frame_data).gpu_pred_templates).location());
            commands.bind_write(1, self.scratch.location());
            commands.set_inline_constants(
                2,
                &[
                    self.frame.groups,
                    self.frame.lists,
                    self.frame.starts_offset(),
                    self.frame.counts_offset(),
                    self.frame.args_offset(),
                    self.frame.template_offset(),
                    intra_records,
                ],
                0,
            );
            commands.dispatch(1, 1, 1);
            // GENERIC_READ includes SRV, indirect arguments and copy source (for stage dumps).
            commands.transition(self.scratch, BufferState::ReadWrite, BufferState::Read);
            perf_gpu_mark(dec, AV1_PERF_GPU_PRED_SCAN);
        }
    }

    /// Account for the grid pass, if present, before timing the list passes.
    pub(super) unsafe fn mark_grid(dec: *mut Av1Core) {
        // SAFETY: the caller is recording the frame's command list.
        unsafe {
            if (*dec).gpu_grid != 0 || (*dec).gpu_grid_check != 0 {
                perf_gpu_mark(dec, AV1_PERF_GPU_GEN_BLOCKS);
            }
        }
    }

    pub(super) fn scratch(&self) -> BufferRef<'a> {
        self.scratch
    }

    pub(super) fn batch(
        &mut self,
        commands: Commands<'_>,
        kind: DispatchKind,
        groups: u32,
        entries: impl IntoIterator<Item = [u32; 4]>,
    ) {
        let first = self.used;
        let template = self.frame.template_offset() as usize / 4;
        for entry in entries {
            let start = template + self.used as usize * 4;
            self.upload[start..start + 4].copy_from_slice(&entry);
            self.used += 1;
        }
        let count = self.used - first;
        assert!(self.batches < self.frame.batches && count > 0);
        let (signature, patch) = match kind {
            DispatchKind::Inter => (&self.signatures.inter, 0),
            DispatchKind::Recon => (&self.signatures.recon, 0),
            DispatchKind::Intra => (&self.signatures.intra, 4),
            DispatchKind::Filter => (&self.signatures.filter, 2),
            DispatchKind::IntraV2 => (&self.signatures.intra_v2, 3),
        };
        let desc = 4 + self.batches as usize * 4;
        self.upload[desc..desc + 4].copy_from_slice(&[first, count, patch, groups]);
        commands.dispatch_indirect(
            signature,
            count,
            self.scratch,
            self.frame.args_offset() + first * DispatchSignature::BYTES as u32,
            self.frame.counts_offset() + self.batches * 4,
        );
        self.batches += 1;
    }

    pub(super) fn finish(self, commands: Commands<'_>) {
        self.upload[0] = self.batches;
        commands.transition(self.scratch, BufferState::Read, BufferState::ReadWrite);
    }
}

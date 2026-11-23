// SPDX-License-Identifier: BSD-2-Clause

//! `-steps`: schedules of the intra wavefront modelled from the transform blocks of a frame (the
//! blocks `host::step_time::place` records, in decode order). The decoder runs one dispatch and one
//! barrier per wavefront iteration (AV1 7.11.2: a block is predicted from the samples above and to the
//! left of it, so it waits for the blocks it reads); a region schedule gives every region of the frame one
//! thread group per step, which runs the region's blocks of the step in passes behind group barriers. A
//! block's step is above the step of every block it reads in another region, and its pass above the pass
//! of every block it reads in its own region in the same step.

use std::collections::HashMap;
use std::ffi::c_int;

use crate::host::step_time::Placed;

/// Threads of the main shader a region's thread group has (two slots of 256), and filter intra blocks
/// it runs side by side (32 threads each).
const GROUP_THREADS: i64 = 512;
const GROUP_FILTERS: i64 = 16;

/// What a schedule of one frame comes to.
#[derive(Default, Clone, Copy)]
pub struct Cost {
    /// Dispatch steps (barriers).
    pub steps: i64,
    /// Over the steps, the most chunks of a group of the step: a chunk is one pass of the main shader's
    /// body over up to 512 threads, or of filter intra over up to 16 blocks.
    pub chunks: i64,
    /// Thread groups over all steps.
    pub groups: i64,
    /// Over the steps, the most passes of a group of the step.
    pub passes: i64,
}

#[derive(Clone, Copy)]
struct Block {
    step: i32,
    pass: i32,
    region: i32,
    iter: i32,
}

struct Plane {
    width: i32,
    height: i32,
    cells: Vec<i32>,
}

impl Plane {
    fn new(width: i32, height: i32) -> Self {
        Self { width, height, cells: vec![-1; (width * height) as usize] }
    }
    fn get(&self, col: i32, row: i32) -> Option<usize> {
        if col < 0 || row < 0 || col >= self.width || row >= self.height {
            return None;
        }
        let at = self.cells[(col + row * self.width) as usize];
        (at >= 0).then_some(at as usize)
    }
    fn set(&mut self, col: i32, row: i32, w: i32, h: i32, block: usize) {
        for r in row.max(0)..(row + h).min(self.height) {
            for c in col.max(0)..(col + w).min(self.width) {
                self.cells[(c + r * self.width) as usize] = block as i32;
            }
        }
    }
}

/// The blocks each transform block reads, as the host's wavefront reads them (`wavefront_iteration_edges`).
fn reads(planes: &[Plane; 2], b: &Placed, out: &mut Vec<usize>) {
    out.clear();
    let plane = &planes[usize::from(b.chroma)];
    let [above_left, above, left] = b.reads;
    if above_left != 0 && above >= 0 {
        out.extend(plane.get(b.col - 1, b.row - 1));
    }
    for i in 0..above.max(0) {
        out.extend(plane.get(b.col + i, b.row - 1));
    }
    for i in 0..left.max(0) {
        out.extend(plane.get(b.col - 1, b.row + i));
    }
    if b.cfl {
        let luma = &planes[0];
        for r in 0..(2 << b.txh) {
            for c in 0..(2 << b.txw) {
                out.extend(luma.get(2 * b.col + c, 2 * b.row + r));
            }
        }
    }
}

/// The frame's blocks under the decoder's schedule (`None`) and under regions of `1 << rw` x `1 << rh`
/// luma units (4x4) each. Returns the cost and the number of blocks whose iteration the model does not
/// reproduce from what they read (0 when the reads are those of the host).
pub fn model(blocks: &[Placed], mi_cols: c_int, mi_rows: c_int, region: Option<(i32, i32)>, cap: i32) -> (Cost, usize) {
    let mut planes =
        [Plane::new(mi_cols + 32, mi_rows + 32), Plane::new((mi_cols + 1) / 2 + 16, (mi_rows + 1) / 2 + 16)];
    let (rw, rh) = region.unwrap_or((30, 30));
    let region_cols = ((mi_cols + 32) >> rw) + 1;
    let mut current: HashMap<i32, i32> = HashMap::new();
    let mut state: Vec<Block> = Vec::with_capacity(blocks.len());
    // (step, region) -> per pass (threads, filter blocks)
    let mut entries: HashMap<(i32, i32), Vec<(i64, i64)>> = HashMap::new();
    let mut deps = Vec::new();
    let mut wrong = 0;
    let mut top_step = -1;
    let mut top_iter = -1;
    for b in blocks {
        reads(&planes, b, &mut deps);
        let shift = i32::from(b.chroma);
        let r = (b.col >> (rw - shift)) + ((b.row >> (rh - shift)) * region_cols);
        let mut iter = deps.iter().map(|&d| state[d].iter).max().unwrap_or(-1) + 1;
        if b.intrabc {
            iter = top_iter + 1;
        }
        if iter != b.iter {
            wrong += 1;
        }
        let mut need = 0;
        for &d in &deps {
            if state[d].region != r {
                need = need.max(state[d].step + 1);
            }
        }
        if b.intrabc {
            need = top_step + 1;
        }
        let cur = current.entry(r).or_insert(-1);
        let mut step = need.max(*cur);
        let mut pass = 0;
        for &d in &deps {
            if state[d].region == r && state[d].step == step {
                pass = pass.max(state[d].pass + 1);
            }
        }
        // A region's group runs at most `cap` passes in a step; a longer chain goes on in the next step.
        if pass >= cap {
            step += 1;
            pass = 0;
        }
        *cur = step;
        if region.is_none() {
            // The decoder: one step per iteration.
            let step = b.iter;
            state.push(Block { step, pass: 0, region: r, iter: b.iter });
        } else {
            state.push(Block { step, pass, region: r, iter: b.iter });
        }
        let s = *state.last().expect("pushed");
        top_step = top_step.max(s.step);
        top_iter = top_iter.max(b.iter);
        let passes = entries.entry((s.step, if region.is_none() { 0 } else { r })).or_default();
        if passes.len() <= s.pass as usize {
            passes.resize(s.pass as usize + 1, (0, 0));
        }
        if b.filter {
            passes[s.pass as usize].1 += 1;
        } else {
            // One thread per four samples of a row; U and V are two records.
            passes[s.pass as usize].0 += (4i64 << (b.txw + b.txh)) * (1 + i64::from(b.chroma));
        }
        planes[usize::from(b.chroma)].set(b.col, b.row, 1 << b.txw, 1 << b.txh, state.len() - 1);
    }
    let mut by_step: HashMap<i32, (i64, i64, i64)> = HashMap::new();
    let mut cost = Cost::default();
    for ((step, _), passes) in &entries {
        let (chunks, groups) = if region.is_none() {
            let (t, f) = passes[0];
            (1, (t + 255) / 256 + f)
        } else {
            (
                passes
                    .iter()
                    .map(|&(t, f)| (t + GROUP_THREADS - 1) / GROUP_THREADS + (f + GROUP_FILTERS - 1) / GROUP_FILTERS)
                    .sum(),
                1,
            )
        };
        let e = by_step.entry(*step).or_default();
        e.0 = e.0.max(chunks);
        e.1 += groups;
        e.2 = e.2.max(passes.len() as i64);
    }
    cost.steps = by_step.len() as i64;
    for (chunks, groups, passes) in by_step.values() {
        cost.chunks += chunks;
        cost.groups += groups;
        cost.passes += passes;
    }
    (cost, wrong)
}

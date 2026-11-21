// SPDX-License-Identifier: BSD-2-Clause
//
// The generation of the grain templates is `generate_grain_y_rust` and `generate_grain_uv_rust` of
// `src/filmgrain.rs` of rav1d (https://github.com/memorysafety/rav1d, commit 2e7216c, the commit
// `third_party/rav1d/UPSTREAM` names), the Rust port of `generate_grain_y_c` and `generate_grain_uv_c` of
// dav1d's `src/filmgrain_tmpl.c`. Its licence:
//
// Copyright © 2018-2019, VideoLAN and dav1d authors
// Copyright © 2023-2024, VideoLAN, dav1d authors, and Internet Security Research Group
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this
//    list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
// ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
// WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
// ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
// (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
// LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
// ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! The grain templates of a frame, made on the CPU (AV1 7.18.3.3, generate grain process): the white
//! noise of the random number process (7.18.3.2) through the Gaussian sequence, then the
//! auto-regressive filter. 73 rows of 82 samples for luma, 38 rows of 44 for each chroma plane (4:2:0,
//! the only chroma layout the decoder has).
//!
//! The filter is a recurrence through the whole template: every sample reads the filtered samples to
//! its left and in the rows above. At lag 3 this takes about 0.2 million multiply-adds per frame. The
//! independent terms from the rows above are computed first so they can be vectorized ([`Filter`]).
//!
//! What differs from rav1d's code is the order of the sums of the filter ([`Filter`], the same values),
//! and what the specification says and dav1d leaves out because those
//! templates are not read: a plane without scaling points has the template 0 (7.18.3.3: "g = 0", and the
//! filter of zeros is zeros; dav1d makes the noise and filters it), and the luma term of the chroma
//! filter is only there with luma points (7.18.3.3, `num_y_points > 0`).

use std::ffi::c_int;

use super::tables::GAUSSIAN_SEQUENCE;
use crate::sys::aom_film_grain_t;

pub(super) const GRAIN_WIDTH: usize = 82;
pub(super) const GRAIN_HEIGHT: usize = 73;
const SUB_GRAIN_WIDTH: usize = 44;
const SUB_GRAIN_HEIGHT: usize = 38;
const AR_PAD: usize = 3;

/// The templates of a frame in the layout the apply shaders read them (`film_grain_filter*.hlsl`):
/// luma, rows of 82 ints, then Cb and Cr, rows of 44 ints each, with nothing between them.
#[repr(C)]
pub(super) struct Templates {
    pub luma: [[c_int; GRAIN_WIDTH]; GRAIN_HEIGHT],
    pub chroma: [[[c_int; SUB_GRAIN_WIDTH]; SUB_GRAIN_HEIGHT]; 2],
}

/// The bytes of [`Templates`]: what a grain frame uploads.
pub(super) const TEMPLATE_BYTES: usize = size_of::<Templates>();
const _: () = assert!(TEMPLATE_BYTES == 4 * (GRAIN_WIDTH * GRAIN_HEIGHT + 2 * SUB_GRAIN_WIDTH * SUB_GRAIN_HEIGHT));

/// Where the chroma templates begin, in ints from the start of [`Templates`].
pub(super) const CHROMA_OFFSET_INTS: [usize; 2] =
    [GRAIN_WIDTH * GRAIN_HEIGHT, GRAIN_WIDTH * GRAIN_HEIGHT + SUB_GRAIN_WIDTH * SUB_GRAIN_HEIGHT];

impl Templates {
    pub(super) const fn zeroed() -> Templates {
        Templates { luma: [[0; GRAIN_WIDTH]; GRAIN_HEIGHT], chroma: [[[0; SUB_GRAIN_WIDTH]; SUB_GRAIN_HEIGHT]; 2] }
    }

    /// The three templates of the parameters (7.18.3.3). With AVX2 (the console's CPU has it) the
    /// same code compiled for it: the sums of the rows above in 8 lanes (0.07 against 0.11 ms a frame
    /// at lag 3 on a laptop CPU; the library is built for plain x86-64).
    pub(super) fn generate(&mut self, data: &aom_film_grain_t) {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU has AVX2.
            unsafe { self.generate_avx2(data) };
            return;
        }
        self.generate_any(data);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    fn generate_avx2(&mut self, data: &aom_film_grain_t) {
        self.generate_any(data);
    }

    #[inline(always)]
    fn generate_any(&mut self, data: &aom_film_grain_t) {
        generate_grain_y(&mut self.luma, data);
        let [cb, cr] = &mut self.chroma;
        generate_grain_uv(cb, &self.luma, data, false);
        generate_grain_uv(cr, &self.luma, data, true);
    }
}

/// The random number process (7.18.3.2).
#[inline]
fn get_random_number(bits: u8, state: &mut u32) -> c_int {
    let r = *state;
    let bit = (r ^ (r >> 1) ^ (r >> 3) ^ (r >> 12)) & 1;
    *state = (r >> 1) | bit << 15;

    (*state >> (16 - bits) & ((1 << bits) - 1)) as c_int
}

#[inline]
fn round2(x: c_int, shift: u8) -> c_int {
    (x + (1 << shift >> 1)) >> shift
}

/// The constants of the white noise and the filter: the shift of the Gaussian sequence and the range of
/// a grain sample.
fn noise_range(data: &aom_film_grain_t) -> (u8, c_int, c_int) {
    let bitdepth_min_8 = (data.bit_depth as u8).wrapping_sub(8) & 7;
    let shift = 4 - bitdepth_min_8 + (data.grain_scale_shift as u8 & 3);
    let grain_ctr = 128 << bitdepth_min_8;
    (shift, -grain_ctr, grain_ctr - 1)
}

#[inline(always)]
fn generate_grain_y(buf: &mut [[c_int; GRAIN_WIDTH]; GRAIN_HEIGHT], data: &aom_film_grain_t) {
    if data.num_y_points == 0 {
        *buf = [[0; GRAIN_WIDTH]; GRAIN_HEIGHT];
        return;
    }
    let mut seed = u32::from(data.random_seed);
    let (shift, grain_min, grain_max) = noise_range(data);

    for row in &mut buf[..GRAIN_HEIGHT] {
        row[..GRAIN_WIDTH].fill_with(|| {
            let value = get_random_number(11, &mut seed);
            round2(GAUSSIAN_SEQUENCE[value as usize], shift)
        });
    }

    let filter = Filter { coeffs: &data.ar_coeffs_y, shift: data.ar_coeff_shift as u8 & 15, grain_min, grain_max };
    // `ar_lag` is 2 bits: one of the four filters.
    match data.ar_coeff_lag & 3 {
        0 => filter.run::<GRAIN_WIDTH, GRAIN_HEIGHT, 0>(buf, |_, _| {}),
        1 => filter.run::<GRAIN_WIDTH, GRAIN_HEIGHT, 1>(buf, |_, _| {}),
        2 => filter.run::<GRAIN_WIDTH, GRAIN_HEIGHT, 2>(buf, |_, _| {}),
        _ => filter.run::<GRAIN_WIDTH, GRAIN_HEIGHT, 3>(buf, |_, _| {}),
    }
}

/// The template of Cb (`is_uv` false) or Cr, subsampled in both directions.
#[inline(always)]
fn generate_grain_uv(
    buf: &mut [[c_int; SUB_GRAIN_WIDTH]; SUB_GRAIN_HEIGHT],
    buf_y: &[[c_int; GRAIN_WIDTH]; GRAIN_HEIGHT],
    data: &aom_film_grain_t,
    is_uv: bool,
) {
    let num_points = if is_uv { data.num_cr_points } else { data.num_cb_points };
    if num_points == 0 && data.chroma_scaling_from_luma == 0 {
        *buf = [[0; SUB_GRAIN_WIDTH]; SUB_GRAIN_HEIGHT];
        return;
    }
    let mut seed = u32::from(data.random_seed) ^ if is_uv { 0x49d8 } else { 0xb524 };
    let (shift, grain_min, grain_max) = noise_range(data);

    for row in &mut buf[..SUB_GRAIN_HEIGHT] {
        row[..SUB_GRAIN_WIDTH].fill_with(|| {
            let value = get_random_number(11, &mut seed);
            round2(GAUSSIAN_SEQUENCE[value as usize], shift)
        });
    }

    let ar_lag = (data.ar_coeff_lag & 3) as usize;
    let coeffs = if is_uv { &data.ar_coeffs_cr } else { &data.ar_coeffs_cb };
    // The luma term: the average of the 2x2 luma samples at the place of the sample, times the
    // coefficient behind those of the chroma samples (7.18.3.3, only with luma points).
    let luma_coeff = coeffs[2 * ar_lag * (ar_lag + 1)];
    let with_luma = data.num_y_points > 0 && luma_coeff != 0;
    let luma_term = |y: usize, sums: &mut [c_int; SUB_GRAIN_WIDTH]| {
        if !with_luma {
            return;
        }
        let rows = &buf_y[((y - AR_PAD) << 1) + AR_PAD..][..2];
        for x in AR_PAD..SUB_GRAIN_WIDTH - AR_PAD {
            let luma_x = ((x - AR_PAD) << 1) + AR_PAD;
            let luma = rows[0][luma_x] + rows[0][luma_x + 1] + rows[1][luma_x] + rows[1][luma_x + 1];
            sums[x] = round2(luma, 2) * luma_coeff;
        }
    };
    let filter = Filter { coeffs, shift: data.ar_coeff_shift as u8 & 15, grain_min, grain_max };
    match ar_lag {
        0 => filter.run::<SUB_GRAIN_WIDTH, SUB_GRAIN_HEIGHT, 0>(buf, luma_term),
        1 => filter.run::<SUB_GRAIN_WIDTH, SUB_GRAIN_HEIGHT, 1>(buf, luma_term),
        2 => filter.run::<SUB_GRAIN_WIDTH, SUB_GRAIN_HEIGHT, 2>(buf, luma_term),
        _ => filter.run::<SUB_GRAIN_WIDTH, SUB_GRAIN_HEIGHT, 3>(buf, luma_term),
    }
}

/// The auto-regressive filter of a template (7.18.3.3): every sample from row 3 and column 3 on, up to 3
/// columns from the right, gets the rounded sum of its neighbours times the coefficients, in the order
/// of the specification: the `LAG` rows above it from `LAG` columns left to `LAG` right, then the `LAG`
/// samples to its left in its own row, all of them already filtered.
///
/// In rav1d's loop every sample sums all its neighbours; here a row is two passes. The rows above do
/// not change while a row is filtered, so their part of every sum of the row is added up first, sample
/// by sample along the row, without a dependence between the samples (which the compiler vectorizes).
/// Only the part of the samples to the left is then the recurrence, at most 3 multiply-adds a sample.
/// The sums are those of the integers, whatever their order: the same values as rav1d's and the
/// specification's (the largest is 25 x 127 x 512, far from overflow).
struct Filter<'a> {
    coeffs: &'a [c_int],
    shift: u8,
    grain_min: c_int,
    grain_max: c_int,
}

impl Filter<'_> {
    /// `extra` sets the start of the sums of a row (index: the row of the template) before the
    /// neighbours are added: the luma term of a chroma template.
    #[inline(always)]
    fn run<const W: usize, const H: usize, const LAG: usize>(
        &self,
        buf: &mut [[c_int; W]; H],
        extra: impl Fn(usize, &mut [c_int; W]),
    ) {
        const { assert!(LAG <= AR_PAD) };
        let taps = 2 * LAG + 1;
        let left = &self.coeffs[LAG * taps..][..LAG];
        for y in AR_PAD..H {
            let mut sums = [0; W];
            extra(y, &mut sums);
            let (above, rest) = buf.split_at_mut(y);
            for dy in 0..LAG {
                let row = &above[y - LAG + dy];
                for dx in 0..taps {
                    let c = self.coeffs[dy * taps + dx];
                    for x in AR_PAD..W - AR_PAD {
                        sums[x] += c * row[x + dx - LAG];
                    }
                }
            }
            let row = &mut rest[0];
            for x in AR_PAD..W - AR_PAD {
                let mut sum = sums[x];
                for dx in 0..LAG {
                    sum += left[dx] * row[x + dx - LAG];
                }
                row[x] = (row[x] + round2(sum, self.shift)).clamp(self.grain_min, self.grain_max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generate grain process of the specification (7.18.3.3), as it is written there, for 4:2:0.
    fn spec_templates(p: &aom_film_grain_t) -> Templates {
        fn get_random_number(bits: u32, reg: &mut u32) -> i32 {
            let r = *reg;
            let bit = (r ^ (r >> 1) ^ (r >> 3) ^ (r >> 12)) & 1;
            *reg = (r >> 1) | (bit << 15);
            ((*reg >> (16 - bits)) & ((1 << bits) - 1)) as i32
        }
        let mut out = Templates::zeroed();
        let round2 = |x: i32, n: i32| if n == 0 { x } else { (x + (1 << (n - 1))) >> n };
        let bit_depth = p.bit_depth as i32;
        let grain_center = 128 << (bit_depth - 8);
        let (grain_min, grain_max) = (-grain_center, (256 << (bit_depth - 8)) - 1 - grain_center);
        let shift = 12 - bit_depth + p.grain_scale_shift;
        let mut random_register = u32::from(p.random_seed);
        let mut luma = [[0i32; 82]; 73];
        for row in luma.iter_mut() {
            for v in row.iter_mut() {
                let g = if p.num_y_points > 0 {
                    GAUSSIAN_SEQUENCE[get_random_number(11, &mut random_register) as usize]
                } else {
                    0
                };
                *v = round2(g, shift);
            }
        }
        let ar_shift = p.ar_coeff_shift;
        let lag = p.ar_coeff_lag;
        for y in 3..73 {
            for x in 3..82 - 3 {
                let (mut s, mut pos) = (0, 0);
                'rows: for dr in -lag..=0 {
                    for dc in -lag..=lag {
                        if dr == 0 && dc == 0 {
                            break 'rows;
                        }
                        s += luma[(y + dr) as usize][(x + dc) as usize] * p.ar_coeffs_y[pos];
                        pos += 1;
                    }
                }
                luma[y as usize][x as usize] =
                    (luma[y as usize][x as usize] + round2(s, ar_shift)).clamp(grain_min, grain_max);
            }
        }
        let mut chroma = [[[0i32; 44]; 38]; 2];
        for (plane, seed_xor) in [(0usize, 0xb524u32), (1, 0x49d8)] {
            let num_points = if plane == 0 { p.num_cb_points } else { p.num_cr_points };
            let has = num_points > 0 || p.chroma_scaling_from_luma != 0;
            random_register = u32::from(p.random_seed) ^ seed_xor;
            for row in chroma[plane].iter_mut() {
                for v in row.iter_mut() {
                    let g =
                        if has { GAUSSIAN_SEQUENCE[get_random_number(11, &mut random_register) as usize] } else { 0 };
                    *v = round2(g, shift);
                }
            }
        }
        for y in 3..38 {
            for x in 3..44 - 3 {
                let (mut s0, mut s1, mut pos) = (0, 0, 0);
                'rows: for dr in -lag..=0 {
                    for dc in -lag..=lag {
                        let (c0, c1) = (p.ar_coeffs_cb[pos], p.ar_coeffs_cr[pos]);
                        if dr == 0 && dc == 0 {
                            if p.num_y_points > 0 {
                                let mut l = 0;
                                let (lx, ly) = (((x - 3) << 1) + 3, ((y - 3) << 1) + 3);
                                for i in 0..=1 {
                                    for j in 0..=1 {
                                        l += luma[(ly + i) as usize][(lx + j) as usize];
                                    }
                                }
                                l = round2(l, 2);
                                s0 += l * c0;
                                s1 += l * c1;
                            }
                            break 'rows;
                        }
                        s0 += chroma[0][(y + dr) as usize][(x + dc) as usize] * c0;
                        s1 += chroma[1][(y + dr) as usize][(x + dc) as usize] * c1;
                        pos += 1;
                    }
                }
                let (y, x) = (y as usize, x as usize);
                chroma[0][y][x] = (chroma[0][y][x] + round2(s0, ar_shift)).clamp(grain_min, grain_max);
                chroma[1][y][x] = (chroma[1][y][x] + round2(s1, ar_shift)).clamp(grain_min, grain_max);
            }
        }
        // A plane without points is never read (the note under 7.18.3.3); the decoder's template is 0.
        for (plane, num_points) in [p.num_cb_points, p.num_cr_points].into_iter().enumerate() {
            if num_points == 0 && p.chroma_scaling_from_luma == 0 {
                chroma[plane] = [[0; 44]; 38];
            }
        }
        out.luma = luma;
        out.chroma = chroma;
        out
    }

    /// A small generator of test parameters.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, below: u32) -> i32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) % u64::from(below)) as i32
        }
    }

    #[test]
    fn templates_follow_the_specification() {
        let mut rng = Lcg(0x5eed);
        for case in 0..400 {
            // SAFETY: all zero is a value of the parameters.
            let mut p: aom_film_grain_t = unsafe { std::mem::zeroed() };
            p.apply_grain = 1;
            p.bit_depth = if case % 2 == 0 { 8 } else { 10 };
            p.random_seed = rng.next(65536) as u16;
            p.ar_coeff_lag = rng.next(4);
            p.ar_coeff_shift = 6 + rng.next(4);
            p.grain_scale_shift = rng.next(4);
            p.num_y_points = if case % 7 == 3 { 0 } else { 1 + rng.next(14) };
            p.chroma_scaling_from_luma = i32::from(case % 5 == 1);
            p.num_cb_points = if case % 3 == 0 { 0 } else { 1 + rng.next(10) };
            p.num_cr_points = if case % 4 == 0 { 0 } else { 1 + rng.next(10) };
            let extreme = case % 11 == 0;
            let coeff = |rng: &mut Lcg| if extreme { [-128, 127][rng.next(2) as usize] } else { rng.next(256) - 128 };
            for c in p.ar_coeffs_y.iter_mut() {
                *c = coeff(&mut rng);
            }
            for c in p.ar_coeffs_cb.iter_mut().chain(p.ar_coeffs_cr.iter_mut()) {
                *c = coeff(&mut rng);
            }
            let expected = spec_templates(&p);
            let mut got = Templates::zeroed();
            // Stale values in the target: every sample is written.
            got.luma[40][40] = 12345;
            got.chroma[1][20][20] = -999;
            got.generate(&p);
            assert_eq!(got.luma, expected.luma, "luma, case {case}");
            assert_eq!(got.chroma, expected.chroma, "chroma, case {case}");
            // The code for plain x86-64 as well, which `generate` does not take on a CPU with AVX2.
            let mut plain = Templates::zeroed();
            plain.generate_any(&p);
            assert_eq!(plain.luma, expected.luma, "luma, plain, case {case}");
            assert_eq!(plain.chroma, expected.chroma, "chroma, plain, case {case}");
        }
    }
}

// SPDX-License-Identifier: BSD-2-Clause
//! The display pass prototype (docs/stage-fusion.md, option B; `-displaypass WxH`): the inline constants
//! of `display_pass.hlsl` and its two tables.
//!
//! The pass is bound by arithmetic (the console's 3.5 ms a 4K frame for a 31 MB pass, against ~0.8 ms of
//! traffic at the idle state's 40 GB/s), and most of it was PQ's transfer functions: the EOTF of three
//! channels and the BT.2390 EETF of the largest, seven `pow` pairs a source pixel. Both are functions of
//! one signal, so the host computes them once here and the shader interpolates: a cubic polynomial an
//! interval (Hermite, from the function's values and one-sided derivatives at the interval's ends, in
//! float64), 16 bytes an interval, within 1e-6 of the function (relative) wherever the light is above 1e-5
//! of SDR white: the float32 level of the model's own `pow`s, which matters for out-of-gamut pixels whose
//! BT.709 channels cancel to almost nothing before the 1/2.4 power. 5.5 KB in all, read into group
//! memory by every group of 64x64 output pixels (3 MB of cache reads a 1080p frame). The tables of a mode
//! are made once a process and copied into the frame's constant memory (5.5 KB a frame, the CPU's only
//! work for the pass).

use std::sync::Mutex;

use crate::sys::{HwFrameBuffer, SequenceHeader};

/// Intervals of the EOTF table: `EOTF_FINE` up to the signal `EOTF_SPLIT`, the rest above, and one more
/// that holds the value at 1 (the shader's position of a signal of 1 or more).
const EOTF_N: usize = 256;
const EOTF_FINE: usize = 64;
const EOTF_SPLIT: f64 = 1.0 / 32.0;
/// Intervals of the gain table: PQ: `GAIN_LOW` from the EETF's knee (below it the gain is 1 / the target)
/// to the source peak's signal (where the curve meets its clip), the rest to 1; HLG: all on log2 nits
/// from the knee's nits, one interval a step, so that 1000 nits is the end of interval `GAIN_N - 1`.
/// One more holds the value at the end.
const GAIN_N: usize = 96;
const GAIN_LOW: usize = 64;
/// Words of the tables: four an interval (the polynomial's coefficients), the EOTF's then the gain's.
pub(crate) const LUT_WORDS: usize = 4 * (EOTF_N + 1 + GAIN_N + 1);

/// Below this largest PQ signal the shader computes the peak's nits instead of interpolating them: the
/// gain is `eetf(peak) / max(peak, 1e-6)`, so below 1e-6 nits (a signal of 1.2e-4) it is proportional to
/// the peak, and pixels there with a large negative channel (out of gamut) show it.
const PQ_EXACT_BELOW: f32 = 4e-4;

/// The tone map's parameters, as the stage the pass stands in for has them: source peak 1000 nits, SDR
/// white 100 nits (the 203-nit endpoint of the EETF).
const SOURCE_PEAK: f64 = 1000.0;
const SDR_WHITE: f64 = 100.0;

// ST 2084 (m1 = 2610/16384, m2 = 2523/32, c1 = 3424/4096, c2 = 2413/128, c3 = 2392/128), in float64: the
// functions the model computes in float32, without its rounding.
const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 32.0;
const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 128.0;
const C3: f64 = 2392.0 / 128.0;

fn pq_to_nits(e: f64) -> f64 {
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1) * 10000.0
}

fn nits_to_pq(nits: f64) -> f64 {
    let p = (nits.clamp(0.0, 10000.0) / 10000.0).powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

fn target() -> f64 {
    (SDR_WHITE * 2.03).min(10000.0)
}

/// The BT.2390 EETF of the model: nits to the SDR range, 1 at the target (2.03 x white).
fn eetf(nits: f64) -> f64 {
    let nits = nits.clamp(0.0, SOURCE_PEAK);
    let target = target();
    if SOURCE_PEAK <= target {
        return (nits / target).min(1.0);
    }
    let peak = nits_to_pq(SOURCE_PEAK);
    let s = nits_to_pq(nits) / peak;
    let top = nits_to_pq(target) / peak;
    let knee = 2.0 * top - 1.0;
    let mut mapped = s;
    if s >= knee {
        let t = (s - knee) / (1.0 - knee);
        let t2 = t * t;
        let t3 = t2 * t;
        mapped = (2.0 * t3 - 3.0 * t2 + 1.0) * knee + (t3 - 2.0 * t2 + t) * (1.0 - knee) + (-2.0 * t3 + 3.0 * t2) * top;
    }
    (pq_to_nits(mapped * peak) / target).clamp(0.0, 1.0)
}

/// The gain the model gives a pixel whose largest channel has `nits` (above 1e-6 nits).
fn gain(nits: f64) -> f64 {
    eetf(nits) / nits.max(1e-6)
}

/// The signal from which the gain is not constant (the EETF's knee, or the target when the curve only
/// clips) and the signal where it meets its clip (the source peak's).
fn gain_range() -> (f64, f64) {
    if SOURCE_PEAK <= target() {
        let e = nits_to_pq(target());
        return (e, e);
    }
    let peak = nits_to_pq(SOURCE_PEAK);
    let knee = 2.0 * (nits_to_pq(target()) / peak) - 1.0;
    (knee * peak, peak)
}

/// The cubic of `f` on [x0, x1] in the shader's form a + t (b + t (c + t d)), t = 0..1: Hermite from the
/// values and the one-sided derivatives at the ends (second order differences inside the interval, so a
/// kink at an end does not count).
fn cubic(f: &dyn Fn(f64) -> f64, x0: f64, x1: f64) -> [u32; 4] {
    let h = x1 - x0;
    let e = h * 1e-4;
    let (v0, v1) = (f(x0), f(x1));
    let d0 = (-3.0 * v0 + 4.0 * f(x0 + e) - f(x0 + 2.0 * e)) / (2.0 * e) * h;
    let d1 = (3.0 * v1 - 4.0 * f(x1 - e) + f(x1 - 2.0 * e)) / (2.0 * e) * h;
    let c = 3.0 * (v1 - v0) - 2.0 * d0 - d1;
    let d = 2.0 * (v0 - v1) + d0 + d1;
    [(v0 as f32).to_bits(), (d0 as f32).to_bits(), (c as f32).to_bits(), (d as f32).to_bits()]
}

/// A table of `f` on the nodes `x(0) ..= x(n)`, and the constant value at `x(n)` behind it.
fn fill(words: &mut [u32], n: usize, x: &dyn Fn(usize) -> f64, f: &dyn Fn(f64) -> f64) {
    for k in 0..n {
        words[4 * k..4 * k + 4].copy_from_slice(&cubic(f, x(k), x(k + 1)));
    }
    words[4 * n..4 * n + 4].copy_from_slice(&[(f(x(n)) as f32).to_bits(), 0, 0, 0]);
}

/// The tables of a mode (0 PQ, 2 HLG; SDR has none) and the shader's constants for them: the EOTF
/// table's a, b, c (its position of a signal m is min(m a, m b + c)), and the gain table's two lines
/// (its position of x is min(x s1 + o1, x s2 + o2)).
struct Tables {
    words: Box<[u32; LUT_WORDS]>,
    eotf: [f32; 3],
    gain: [f32; 4],
}

fn tables(mode: u32) -> Tables {
    let mut words = Box::new([0u32; LUT_WORDS]);
    let a = EOTF_FINE as f64 / EOTF_SPLIT;
    let b = (EOTF_N - EOTF_FINE) as f64 / (1.0 - EOTF_SPLIT);
    let c = EOTF_FINE as f64 - EOTF_SPLIT * b;
    let (eotf_words, gain_words) = words.split_at_mut(4 * (EOTF_N + 1));
    let (knee, peak) = gain_range();
    let gain_lines = if mode == 2 {
        // HLG: the peak's log2 nits, one line: 1000 nits on a node, the table to 1032 nits (the OOTF of a
        // 1000-nit display ends at 1000.1).
        let u0 = pq_to_nits(knee).log2();
        let step = (SOURCE_PEAK.log2() - u0) / (GAIN_N - 1) as f64;
        fill(gain_words, GAIN_N, &|j| u0 + j as f64 * step, &|u| gain(u.exp2()));
        let s = 1.0 / step;
        [s, -u0 * s, s, -u0 * s]
    } else {
        let s1 = GAIN_LOW as f64 / (peak - knee);
        let s2 = (GAIN_N - GAIN_LOW) as f64 / (1.0 - peak);
        let x = |j: usize| if j < GAIN_LOW { knee + j as f64 / s1 } else { peak + (j - GAIN_LOW) as f64 / s2 };
        fill(gain_words, GAIN_N, &x, &|e| gain(pq_to_nits(e)));
        let x = |k: usize| if k < EOTF_FINE { k as f64 / a } else { (k as f64 - c) / b };
        fill(eotf_words, EOTF_N, &x, &pq_to_nits);
        [s1, -knee * s1, s2, GAIN_LOW as f64 - peak * s2]
    };
    Tables { words, eotf: [a as f32, b as f32, c as f32], gain: gain_lines.map(|v| v as f32) }
}

/// The tables of the last mode asked for: made once, a mode change (a new sequence) makes them again.
static TABLES: Mutex<Option<(u32, Tables)>> = Mutex::new(None);

/// The colour mode of a sequence: 0 PQ (H.273 transfer 16), 2 HLG (18), else 1 (SDR).
fn mode_of(seq: &SequenceHeader) -> u32 {
    match seq.transfer_characteristics as u32 {
        16 => 0,
        18 => 2,
        _ => 1,
    }
}

/// Writes the tables of the sequence's mode to `dst` (`LUT_WORDS` words of the frame's constant memory;
/// nothing for SDR, whose shader does not read them) and returns the inline constants of
/// `display_pass.hlsl` for the frame `src` and the output size `out`.
///
/// # Safety
/// `dst` is writable for `LUT_WORDS` words.
pub(crate) unsafe fn constants(src: &HwFrameBuffer, out: (i32, i32), seq: &SequenceHeader, dst: *mut u32) -> [u32; 26] {
    let mode = mode_of(seq);
    let (eotf, gain) = if mode == 1 {
        ([0.0; 3], [0.0; 4])
    } else {
        let mut cache = TABLES.lock().unwrap_or_else(|e| e.into_inner());
        if cache.as_ref().is_none_or(|(m, _)| *m != mode) {
            *cache = Some((mode, tables(mode)));
        }
        let (_, t) = cache.as_ref().expect("made above");
        // SAFETY: the caller's contract.
        unsafe { dst.copy_from_nonoverlapping(t.words.as_ptr(), LUT_WORDS) };
        (t.eotf, t.gain)
    };
    let (w, h) = (src.y_crop_width as u32, src.y_crop_height as u32);
    let aligned =
        [src.planes[0].offset, src.planes[1].offset, src.planes[2].offset, src.planes[0].stride, src.planes[1].stride]
            .iter()
            .all(|v| v % 4 == 0);
    // Exactly 2:1 with 4-byte aligned planes: the shader's quad path (3840x2160 to 1920x1080).
    let quad = aligned && w == 2 * out.0 as u32 && h == 2 * out.1 as u32;
    let wide = seq.matrix_coefficients as u32 == 9;
    let full = seq.color_range != 0;
    // Y'CbCr to R'G'B' (BT.2020 or BT.709 weights, the model's arithmetic folded into eight constants).
    let (wr, wb, wg) = if wide { (0.2627f32, 0.0593f32, 0.6780f32) } else { (0.2126, 0.0722, 0.7152) };
    // The codes of the depth: 10 bits (64 + 876 Y', 512 + 896 C), or 8 (16 + 219 Y', 128 + 224 C).
    let eight = src.hbd == 0;
    let (black, y_range, mid, c_range, full_range) =
        if eight { (16.0, 219.0, 128.0, 224.0, 255.0) } else { (64.0f32, 876.0f32, 512.0f32, 896.0f32, 1023.0f32) };
    let ys = 1.0 / if full { full_range } else { y_range };
    let yo = if full { 0.0 } else { -black / y_range };
    let cs = 1.0 / if full { full_range } else { c_range };
    let (kr, kb) = (2.0 * (1.0 - wr), 2.0 * (1.0 - wb));
    let decode = [ys, yo, cs, -mid * cs, kr, kb, -wb * kb / wg, -wr * kr / wg];
    let flags = mode
        | (u32::from(wide) << 2)
        | (u32::from(full) << 3)
        // AV1 chroma_sample_position: 2 (colocated) sits on the luma row, the others between two rows.
        | (u32::from(seq.chroma_sample_position as u32 != 2) << 4)
        | (u32::from(quad) << 5)
        | (u32::from(eight) << 6);
    [
        w,
        h,
        out.0 as u32,
        out.1 as u32,
        src.planes[0].offset as u32,
        src.planes[1].offset as u32,
        src.planes[2].offset as u32,
        src.planes[0].stride as u32,
        src.planes[1].stride as u32,
        flags,
        eotf[0].to_bits(),
        eotf[1].to_bits(),
        eotf[2].to_bits(),
        gain[0].to_bits(),
        gain[1].to_bits(),
        gain[2].to_bits(),
        gain[3].to_bits(),
        PQ_EXACT_BELOW.to_bits(),
        decode[0].to_bits(),
        decode[1].to_bits(),
        decode[2].to_bits(),
        decode[3].to_bits(),
        decode[4].to_bits(),
        decode[5].to_bits(),
        decode[6].to_bits(),
        decode[7].to_bits(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(t: &Tables, base: usize, pos: f64) -> f64 {
        let i = pos as usize;
        let f = (pos - i as f64) as f32;
        let w = |k: usize| f32::from_bits(t.words[4 * (base + i) + k]);
        f64::from(w(0) + f * (w(1) + f * (w(2) + f * w(3))))
    }

    /// The tables against the functions they hold, positioned as the shader positions them: the EOTF
    /// within 2e-6 (relative) above 1e-5 of SDR white, the gain within 2e-6 everywhere.
    #[test]
    fn tables_hold_their_functions() {
        let t = tables(0);
        let [a, b, c] = t.eotf.map(f64::from);
        let [s1, o1, s2, o2] = t.gain.map(f64::from);
        let (knee, _) = gain_range();
        for n in 1..=100_000 {
            let m = n as f64 / 100_000.0;
            let want = pq_to_nits(m);
            let got = eval(&t, 0, (m * a).min(m * b + c).min(EOTF_N as f64));
            if want / 203.0 > 1e-5 {
                assert!((got - want).abs() <= 2e-6 * want, "EOTF at {m}: {got} for {want}");
            }
            let x = knee + (1.0 - knee) * m;
            let want = gain(pq_to_nits(x));
            let got = eval(&t, EOTF_N + 1, (x * s1 + o1).min(x * s2 + o2).clamp(0.0, GAIN_N as f64));
            assert!((got - want).abs() <= 2e-6 * want, "gain at {x}: {got} for {want}");
        }
        assert!((eval(&t, EOTF_N + 1, 0.0) * 203.0 - 1.0).abs() < 1e-6);
    }
}

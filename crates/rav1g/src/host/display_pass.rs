// SPDX-License-Identifier: BSD-2-Clause
//! The display pass prototype (docs/stage-fusion.md, option B; `-displaypass WxH`): the inline constants
//! of `display_pass.hlsl` and its two tables.
//!
//! The pass is bound by arithmetic (the console's 3.5 ms a 4K frame for a 31 MB pass, against ~0.8 ms of
//! traffic at the idle state's 40 GB/s), and most of it was PQ's transfer functions: the EOTF of three
//! channels and the BT.2390 EETF of the largest, seven `pow` pairs a source pixel. Both are functions of
//! one signal, so the host computes them once here, in float32 with the functions of the model
//! (`testing/display_pass/model.py`), and the shader interpolates (6 KB, read into group memory by every
//! group of 64x64 output pixels: 1 MB of cache reads a 1080p frame). The tables of a colour mode are made
//! once a process and copied into the frame's constant memory (6 KB a frame, the CPU's only work for the
//! pass).

use std::sync::Mutex;

use crate::sys::{HwFrameBuffer, SequenceHeader};

/// Intervals of the EOTF table: `EOTF_FINE` up to the signal `EOTF_SPLIT`, the rest above. With linear
/// interpolation a grey's output code is then within 0.09 of the exact EOTF's (0.006 above a signal of
/// 0.01; the worst is in the first interval, a few codes above black).
const EOTF_N: usize = 1024;
const EOTF_FINE: usize = 256;
const EOTF_SPLIT: f32 = 1.0 / 32.0;
/// Intervals of the gain table, from the EETF's knee (below it the gain is 1 / the target) to a signal of
/// 1 (PQ) or 1024 nits (HLG).
const GAIN_N: usize = 512;
/// Words of the tables: the EOTF's `EOTF_N + 1` values, the gain's `GAIN_N + 1`, two of padding (the
/// shader reads them in 16-byte pieces).
pub(crate) const LUT_WORDS: usize = 1540;
const _: () = assert!(EOTF_N + 1 + GAIN_N < LUT_WORDS && LUT_WORDS.is_multiple_of(4));

/// Below this largest PQ signal the shader computes the peak's nits instead of interpolating them: the
/// gain is `eetf(peak) / max(peak, 1e-6)`, so below 1e-6 nits (a signal of 1.2e-4) it is proportional to
/// the peak, and pixels there with a large negative channel (out of gamut) show it.
const PQ_EXACT_BELOW: f32 = 4e-4;

/// The tone map's parameters, as the stage the pass stands in for has them: source peak 1000 nits, SDR
/// white 100 nits (the 203-nit endpoint of the EETF).
const SOURCE_PEAK: f32 = 1000.0;
const SDR_WHITE: f32 = 100.0;

// ST 2084's constants as the standard writes them (m1 = 2610/16384, m2 = 2523/32, c1 = 3424/4096, ...).
#[allow(clippy::excessive_precision)]
fn pq_to_nits(e: f32) -> f32 {
    let p = e.clamp(0.0, 1.0).powf(1.0 / 78.843_75);
    ((p - 0.835_937_5).max(0.0) / (18.851_562_5 - 18.6875 * p)).powf(1.0 / 0.159_301_757_812_5) * 10000.0
}

#[allow(clippy::excessive_precision)]
fn nits_to_pq(nits: f32) -> f32 {
    let p = (nits.clamp(0.0, 10000.0) / 10000.0).powf(0.159_301_757_812_5);
    ((0.835_937_5 + 18.851_562_5 * p) / (1.0 + 18.6875 * p)).powf(78.843_75)
}

/// The BT.2390 EETF of the model: nits to the SDR range, 1 at the target (2.03 x white).
fn eetf(nits: f32) -> f32 {
    let nits = nits.clamp(0.0, SOURCE_PEAK);
    let target = (SDR_WHITE * 2.03).min(10000.0);
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
fn gain(nits: f32) -> f32 {
    eetf(nits) / nits.max(1e-6)
}

/// The signal and the nits from which the gain is not constant: the EETF's knee, or the target when the
/// curve only clips.
fn gain_start() -> (f32, f32) {
    let target = (SDR_WHITE * 2.03).min(10000.0);
    if SOURCE_PEAK <= target {
        return (nits_to_pq(target), target);
    }
    let peak = nits_to_pq(SOURCE_PEAK);
    let knee = 2.0 * (nits_to_pq(target) / peak) - 1.0;
    let e = knee * peak;
    (e, pq_to_nits(e))
}

/// The tables of a mode (0 PQ, 2 HLG; SDR has none) and the shader's constants for them: the EOTF
/// table's a, b, c, the gain table's scale and offset.
struct Tables {
    words: Box<[u32; LUT_WORDS]>,
    eotf: [f32; 3],
    gain: [f32; 2],
}

fn tables(mode: u32) -> Tables {
    let mut words = Box::new([0u32; LUT_WORDS]);
    let a = EOTF_FINE as f32 / EOTF_SPLIT;
    let b = (EOTF_N - EOTF_FINE) as f32 / (1.0 - EOTF_SPLIT);
    let c = EOTF_FINE as f32 - EOTF_SPLIT * b;
    let (start_e, start_nits) = gain_start();
    let gain_words = &mut words[EOTF_N + 1..EOTF_N + 1 + GAIN_N + 1];
    let (scale, offset) = if mode == 2 {
        // HLG: the peak's log2 nits, from the knee's to 1024 nits (the OOTF of a 1000-nit display ends
        // there).
        let u0 = start_nits.log2();
        let scale = GAIN_N as f32 / (10.0 - u0);
        for (j, w) in gain_words.iter_mut().enumerate() {
            *w = gain((u0 + j as f32 / scale).exp2()).to_bits();
        }
        (scale, -u0 * scale)
    } else {
        let scale = GAIN_N as f32 / (1.0 - start_e);
        for (j, w) in gain_words.iter_mut().enumerate() {
            *w = gain(pq_to_nits(start_e + j as f32 / scale)).to_bits();
        }
        for (k, w) in words[..=EOTF_N].iter_mut().enumerate() {
            let e = if k < EOTF_FINE { k as f32 / a } else { (k as f32 - c) / b };
            *w = pq_to_nits(e.min(1.0)).to_bits();
        }
        (scale, -start_e * scale)
    };
    Tables { words, eotf: [a, b, c], gain: [scale, offset] }
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
pub(crate) unsafe fn constants(src: &HwFrameBuffer, out: (i32, i32), seq: &SequenceHeader, dst: *mut u32) -> [u32; 24] {
    let mode = mode_of(seq);
    let (eotf, gain) = if mode == 1 {
        ([0.0; 3], [0.0; 2])
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
    let ys = if full { 1.0 / 1023.0 } else { 1.0f32 / 876.0 };
    let yo = if full { 0.0 } else { -64.0f32 / 876.0 };
    let cs = if full { 1.0 / 1023.0 } else { 1.0f32 / 896.0 };
    let (kr, kb) = (2.0 * (1.0 - wr), 2.0 * (1.0 - wb));
    let decode = [ys, yo, cs, -512.0 * cs, kr, kb, -wb * kb / wg, -wr * kr / wg];
    let flags = mode
        | (u32::from(wide) << 2)
        | (u32::from(full) << 3)
        // AV1 chroma_sample_position: 2 (colocated) sits on the luma row, the others between two rows.
        | (u32::from(seq.chroma_sample_position as u32 != 2) << 4)
        | (u32::from(quad) << 5);
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

    /// The tables against the functions they hold: the EOTF interpolated as the shader does it, within
    /// 0.1 of an output code for a grey, and the gain constant up to the knee.
    #[test]
    fn tables_hold_their_functions() {
        let t = tables(0);
        let value = |i: usize| f32::from_bits(t.words[i]);
        let [a, b, c] = t.eotf;
        let encode = |nits: f32| (nits / 203.0).clamp(0.0, 1.0).powf(1.0 / 2.4) * 876.0;
        let mut worst = 0.0f32;
        for n in 1..200_000 {
            let m = n as f32 / 200_000.0;
            let pos = (m * a).min(m * b + c).min(EOTF_N as f32);
            let i = (pos as usize).min(EOTF_N - 1);
            let f = pos - i as f32;
            let v = value(i) + f * (value(i + 1) - value(i));
            worst = worst.max((encode(v) - encode(pq_to_nits(m))).abs());
        }
        assert!(worst < 0.1, "EOTF table: {worst} codes");
        let (start_e, _) = gain_start();
        assert!((value(EOTF_N + 1) - gain(pq_to_nits(start_e * 0.5))).abs() < 1e-6);
        assert!((value(EOTF_N + 1) * 203.0 - 1.0).abs() < 1e-3);
    }
}

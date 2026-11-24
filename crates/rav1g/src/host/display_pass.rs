// SPDX-License-Identifier: BSD-2-Clause
//! The display pass (docs/stage-fusion.md, option B; `Rav1gSettings::display`): which transform a frame
//! takes, the inline constants of `display_pass.hlsl` and its two tables.
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
//! and of the tone map's parameters are made once (about 2000 evaluations of the float64 functions, well
//! under a millisecond) and copied into the frame's constant memory (5.5 KB a frame, the CPU's only work
//! for the pass).

use std::ffi::c_int;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::sys::{
    AV1_DISPLAY_TRANSFORM_AUTO, AV1_DISPLAY_TRANSFORM_GAMUT, AV1_DISPLAY_TRANSFORM_HLG, AV1_DISPLAY_TRANSFORM_PQ,
    AV1_DISPLAY_TRANSFORM_SCALE, HwFrameBuffer, SequenceHeader, aom_hdr_metadata_t, av1_display_cfg,
};

/// `av1_set_display_fixed_peak` (`dx/av1_perf.h`): the configured source peak for every PQ frame, the HDR
/// metadata ignored (the version before interface 3), for a picture A/B.
static FIXED_PEAK: AtomicBool = AtomicBool::new(false);
/// `av1_set_display_bilinear_chroma`: the bilinear input chroma of the version before interface 3.
static BILINEAR_CHROMA: AtomicBool = AtomicBool::new(false);

/// `av1_set_display_fixed_peak` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_display_fixed_peak(enable: c_int) {
    FIXED_PEAK.store(enable != 0, Ordering::Relaxed);
}

/// `av1_set_display_bilinear_chroma` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_display_bilinear_chroma(enable: c_int) {
    BILINEAR_CHROMA.store(enable != 0, Ordering::Relaxed);
}

/// The source peak the EETF maps from for a frame (docs/integration.md, "The source peak"), nits. For PQ
/// (ST 2084, mode 0) the HDR metadata says it: MaxCLL when above 0, else the mastering display's largest
/// luminance when above 0, and the smaller of the two when both are (a MaxCLL above the display the
/// content was graded on is broken metadata), clamped to 400..10000 nits (below is broken metadata, 10000
/// is the end of PQ); without either the configured peak (`Rav1gDisplay::source_peak_nits`, 1000 by
/// default) as configured. Each value is the stream's (the CLL and MDCV metadata OBUs, AV1 5.8.3/5.8.4,
/// 6.7.3/6.7.4: max_cll in cd/m2, luminance_max in 24.8 fixed point, the ones that hold for the frame) or,
/// where the stream says nothing or 0, the application's (`max_cll_nits`, `mastering_max_nits`), as
/// for the assumed colour code points. HLG keeps the configured peak (its OOTF renders for a 1000-nit
/// display already), and so does every frame under `av1_set_display_fixed_peak`. 0 for SDR: no tone map.
pub(crate) fn source_peak(choice: Choice, hdr: &aom_hdr_metadata_t, display: &av1_display_cfg) -> f32 {
    match choice.mode {
        1 => return 0.0,
        2 => return display.source_peak_nits,
        _ if FIXED_PEAK.load(Ordering::Relaxed) => return display.source_peak_nits,
        _ => {}
    }
    let stream_cll = if hdr.has_cll != 0 { f32::from(hdr.max_cll) } else { 0.0 };
    let stream_mastering = if hdr.has_mdcv != 0 { (f64::from(hdr.mdcv_luminance_max) / 256.0) as f32 } else { 0.0 };
    let cll = if stream_cll > 0.0 { stream_cll } else { display.max_cll_nits };
    let mastering = if stream_mastering > 0.0 { stream_mastering } else { display.mastering_max_nits };
    let peak = match (cll > 0.0, mastering > 0.0) {
        (true, true) => cll.min(mastering),
        (true, false) => cll,
        (false, true) => mastering,
        (false, false) => return display.source_peak_nits,
    };
    peak.clamp(400.0, 10000.0)
}

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

/// The inline constants of `display_pass.hlsl` (`cb_display`): the pipeline layout's count
/// (`compute.rs`) is this, so that the shader never reads a constant the layout does not set.
pub(crate) const CONSTANTS: usize = 26;

/// Below this largest PQ signal the shader computes the peak's nits instead of interpolating them: the
/// gain is `eetf(peak) / max(peak, 1e-6)`, so below 1e-6 nits (a signal of 1.2e-4) it is proportional to
/// the peak, and pixels there with a large negative channel (out of gamut) show it.
const PQ_EXACT_BELOW: f32 = 4e-4;

/// The tone map's parameters: the source peak the BT.2390 EETF maps to the SDR range (`source_peak`), and
/// the SDR white whose 2.03 times is the EETF's endpoint (`Rav1gDisplay::target_white_nits`: 203 nits for
/// a white of 100). The defaults are those of the stage the pass stands in for: 1000 and 100.
#[derive(Clone, Copy, PartialEq, Debug)]
struct ToneMap {
    peak: f64,
    white: f64,
}

/// The nits of HLG's nominal display, at which its OOTF (BT.2100, gamma 1.2) is applied: the light of an
/// HLG frame ends at 1000.1 nits whatever the tone map's peak.
const HLG_DISPLAY: f64 = 1000.0;

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

impl ToneMap {
    fn target(self) -> f64 {
        (self.white * 2.03).min(10000.0)
    }

    /// The BT.2390 EETF of the model: nits to the SDR range, 1 at the target (2.03 x white).
    fn eetf(self, nits: f64) -> f64 {
        let nits = nits.clamp(0.0, self.peak);
        let target = self.target();
        if self.peak <= target {
            return (nits / target).min(1.0);
        }
        let peak = nits_to_pq(self.peak);
        let s = nits_to_pq(nits) / peak;
        let top = nits_to_pq(target) / peak;
        let knee = 2.0 * top - 1.0;
        let mut mapped = s;
        if s >= knee {
            let t = (s - knee) / (1.0 - knee);
            let t2 = t * t;
            let t3 = t2 * t;
            mapped =
                (2.0 * t3 - 3.0 * t2 + 1.0) * knee + (t3 - 2.0 * t2 + t) * (1.0 - knee) + (-2.0 * t3 + 3.0 * t2) * top;
        }
        (pq_to_nits(mapped * peak) / target).clamp(0.0, 1.0)
    }

    /// The gain the model gives a pixel whose largest channel has `nits` (above 1e-6 nits).
    fn gain(self, nits: f64) -> f64 {
        self.eetf(nits) / nits.max(1e-6)
    }

    /// The signal from which the gain is not constant (the EETF's knee, or the target when the curve
    /// only clips) and the signal where it meets its clip (the source peak's, or the target's).
    fn gain_range(self) -> (f64, f64) {
        if self.peak <= self.target() {
            // The curve is a line to the clip at min(peak, target): constant gain up to there.
            let e = nits_to_pq(self.peak.min(self.target()));
            return (e, e);
        }
        let peak = nits_to_pq(self.peak);
        let knee = 2.0 * (nits_to_pq(self.target()) / peak) - 1.0;
        (knee * peak, peak)
    }
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

fn tables(mode: u32, tone: ToneMap) -> Tables {
    let mut words = Box::new([0u32; LUT_WORDS]);
    let a = EOTF_FINE as f64 / EOTF_SPLIT;
    let b = (EOTF_N - EOTF_FINE) as f64 / (1.0 - EOTF_SPLIT);
    let c = EOTF_FINE as f64 - EOTF_SPLIT * b;
    let (eotf_words, gain_words) = words.split_at_mut(4 * (EOTF_N + 1));
    let (knee, peak) = tone.gain_range();
    let gain_lines = if mode == 2 {
        // HLG: the peak's log2 nits, one line from the knee's nits to the larger of the tone map's peak and
        // the nominal display's 1000 (the light of HLG ends at 1000.1): that on a node, the table one
        // interval further (1032 nits with the defaults).
        let u0 = pq_to_nits(knee).log2();
        let step = (tone.peak.max(HLG_DISPLAY).log2() - u0).max(1e-3) / (GAIN_N - 1) as f64;
        fill(gain_words, GAIN_N, &|j| u0 + j as f64 * step, &|u| tone.gain(u.exp2()));
        let s = 1.0 / step;
        [s, -u0 * s, s, -u0 * s]
    } else {
        // PQ: GAIN_LOW intervals from the knee to the peak's signal, the rest to 1; a curve without a knee
        // (a peak at or below the target) has all of them from its clip to 1. The shader takes the smaller
        // of the two lines when the knee's intervals are the denser (s1 >= s2: peaks up to about 1200
        // nits), the larger when those above the peak are (higher peaks, as HDR metadata gives them).
        let low = if peak > knee { GAIN_LOW } else { 0 };
        let s1 = if low > 0 { low as f64 / (peak - knee) } else { 1.0 };
        let s2 = (GAIN_N - low) as f64 / (1.0 - peak).max(1e-6);
        let x = move |j: usize| if j < low { knee + j as f64 / s1 } else { peak + (j - low) as f64 / s2 };
        fill(gain_words, GAIN_N, &x, &|e| tone.gain(pq_to_nits(e)));
        let x = |k: usize| if k < EOTF_FINE { k as f64 / a } else { (k as f64 - c) / b };
        fill(eotf_words, EOTF_N, &x, &pq_to_nits);
        if low > 0 { [s1, -knee * s1, s2, GAIN_LOW as f64 - peak * s2] } else { [s2, -peak * s2, s2, -peak * s2] }
    };
    Tables { words, eotf: [a as f32, b as f32, c as f32], gain: gain_lines.map(|v| v as f32) }
}

/// The tables of the last mode and parameters asked for: made once, a change (a new sequence, another
/// decoder's parameters, a stream whose HDR metadata gives another peak) makes them again (about 2000
/// float64 evaluations, well under a millisecond of one CPU; the metadata of a stream is constant in
/// practice).
static TABLES: Mutex<Option<(u32, ToneMap, Tables)>> = Mutex::new(None);

/// What the display pass does to a frame: the colour it takes the frame to have (the sequence header's,
/// with the assumed code points where it says "unspecified" or "unknown"), and from it the shader's mode
/// (0 PQ, 1 SDR, 2 HLG, the pipeline), whether its primaries are BT.2020 (converted to BT.709) and the
/// transform the frame reports (`AV1_DISPLAY_TRANSFORM_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    pub mode: u32,
    pub bt2020: bool,
    pub transform: u32,
    pub matrix: u8,
    pub chroma_sample_position: u8,
}

/// H.273 "unspecified" (2), which an AV1 sequence header without a colour description has for all three.
const UNSPECIFIED: u8 = 2;

/// The transform of a frame of the sequence `seq` under the settings `display`.
pub(crate) fn choose(seq: &SequenceHeader, display: &av1_display_cfg) -> Choice {
    let assume = |value: u8, unknown: u8, assumed: u8| if value == unknown && assumed != 0 { assumed } else { value };
    let primaries = assume(seq.color_primaries as u8, UNSPECIFIED, display.assumed_primaries);
    let transfer = assume(seq.transfer_characteristics as u8, UNSPECIFIED, display.assumed_transfer);
    let matrix = assume(seq.matrix_coefficients as u8, UNSPECIFIED, display.assumed_matrix);
    // AV1 chroma_sample_position 0 is CSP_UNKNOWN.
    let chroma_sample_position = assume(seq.chroma_sample_position as u8, 0, display.assumed_chroma_sample_position);
    let wide = primaries == 9;
    let transform = match display.transform {
        AV1_DISPLAY_TRANSFORM_AUTO => match transfer {
            16 => AV1_DISPLAY_TRANSFORM_PQ,
            18 => AV1_DISPLAY_TRANSFORM_HLG,
            _ if wide => AV1_DISPLAY_TRANSFORM_GAMUT,
            _ => AV1_DISPLAY_TRANSFORM_SCALE,
        },
        forced => forced,
    };
    let (mode, bt2020) = match transform {
        AV1_DISPLAY_TRANSFORM_PQ => (0, wide),
        AV1_DISPLAY_TRANSFORM_HLG => (2, wide),
        AV1_DISPLAY_TRANSFORM_GAMUT => (1, true),
        _ => (1, false),
    };
    Choice { mode, bt2020, transform, matrix, chroma_sample_position }
}

/// The flag of the shader's fast path for a frame of `src` pixels into an image of `out` (bit 5, 8 or 9 of
/// its flags), or 0 for the area loop. The fast paths need planes at multiples of 4 bytes (`aligned`):
/// exactly 2:1 (the quad path: 3840x2160 to 1920x1080, 3840x1600 to 1920x800), exactly 1:1 (a block path:
/// a 4K frame on a 4K display, a 1080p frame on a 1080p display) and exactly 3:2 (a block path: 3840x2160
/// to 2560x1440). The display sizes they are for are those of an HDMI mode with the frame fitted into it;
/// a ratio that is not exact in both axes (3840x1600 to 2560x1066) takes the area loop.
fn path_flags(src: (u32, u32), out: (u32, u32), aligned: bool) -> u32 {
    let ((w, h), (ow, oh)) = (src, out);
    if !aligned {
        0
    } else if w == 2 * ow && h == 2 * oh {
        1 << 5
    } else if w == ow && h == oh {
        1 << 8
    } else if 2 * w == 3 * ow && 2 * h == 3 * oh {
        1 << 9
    } else {
        0
    }
}

/// The inline constant that holds the flags (`cb_flags`).
const FLAGS: usize = 9;

/// Whether the constants of a frame (`constants`) take a block path (1:1 or 3:2: bit 8 or 9 of the flags),
/// whose pipelines are `shader_display_pass_block`; else the 2:1 path or the area loop, of
/// `shader_display_pass` (`DISPLAY_PATHS` of `display_pass.hlsl`). Two pipelines a mode so that the 2:1
/// path, the case Bale runs most (HDR10 on a 1080p display), has neither the block paths' registers nor
/// their code: in one pipeline they took PQ to the RDNA2 ceiling of 106 SGPRs and 38% more code around the
/// same 2:1 loop, which the console measured 0.10 ms a 4K frame slower (1.49 to 1.59 ms).
pub(crate) fn block_path(constants: &[u32; CONSTANTS]) -> bool {
    constants[FLAGS] & ((1 << 8) | (1 << 9)) != 0
}

/// Writes the tables of the frame's mode to `dst` (`LUT_WORDS` words of the frame's constant memory;
/// nothing for SDR, whose shader does not read them) and returns the inline constants of
/// `display_pass.hlsl` for the frame `src` (read as the `choice` says; `monochrome`: its chroma is taken
/// as the middle value) into an image of the size `out` of the settings `display`, tone-mapped from
/// `peak` (`source_peak`).
///
/// # Safety
/// `dst` is writable for `LUT_WORDS` words.
#[expect(clippy::too_many_arguments, reason = "what the frame, its settings and its source peak give the pass")]
pub(crate) unsafe fn constants(
    src: &HwFrameBuffer,
    out: (i32, i32),
    choice: Choice,
    full: bool,
    monochrome: bool,
    display: &av1_display_cfg,
    peak: f32,
    dst: *mut u32,
) -> [u32; CONSTANTS] {
    let mode = choice.mode;
    let tone = ToneMap { peak: f64::from(peak), white: f64::from(display.target_white_nits) };
    let (eotf, gain) = if mode == 1 {
        ([0.0; 3], [0.0; 4])
    } else {
        let mut cache = TABLES.lock().unwrap_or_else(|e| e.into_inner());
        if cache.as_ref().is_none_or(|(m, t, _)| *m != mode || *t != tone) {
            *cache = Some((mode, tone, tables(mode, tone)));
        }
        let (_, _, t) = cache.as_ref().expect("made above");
        // SAFETY: the caller's contract.
        unsafe { dst.copy_from_nonoverlapping(t.words.as_ptr(), LUT_WORDS) };
        (t.eotf, t.gain)
    };
    let (w, h) = (src.y_crop_width as u32, src.y_crop_height as u32);
    let aligned =
        [src.planes[0].offset, src.planes[1].offset, src.planes[2].offset, src.planes[0].stride, src.planes[1].stride]
            .iter()
            .all(|v| v % 4 == 0);
    let path = path_flags((w, h), (out.0 as u32, out.1 as u32), aligned);
    // Y'CbCr to R'G'B' by the matrix (H.273: BT.2020 9, BT.601 5 and 6, else BT.709 weights), the
    // model's arithmetic folded into eight constants.
    let (wr, wb) = match choice.matrix {
        9 => (0.2627f32, 0.0593f32),
        5 | 6 => (0.299, 0.114),
        _ => (0.2126, 0.0722),
    };
    let wg = 1.0 - wr - wb;
    // The codes of the frame's depth: 10 bits (64 + 876 Y', 512 + 896 C), or 8 (16 + 219 Y', 128 + 224 C).
    let eight = src.hbd == 0;
    let (black, y_range, mid, c_range, full_range) =
        if eight { (16.0, 219.0, 128.0, 224.0, 255.0) } else { (64.0f32, 876.0f32, 512.0f32, 896.0f32, 1023.0f32) };
    let ys = 1.0 / if full { full_range } else { y_range };
    let yo = if full { 0.0 } else { -black / y_range };
    // A monochrome frame: Cb = Cr = 0 whatever its chroma planes hold.
    let cs = if monochrome { 0.0 } else { 1.0 / if full { full_range } else { c_range } };
    let (kr, kb) = (2.0 * (1.0 - wr), 2.0 * (1.0 - wb));
    let decode = [ys, yo, cs, -mid * cs, kr, kb, -wb * kb / wg, -wr * kr / wg];
    let flags = mode
        | (u32::from(choice.bt2020) << 2)
        | (u32::from(full) << 3)
        // AV1 chroma_sample_position: 2 (colocated) sits on the luma row, the others between two rows.
        | (u32::from(choice.chroma_sample_position != 2) << 4)
        | path
        | (u32::from(eight) << 6)
        | (u32::from(display.depth == 8) << 7)
        | (u32::from(display.dither != 0) << 10)
        | (u32::from(BILINEAR_CHROMA.load(Ordering::Relaxed)) << 11);
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

    /// The gain table's position of x as `gain_table` of the shader has it: the smaller of the two lines
    /// when the first is the steeper (or as steep), else the larger; 0 to GAIN_N.
    fn gain_position(t: &Tables, x: f64) -> f64 {
        let [s1, o1, s2, o2] = t.gain.map(f64::from);
        let (a, b) = (x * s1 + o1, x * s2 + o2);
        (if s1 >= s2 { a.min(b) } else { a.max(b) }).clamp(0.0, GAIN_N as f64)
    }

    /// The shader's constant block has as many 32-bit fields as the layout sets (`CONSTANTS`).
    #[test]
    fn constants_are_those_of_the_shader() {
        let shader = include_str!("../../../../libav1/dx/shaders/display_pass.hlsl");
        let block = shader.split("cbuffer cb_display").nth(1).and_then(|s| s.split("};").next()).expect("cb_display");
        let fields = block
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with("uint cb_") || line.starts_with("float cb_"))
            .count();
        assert_eq!(fields, CONSTANTS);
    }

    /// The fast path a size takes: 2:1, 1:1 and 3:2 exactly, aligned planes; the rest the area loop.
    #[test]
    fn sizes_take_their_paths() {
        let path = |w, h, ow, oh| path_flags((w, h), (ow, oh), true);
        assert_eq!(path(3840, 2160, 1920, 1080), 1 << 5);
        assert_eq!(path(3840, 1600, 1920, 800), 1 << 5);
        assert_eq!(path(3840, 2160, 3840, 2160), 1 << 8);
        assert_eq!(path(1920, 1080, 1920, 1080), 1 << 8);
        assert_eq!(path(3840, 2160, 2560, 1440), 1 << 9);
        assert_eq!(path(1920, 1080, 1280, 720), 1 << 9);
        assert_eq!(path(3840, 1600, 2560, 1066), 0);
        assert_eq!(path(3840, 2160, 1280, 720), 0);
        assert_eq!(path_flags((3840, 2160), (1920, 1080), false), 0);
    }

    /// The pipeline a frame takes follows its path flags: the block pipelines for 1:1 and 3:2 alone, and
    /// the flags are the constant `FLAGS` (the field `cb_flags` of the shader).
    #[test]
    fn block_paths_take_their_pipelines() {
        let shader = include_str!("../../../../libav1/dx/shaders/display_pass.hlsl");
        let block = shader.split("cbuffer cb_display").nth(1).and_then(|s| s.split("};").next()).expect("cb_display");
        let names: Vec<&str> = block
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with("uint cb_") || line.starts_with("float cb_"))
            .collect();
        assert!(names[FLAGS].starts_with("uint cb_flags;"));
        let with = |path: u32| {
            let mut c = [0u32; CONSTANTS];
            // Every other flag set: the mode bits, the primaries, range, siting, sample size, output depth,
            // dither and chroma switch.
            c[FLAGS] = path | 3 | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 10) | (1 << 11);
            block_path(&c)
        };
        assert!(with(1 << 8));
        assert!(with(1 << 9));
        assert!(!with(1 << 5));
        assert!(!with(0));
    }

    const DEFAULT: ToneMap = ToneMap { peak: 1000.0, white: 100.0 };

    /// The tables against the functions they hold, positioned as the shader positions them: the EOTF
    /// within 2e-6 (relative) above 1e-5 of SDR white, the gain within 2e-6 everywhere; with the default
    /// parameters, a lower peak and white, and a peak below the target (no knee).
    #[test]
    fn tables_hold_their_functions() {
        let tones = [
            DEFAULT,
            ToneMap { peak: 600.0, white: 203.0 },
            ToneMap { peak: 150.0, white: 100.0 },
            // Peaks from the HDR metadata (source_peak): the knee's intervals denser from about 1200 nits on.
            ToneMap { peak: 1184.0, white: 100.0 },
            ToneMap { peak: 1500.0, white: 100.0 },
            ToneMap { peak: 4000.0, white: 100.0 },
            ToneMap { peak: 9500.0, white: 100.0 },
            ToneMap { peak: 10000.0, white: 100.0 },
            ToneMap { peak: 400.0, white: 400.0 },
            ToneMap { peak: 4000.0, white: 80.0 },
        ];
        for tone in tones {
            let t = tables(0, tone);
            let [a, b, c] = t.eotf.map(f64::from);
            let (knee, _) = tone.gain_range();
            for n in 1..=100_000 {
                let m = n as f64 / 100_000.0;
                let want = pq_to_nits(m);
                let got = eval(&t, 0, (m * a).min(m * b + c).min(EOTF_N as f64));
                if want / 203.0 > 1e-5 {
                    assert!((got - want).abs() <= 2e-6 * want, "EOTF at {m}: {got} for {want}");
                }
                let x = knee + (1.0 - knee) * m;
                let want = tone.gain(pq_to_nits(x));
                let got = eval(&t, EOTF_N + 1, gain_position(&t, x));
                assert!((got - want).abs() <= 2e-6 * want, "{tone:?}: gain at {x}: {got} for {want}");
            }
            // Below the knee the gain is that of the line: 1 / the target, or 1 / the clip's nits.
            let below = 1.0 / tone.target().min(tone.peak.max(tone.target()));
            assert!((eval(&t, EOTF_N + 1, 0.0) - below).abs() < 1e-6 * below, "{tone:?}");
        }
    }

    /// HLG's gain table on log2 nits reaches the end of HLG's light (1000.1 nits) whatever the peak.
    #[test]
    fn hlg_table_reaches_the_light_of_hlg() {
        for tone in [DEFAULT, ToneMap { peak: 400.0, white: 100.0 }] {
            let t = tables(2, tone);
            let [s1, o1, ..] = t.gain.map(f64::from);
            for nits in [250.0, 400.0, 700.0, 1000.1] {
                let x = f64::log2(nits);
                let want = tone.gain(nits);
                let got = eval(&t, EOTF_N + 1, (x * s1 + o1).clamp(0.0, GAIN_N as f64));
                assert!((got - want).abs() <= 2e-6 * want, "{tone:?}: gain at {nits} nits: {got} for {want}");
            }
        }
    }

    /// The source peak of a frame (`source_peak`): Bale's console checks of its stage (MaxCLL 4057 with a
    /// mastering display of 1000: 1000; 1184 with 4000: 1184; none: the configured 1000), MaxCLL alone,
    /// the mastering display alone, both clamps, the configured peak unclamped, the application's values
    /// where the stream says nothing (or 0) and the stream's where it does, HLG and SDR, the fixed peak.
    #[test]
    fn the_source_peak_follows_the_hdr_metadata() {
        let pq =
            Choice { mode: 0, bt2020: true, transform: AV1_DISPLAY_TRANSFORM_PQ, matrix: 9, chroma_sample_position: 0 };
        let hlg = Choice { mode: 2, transform: AV1_DISPLAY_TRANSFORM_HLG, ..pq };
        let sdr = Choice { mode: 1, transform: AV1_DISPLAY_TRANSFORM_GAMUT, ..pq };
        // The stream's metadata: MaxCLL in cd/m2, the mastering display's largest luminance in 24.8.
        let stream = |cll: Option<u16>, mastering: Option<f64>| aom_hdr_metadata_t {
            has_cll: u8::from(cll.is_some()),
            max_cll: cll.unwrap_or(0),
            has_mdcv: u8::from(mastering.is_some()),
            mdcv_luminance_max: mastering.map_or(0, |nits| (nits * 256.0) as u32),
            ..Default::default()
        };
        let app = |peak: f32, cll: f32, mastering: f32| av1_display_cfg {
            source_peak_nits: peak,
            target_white_nits: 100.0,
            max_cll_nits: cll,
            mastering_max_nits: mastering,
            ..Default::default()
        };
        let none = stream(None, None);
        let config = app(1000.0, 0.0, 0.0);
        let cases: [(Choice, aom_hdr_metadata_t, av1_display_cfg, f32); 17] = [
            (pq, stream(Some(4057), Some(1000.0)), config, 1000.0),
            (pq, stream(Some(1184), Some(4000.0)), config, 1184.0),
            (pq, none, config, 1000.0),
            (pq, none, app(600.0, 0.0, 0.0), 600.0),
            (pq, stream(Some(1500), None), config, 1500.0),
            (pq, stream(None, Some(4000.0)), config, 4000.0),
            (pq, stream(None, Some(1000.5)), config, 1000.5),
            (pq, stream(Some(250), None), config, 400.0),
            (pq, stream(None, Some(100.0)), config, 400.0),
            (pq, stream(Some(12000), None), config, 10000.0),
            (pq, stream(None, Some(50000.0)), config, 10000.0),
            // The application's values fill what the stream does not say, value by value.
            (pq, none, app(1000.0, 4057.0, 1000.0), 1000.0),
            (pq, stream(Some(1184), None), app(1000.0, 600.0, 4000.0), 1184.0),
            (pq, stream(Some(0), Some(4000.0)), app(1000.0, 2000.0, 0.0), 2000.0),
            (pq, stream(None, Some(1000.0)), app(1000.0, 4057.0, 600.0), 1000.0),
            (hlg, stream(Some(4000), Some(4000.0)), config, 1000.0),
            (sdr, stream(Some(4000), Some(4000.0)), config, 0.0),
        ];
        for (n, (choice, hdr, display, want)) in cases.iter().enumerate() {
            assert_eq!(source_peak(*choice, hdr, display), *want, "case {n}");
        }
        av1_set_display_fixed_peak(1);
        let fixed = source_peak(pq, &stream(Some(4000), Some(4000.0)), &config);
        av1_set_display_fixed_peak(0);
        assert_eq!(fixed, 1000.0);
    }

    /// The transform of a frame: by its transfer and primaries, the assumed code points where the
    /// sequence header says "unspecified", or what the settings force.
    #[test]
    fn a_frame_takes_the_transform_of_its_colour() {
        let seq = |primaries: u8, transfer: u8, matrix: u8| {
            // SAFETY: all zero is a sequence header.
            let mut seq: SequenceHeader = unsafe { std::mem::zeroed() };
            seq.color_primaries = primaries.into();
            seq.transfer_characteristics = transfer.into();
            seq.matrix_coefficients = matrix.into();
            seq
        };
        let auto = av1_display_cfg { width: 1920, height: 1080, ..Default::default() };
        assert_eq!(choose(&seq(9, 16, 9), &auto).transform, AV1_DISPLAY_TRANSFORM_PQ);
        assert_eq!(choose(&seq(9, 16, 9), &auto).mode, 0);
        assert!(choose(&seq(9, 16, 9), &auto).bt2020);
        assert_eq!(choose(&seq(9, 18, 9), &auto).transform, AV1_DISPLAY_TRANSFORM_HLG);
        assert_eq!(choose(&seq(9, 14, 9), &auto).transform, AV1_DISPLAY_TRANSFORM_GAMUT);
        assert_eq!(choose(&seq(1, 1, 1), &auto).transform, AV1_DISPLAY_TRANSFORM_SCALE);
        assert_eq!(choose(&seq(2, 2, 2), &auto).transform, AV1_DISPLAY_TRANSFORM_SCALE);
        let assumed = av1_display_cfg { assumed_primaries: 9, assumed_transfer: 16, assumed_matrix: 9, ..auto };
        let c = choose(&seq(2, 2, 2), &assumed);
        assert_eq!((c.transform, c.bt2020, c.matrix), (AV1_DISPLAY_TRANSFORM_PQ, true, 9));
        // What the header says stands.
        assert_eq!(choose(&seq(1, 1, 1), &assumed).transform, AV1_DISPLAY_TRANSFORM_SCALE);
        let forced = av1_display_cfg { transform: AV1_DISPLAY_TRANSFORM_SCALE, ..auto };
        assert_eq!(
            choose(&seq(9, 16, 9), &forced),
            Choice {
                mode: 1,
                bt2020: false,
                transform: AV1_DISPLAY_TRANSFORM_SCALE,
                matrix: 9,
                chroma_sample_position: 0
            }
        );
    }
}

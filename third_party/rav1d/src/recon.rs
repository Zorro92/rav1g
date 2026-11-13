#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::{c_int, c_uint};
use std::hint::assert_unchecked;
use std::ops::BitOr;
use std::cmp;
use std::sync::OnceLock;

use assert_matches::debug_assert_matches;
use libc::intptr_t;
use to_method::To as _;

use crate::ctx::CaseSet;
use crate::env::get_uv_inter_txtp;
use crate::in_range::InRange;
use crate::include::common::bitdepth::{AsPrimitive, BitDepth, ToPrimitive, BPC};
use crate::include::common::dump::{ac_dump, coef_dump, hex_dump, hex_dump_pic};
use crate::include::common::intops::{apply_sign64, clip, ulog2};
use crate::include::dav1d::dav1d::Rav1dInloopFilterType;
use crate::include::dav1d::headers::{
    Rav1dPixelLayout, Rav1dPixelLayoutSubSampled, Rav1dWarpedMotionParams, Rav1dWarpedMotionType,
};
use crate::include::dav1d::picture::{Rav1dPictureDataComponent, Rav1dPictureDataComponentOffset};
use crate::internal::{
    Bxy, Cf, CodedBlockInfo, Rav1dContext, Rav1dFrameData, Rav1dTaskContext, Rav1dTileStateContext,
    ScratchEmuEdge, TaskContextScratch, TileStateRef,
};
use crate::intra_edge::EdgeFlags;
use crate::levels::{
    Av1Block, Av1BlockInter, Av1BlockIntra, Av1BlockIntraInter, BlockSize, CompInterType, Filter2d,
    InterIntraPredMode, InterIntraType, IntraPredMode, MotionMode, Mv, TxClass, TxfmSize, TxfmType,
    CFL_PRED, DCT_DCT, DC_PRED, FILTER_PRED, GLOBALMV, GLOBALMV_GLOBALMV, IDTX, SMOOTH_PRED,
    WHT_WHT,
};
use crate::msac::{
    rav1d_msac_decode_bool_adapt, rav1d_msac_decode_bool_equi, rav1d_msac_decode_bools,
    rav1d_msac_decode_hi_tok, rav1d_msac_decode_symbol_adapt16, rav1d_msac_decode_symbol_adapt4,
    rav1d_msac_decode_symbol_adapt8, MsacContext,
};
use crate::picture::Rav1dThreadPicture;
use crate::pixels::Pixels as _;
use crate::parse::ParseTxb;
use crate::scan::DAV1D_SCANS;
use crate::strided::Strided as _;
use crate::tables::{
    LoCtxOffset, TxfmInfo, DAV1D_FILTER_2D, DAV1D_FILTER_MODE_TO_Y_MODE, DAV1D_LO_CTX_OFFSETS,
    DAV1D_SKIP_CTX, DAV1D_TXFM_DIMENSIONS, DAV1D_TXTP_FROM_UVMODE, DAV1D_TX_TYPES_PER_SET,
    DAV1D_TX_TYPE_CLASS,
};
use crate::with_offset::WithOffset;

impl Bxy {
    pub fn debug_block_info(&self) -> bool {
        let Self { x, y } = *self;
        (0..4).contains(&y) && (8..12).contains(&x)
    }
}

// TODO: add feature and compile-time guard around this code
/// Determine if we should print debug information for the current block.
///
/// Takes a [`Rav1dFrameData`] and a [`Bxy`] as arguments to
/// determine the current block and frame offset.
///
/// This a macro rather than a function so that the compiler can see which
/// specific fields are used to avoid borrowck errors.
///
/// [`Bxy`]: crate::internal::Bxy
macro_rules! debug_block_info {
    ($f:expr, $tb:expr) => {{
        use crate::internal::Bxy;

        let tb: Bxy = $tb;
        false && $f.frame_hdr.as_ref().unwrap().frame_offset == 2 && tb.debug_block_info()
    }};
}
pub(crate) use debug_block_info;

const DEBUG_B_PIXELS: bool = false;

pub(crate) type ReadCoefBlocksFn = fn(
    &Rav1dContext,
    &Rav1dFrameData,
    &mut Rav1dTaskContext,
    &mut Rav1dTileStateContext,
    BlockSize,
    &Av1Block,
) -> Result<(), ()>;

pub(crate) type CopyPalBlockFn = fn(
    t: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    bx4: usize,
    by4: usize,
    bw4: usize,
    bh4: usize,
) -> ();

pub(crate) type ReadPalPlaneFn = fn(
    t: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    ts_c: &mut Rav1dTileStateContext,
    pl: bool,
    sz_ctx: u8,
    bx4: usize,
    by4: usize,
) -> u8; // `pal_sz`

pub(crate) type ReadPalUVFn = fn(
    t: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    ts_c: &mut Rav1dTileStateContext,
    sz_ctx: u8,
    bx4: usize,
    by4: usize,
) -> u8; // `pal_sz[1]`

#[inline]
fn read_golomb(msac: &mut MsacContext) -> u32 {
    let mut len = 0;
    let mut val = 1;

    while !rav1d_msac_decode_bool_equi(msac) && len < 32 {
        len += 1;
    }
    for _ in 0..len {
        val = (val << 1) + rav1d_msac_decode_bool_equi(msac) as u32;
    }

    val - 1
}

trait ReadInt {
    /// (Try to or panic) read [`Self`] from the front of `bytes` in native endianness.
    /// These are similar to the [`u32::from_ne_bytes`] type methods,
    /// but generalized into a `trait` to be generic,
    /// and operating on a slice that is sliced and converted into an array first.
    ///
    /// This replaces the previous code that used `unsafe` transmutes through casting.
    fn read_ne(bytes: &[u8]) -> Self;
}

macro_rules! impl_ReadInt {
    ($U:ty) => {
        impl ReadInt for $U {
            fn read_ne(bytes: &[u8]) -> Self {
                let n = std::mem::size_of::<Self>();
                Self::from_ne_bytes(bytes[..n].try_into().unwrap())
            }
        }
    };
}

impl_ReadInt!(u8);
impl_ReadInt!(u16);
impl_ReadInt!(u32);
impl_ReadInt!(u64);
impl_ReadInt!(u128);

trait MergeInt: Sized + Copy {
    type Output: BitOr<Self::Output, Output = Self::Output>;

    fn lo(self) -> Self::Output;

    fn hi(self) -> Self::Output;

    fn merge(self) -> Self::Output {
        self.lo() | self.hi()
    }
}

macro_rules! impl_MergeInt {
    ($T:ty, $U:ty) => {
        impl MergeInt for $T {
            type Output = $U;

            fn lo(self) -> Self::Output {
                self as $U
            }

            fn hi(self) -> Self::Output {
                (self >> <$U>::BITS) as $U
            }
        }
    };
}

impl_MergeInt!(u16, u8);
impl_MergeInt!(u32, u16);
impl_MergeInt!(u64, u32);
impl_MergeInt!(u128, u64);

#[inline]
fn get_skip_ctx(
    t_dim: &TxfmInfo,
    bs: BlockSize,
    a: &[u8],
    l: &[u8],
    chroma: bool,
    layout: Rav1dPixelLayout,
) -> InRange<u8, 0, { 13 - 1 }> {
    let b_dim = bs.dimensions();
    let skip_ctx = if chroma {
        let ss_ver = layout == Rav1dPixelLayout::I420;
        let ss_hor = layout != Rav1dPixelLayout::I444;
        let not_one_blk = b_dim[2] - (b_dim[2] != 0 && ss_hor) as u8 > t_dim.lw
            || b_dim[3] - (b_dim[3] != 0 && ss_ver) as u8 > t_dim.lh;
        fn merge_ctx<const N: usize>(dir: &[u8]) -> bool {
            dir[..N] != [0x40; N]
        }

        fn cdir(dir: &[u8]) -> u8 {
            let cdir = match dir.len() {
                1 => merge_ctx::<1>(dir),
                2 => merge_ctx::<2>(dir),
                4 => merge_ctx::<4>(dir),
                8 => merge_ctx::<8>(dir),
                _ => {
                    debug_assert!(false);
                    false
                }
            };
            cdir as u8
        }

        (7 + (not_one_blk as u8) * 3) + cdir(a) + cdir(l)
    } else if b_dim[2] == t_dim.lw && b_dim[3] == t_dim.lh {
        0
    } else {
        /// Read and xor all the bytes.
        fn merge_ctx(dir: &[u8]) -> u8 {
            let n = dir.len();
            if n == 1 {
                u8::read_ne(dir)
            } else {
                (if n == 2 {
                    u16::read_ne(dir)
                } else {
                    (if n == 4 {
                        u32::read_ne(dir)
                    } else {
                        (if n == 8 {
                            u64::read_ne(dir)
                        } else {
                            (if n == 16 {
                                u128::read_ne(dir)
                            } else {
                                debug_assert!(false);
                                0
                            })
                            .merge()
                        })
                        .merge()
                    })
                    .merge()
                })
                .merge()
            }
        }

        fn ldir(dir: &[u8]) -> usize {
            let ldir = merge_ctx(dir);
            cmp::min(ldir & 0x3f, 4) as usize
        }

        DAV1D_SKIP_CTX[ldir(a)][ldir(l)]
    };
    InRange::new(skip_ctx).unwrap()
}

#[inline]
fn get_dc_sign_ctx(tx: TxfmSize, a: &[u8], l: &[u8]) -> c_uint {
    let mask = 0xc0c0c0c0c0c0c0c0 as u64;
    let mul = 0x101010101010101 as u64;

    use TxfmSize::*;
    let s = match tx {
        S4x4 => {
            let mut t = u8::read_ne(a) as i32 >> 6;
            t += u8::read_ne(l) as i32 >> 6;
            t - 1 - 1
        }
        S8x8 => {
            let mut t = u16::read_ne(a) as u32 & mask as u32;
            t += u16::read_ne(l) as u32 & mask as u32;
            t = t.wrapping_mul(0x4040404);
            (t >> 24) as i32 - 2 - 2
        }
        S16x16 => {
            let mut t = (u32::read_ne(a) & mask as u32) >> 6;
            t += (u32::read_ne(l) & mask as u32) >> 6;
            t = t.wrapping_mul(mul as u32);
            (t >> 24) as i32 - 4 - 4
        }
        S32x32 => {
            let mut t = (u64::read_ne(a) & mask) >> 6;
            t += (u64::read_ne(l) & mask) >> 6;
            t = t.wrapping_mul(mul);
            (t >> 56) as i32 - 8 - 8
        }
        S64x64 => {
            let mut t = (u64::read_ne(&a[0..]) & mask) >> 6;
            t += (u64::read_ne(&a[8..]) & mask) >> 6;
            t += (u64::read_ne(&l[0..]) & mask) >> 6;
            t += (u64::read_ne(&l[8..]) & mask) >> 6;
            t = t.wrapping_mul(mul);
            (t >> 56) as i32 - 16 - 16
        }
        R4x8 => {
            let mut t = u8::read_ne(a) as u32 & mask as u32;
            t += u16::read_ne(l) as u32 & mask as u32;
            t = t.wrapping_mul(0x4040404);
            (t >> 24) as i32 - 1 - 2
        }
        R8x4 => {
            let mut t = u16::read_ne(a) as u32 & mask as u32;
            t += u8::read_ne(l) as u32 & mask as u32;
            t = t.wrapping_mul(0x4040404);
            (t >> 24) as i32 - 2 - 1
        }
        R8x16 => {
            let mut t = u16::read_ne(a) as u32 & mask as u32;
            t += u32::read_ne(l) & mask as u32;
            t = (t >> 6).wrapping_mul(mul as u32);
            (t >> 24) as i32 - 2 - 4
        }
        R16x8 => {
            let mut t = u32::read_ne(a) & mask as u32;
            t += u16::read_ne(l) as c_uint & mask as u32;
            t = (t >> 6).wrapping_mul(mul as u32);
            (t >> 24) as i32 - 4 - 2
        }
        R16x32 => {
            let mut t = (u32::read_ne(a) & mask as u32) as u64;
            t += u64::read_ne(l) & mask;
            t = (t >> 6).wrapping_mul(mul);
            (t >> 56) as i32 - 4 - 8
        }
        R32x16 => {
            let mut t = u64::read_ne(a) & mask;
            t += (u32::read_ne(l) & mask as u32) as u64;
            t = (t >> 6).wrapping_mul(mul);
            (t >> 56) as i32 - 8 - 4
        }
        R32x64 => {
            let mut t = (u64::read_ne(&a[0..]) & mask) >> 6;
            t += (u64::read_ne(&l[0..]) & mask) >> 6;
            t += (u64::read_ne(&l[8..]) & mask) >> 6;
            t = t.wrapping_mul(mul);
            (t >> 56) as i32 - 8 - 16
        }
        R64x32 => {
            let mut t = (u64::read_ne(&a[0..]) & mask) >> 6;
            t += (u64::read_ne(&a[8..]) & mask) >> 6;
            t += (u64::read_ne(&l[0..]) & mask) >> 6;
            t = t.wrapping_mul(mul);
            (t >> 56) as i32 - 16 - 8
        }
        R4x16 => {
            let mut t = u8::read_ne(a) as u32 & mask as u32;
            t += u32::read_ne(l) & mask as u32;
            t = (t >> 6).wrapping_mul(mul as u32);
            (t >> 24) as i32 - 1 - 4
        }
        R16x4 => {
            let mut t = u32::read_ne(a) & mask as u32;
            t += u8::read_ne(l) as u32 & mask as u32;
            t = (t >> 6).wrapping_mul(mul as u32);
            (t >> 24) as i32 - 4 - 1
        }
        R8x32 => {
            let mut t = (u16::read_ne(a) as u32 & mask as u32) as u64;
            t += u64::read_ne(l) & mask;
            t = (t >> 6).wrapping_mul(mul);
            (t >> 56) as i32 - 2 - 8
        }
        R32x8 => {
            let mut t = u64::read_ne(a) & mask;
            t += (u16::read_ne(l) as u32 & mask as u32) as u64;
            t = (t >> 6).wrapping_mul(mul);
            (t >> 56) as i32 - 8 - 2
        }
        R16x64 => {
            let mut t = (u32::read_ne(a) & mask as u32) as u64;
            t += u64::read_ne(&l[0..]) & mask;
            t = (t >> 6) + ((u64::read_ne(&l[8..]) & mask) >> 6);
            t = t.wrapping_mul(mul);
            (t >> 56) as i32 - 4 - 16
        }
        R64x16 => {
            let mut t = u64::read_ne(&a[0..]) & mask;
            t += (u32::read_ne(l) & mask as u32) as u64;
            t = (t >> 6) + ((u64::read_ne(&a[8..]) & mask) >> 6);
            t = t.wrapping_mul(mul);
            (t >> 56) as i32 - 16 - 4
        }
    };

    (s != 0) as c_uint + (s > 0) as c_uint
}

/// This is used for `lo_ctx`, which is used for a lookup into `lo_cdf`.
/// `lo_cdf` corresponds roughly to `Coeff_Base_Cdf` in the spec.
/// `rav1d`/`dav1d` use a maximum value of 40 (for an array size of 41),
/// whereas the spec has the array being 42 elements.
type LoCdfIndex = InRange<usize, 0, 40>;

#[inline]
fn get_lo_ctx(
    levels: &[u8],
    tx_class: TxClass,
    hi_mag: &mut u32,
    ctx_offsets: Option<&[[LoCtxOffset; 32]; 32]>,
    x: u8,
    y: u8,
    stride: u8,
) -> LoCdfIndex {
    let stride = stride as usize;
    let level = |y, x| levels[y * stride + x] as u32;

    // Stride:
    //  - In the `H | V` cases, `stride = 16`.
    //  - In the `TwoD` case, `stride = 4 << slh`.
    //    Max `slh` is `TxfmSize::S32x32 as u8`, which is 3.
    //    Max `stride` is `4 << 3 = 32`.
    //
    // We access level pairs (0, 1), (0, 2), (0, 3), (0, 4), (1, 0), (1, 1), and (2, 0),
    // which means the maximum index we access is:
    //  - `2 * 16 + 0 = 32` in the `H | V` cases
    //  - `2 * 32 + 0 = 64` in the `TwoD` case
    // Therefore, the caller must ensure that `levels` has at least 33 or 65 elements.

    let mut mag = level(0, 1) + level(1, 0);
    let offset;
    match ctx_offsets {
        Some(ctx_offsets) => {
            debug_assert_matches!(tx_class, TxClass::TwoD);
            mag += level(1, 1);
            *hi_mag = mag;
            mag += level(0, 2) + level(2, 0);
            offset = ctx_offsets[y as usize][x as usize].get();
            // The type for `ctx_offsets` guarantees bounds.
            assert!(offset <= 21); // Elided
        }
        None => {
            debug_assert_matches!(tx_class, TxClass::H | TxClass::V);
            mag += level(0, 2);
            *hi_mag = mag;
            mag += level(0, 3) + level(0, 4);
            offset = 26 + if y > 1 { 10 } else { y * 5 };
            // This follows from the definition of offset.
            assert!(offset <= 36); // Elided
        }
    }

    // What is the maxiumum value of `lo_ctx`, the result?
    // The callers are using it as a lookup into `lo_cdf`, which has size 41.
    // Can we statically prove the our return value is less than 41?
    //
    // In the `H | V` cases, the maximum value of `offset` is `26 + 10 = 36`.
    // In the `TwoD` case, `offset` takes a value from `ctx_offsets`.
    // The highest value in the underlying table is 21, and we guarantee that with types.
    //
    // So at this point, `offset <= 36`. However, it seems the compiler needs
    // some help to unify the offset maxima from the branches.
    assert!(offset <= 36); // Elided

    let lo_ctx = offset as usize
        + if mag > 512 {
            4
        } else {
            // `mag <= 512` in this branch, so the maximum value is `576 >> 7 == 4`
            ((mag + 64) >> 7) as u8 as usize
        } as usize;

    // `36 + 4 == 40`.
    LoCdfIndex::new(lo_ctx).unwrap() // Elided
}

/// rav1g: for a transform size and class, the scan index of the coefficient at each position `rc` of
/// the block as `decode_coefs` numbers them: the inverse of what its token loop does with `i`.
fn inverse_scan(tx: TxfmSize, tx_class: TxClass) -> &'static [u16] {
    static TABLES: OnceLock<Vec<Box<[u16]>>> = OnceLock::new();
    let tables = TABLES.get_or_init(|| {
        let mut tables = Vec::with_capacity(DAV1D_SCANS.len() * 3);
        for (tx, scan) in DAV1D_SCANS.iter().enumerate() {
            let t_dim = &DAV1D_TXFM_DIMENSIONS[tx];
            let slw = cmp::min(t_dim.lw, TxfmSize::S32x32 as u8) as u32;
            let slh = cmp::min(t_dim.lh, TxfmSize::S32x32 as u8) as u32;
            let n = (4usize << slw) * (4usize << slh);
            for tx_class in [TxClass::TwoD, TxClass::H, TxClass::V] {
                let mut inverse = vec![0u16; n].into_boxed_slice();
                for i in 0..n {
                    let rc = match tx_class {
                        TxClass::TwoD => scan[i].get() as usize,
                        TxClass::H => i,
                        TxClass::V => {
                            let x = i & ((4 << slw) - 1);
                            let y = i >> (slw + 2);
                            x << (slh + 2) | y
                        }
                    };
                    inverse[rc] = i as u16;
                }
                tables.push(inverse);
            }
        }
        tables
    });
    &tables[tx as usize * 3 + tx_class as usize]
}

/// rav1g: the coefficients do not stay in the task's or the frame's block (`cf` of upstream). The token
/// loop uses the task's block as before; the loop that reads signs and remainders writes each
/// dequantised coefficient where the sink says, at its index in scan order, 32 bits wide and not
/// clamped to the range of the bit depth, and leaves the task's block zero. `pos` is the position of
/// the transform block in 4x4 units of its plane. The result is the index of the last coefficient, -1
/// for a block without one; `Err` when the sink has no room.
fn decode_coefs<BD: BitDepth>(
    c: &Rav1dContext,
    pos: (c_int, c_int),
    f: &Rav1dFrameData,
    ts: usize,
    ts_c: &mut Rav1dTileStateContext,
    dbg_block_info: bool,
    scratch: &mut TaskContextScratch,
    t_cf: &mut Cf,
    a: &mut [u8],
    l: &mut [u8],
    tx: TxfmSize,
    bs: BlockSize,
    b: &Av1Block,
    plane: usize,
    txtp: &mut TxfmType,
    res_ctx: &mut u8,
) -> Result<c_int, ()> {
    let ts_idx = ts;
    let dc_sign_ctx;
    let dc_sign;
    let mut dc_dq;
    let ts = &f.ts[ts];
    let chroma = plane != 0;
    let frame_hdr = &***f.frame_hdr.as_ref().unwrap();
    let lossless = frame_hdr.segmentation.lossless[b.seg_id.get()];
    let t_dim = &DAV1D_TXFM_DIMENSIONS[tx as usize];
    #[expect(clippy::overly_complex_bool_expr, reason = "used for debugging")]
    let dbg = dbg_block_info && plane != 0 && false;

    if dbg {
        println!("Start: r={}", ts_c.msac.rng);
    }

    // does this block have any non-zero coefficients
    let sctx = get_skip_ctx(t_dim, bs, a, l, chroma, f.cur.p.layout);
    let all_skip = rav1d_msac_decode_bool_adapt(
        &mut ts_c.msac,
        &mut ts_c.cdf.coef.skip[t_dim.ctx as usize][sctx.get() as usize],
    );
    if dbg {
        println!(
            "Post-non-zero[{}][{}][{}]: r={}",
            t_dim.ctx, sctx, all_skip, ts_c.msac.rng,
        );
    }
    if all_skip {
        *res_ctx = 0x40;
        *txtp = if lossless { WHT_WHT } else { DCT_DCT };
        return Ok(-1);
    }

    // transform type (chroma: derived, luma: explicitly coded)
    use Av1BlockIntraInter::*;
    *txtp = match &b.ii {
        _ if lossless => {
            assert!(t_dim.max == TxfmSize::S4x4 as _);
            WHT_WHT
        }
        Intra(_) if t_dim.max >= TxfmSize::S32x32 as _ => DCT_DCT,
        Inter(_) if t_dim.max >= TxfmSize::S64x64 as _ => DCT_DCT,
        Intra(intra) if chroma => DAV1D_TXTP_FROM_UVMODE[intra.uv_mode as usize],
        // inferred from either the luma txtp (inter) or a LUT (intra)
        Inter(_) if chroma => get_uv_inter_txtp(t_dim, *txtp),
        // In libaom, lossless is checked by a literal qidx == 0, but not all
        // such blocks are actually lossless. The remainder gets an implicit
        // transform type (for luma)
        _ if frame_hdr.segmentation.qidx[b.seg_id.get()] == 0 => DCT_DCT,
        Intra(intra) => {
            let y_mode_nofilt = if intra.y_mode == FILTER_PRED {
                DAV1D_FILTER_MODE_TO_Y_MODE[intra.y_angle as usize]
            } else {
                intra.y_mode
            };
            let idx;
            let txtp = if frame_hdr.reduced_txtp_set != 0 || t_dim.min == TxfmSize::S16x16 as _ {
                idx = rav1d_msac_decode_symbol_adapt8(
                    &mut ts_c.msac,
                    &mut ts_c.cdf.m.txtp_intra2[t_dim.min as usize][y_mode_nofilt as usize],
                    4,
                );
                DAV1D_TX_TYPES_PER_SET[idx as usize + 0]
            } else {
                idx = rav1d_msac_decode_symbol_adapt8(
                    &mut ts_c.msac,
                    &mut ts_c.cdf.m.txtp_intra1[t_dim.min as usize][y_mode_nofilt as usize],
                    6,
                );
                DAV1D_TX_TYPES_PER_SET[idx as usize + 5]
            };
            if dbg {
                println!(
                    "Post-txtp-intra[{:?}->{}][{}][{}->{}]: r={}",
                    tx, t_dim.min, y_mode_nofilt, idx, txtp, ts_c.msac.rng,
                );
            }
            txtp
        }
        Inter(_) => {
            let idx;
            let txtp = if frame_hdr.reduced_txtp_set != 0 || t_dim.max == TxfmSize::S32x32 as _ {
                let bool_idx = rav1d_msac_decode_bool_adapt(
                    &mut ts_c.msac,
                    &mut ts_c.cdf.m.txtp_inter3[t_dim.min as usize],
                );
                idx = bool_idx as u8;
                if bool_idx {
                    DCT_DCT
                } else {
                    IDTX
                }
            } else if t_dim.min == TxfmSize::S16x16 as _ {
                idx = rav1d_msac_decode_symbol_adapt16(
                    &mut ts_c.msac,
                    &mut ts_c.cdf.m.txtp_inter2.0,
                    11,
                );
                DAV1D_TX_TYPES_PER_SET[idx as usize + 12]
            } else {
                idx = rav1d_msac_decode_symbol_adapt16(
                    &mut ts_c.msac,
                    &mut ts_c.cdf.m.txtp_inter1[t_dim.min as usize],
                    15,
                );
                DAV1D_TX_TYPES_PER_SET[idx as usize + 24]
            };
            if dbg {
                println!(
                    "Post-txtp-inter[{:?}->{}][{}->{}]: r={}",
                    tx, t_dim.min, idx, txtp, ts_c.msac.rng,
                );
            }
            txtp
        }
    };

    // find end-of-block (eob)
    let tx2dszctx =
        cmp::min(t_dim.lw, TxfmSize::S32x32 as u8) + cmp::min(t_dim.lh, TxfmSize::S32x32 as u8);
    let tx_class = DAV1D_TX_TYPE_CLASS[*txtp as usize];
    let chroma = chroma as usize;
    let is_1d = (tx_class != TxClass::TwoD) as usize;
    let eob_bin = match tx2dszctx {
        0 => {
            let eob_bin_cdf = &mut ts_c.cdf.coef.eob_bin_16[chroma][is_1d];
            rav1d_msac_decode_symbol_adapt8(&mut ts_c.msac, eob_bin_cdf, 4 + 0)
        }
        1 => {
            let eob_bin_cdf = &mut ts_c.cdf.coef.eob_bin_32[chroma][is_1d];
            rav1d_msac_decode_symbol_adapt8(&mut ts_c.msac, eob_bin_cdf, 4 + 1)
        }
        2 => {
            let eob_bin_cdf = &mut ts_c.cdf.coef.eob_bin_64[chroma][is_1d];
            rav1d_msac_decode_symbol_adapt8(&mut ts_c.msac, eob_bin_cdf, 4 + 2)
        }
        3 => {
            let eob_bin_cdf = &mut ts_c.cdf.coef.eob_bin_128[chroma][is_1d];
            rav1d_msac_decode_symbol_adapt8(&mut ts_c.msac, eob_bin_cdf, 4 + 3)
        }
        4 => {
            let eob_bin_cdf = &mut ts_c.cdf.coef.eob_bin_256[chroma][is_1d];
            rav1d_msac_decode_symbol_adapt16(&mut ts_c.msac, eob_bin_cdf, 4 + 4)
        }
        5 => {
            let eob_bin_cdf = &mut ts_c.cdf.coef.eob_bin_512[chroma];
            rav1d_msac_decode_symbol_adapt16(&mut ts_c.msac, eob_bin_cdf, 4 + 5)
        }
        6 => {
            let eob_bin_cdf = &mut ts_c.cdf.coef.eob_bin_1024[chroma];
            rav1d_msac_decode_symbol_adapt16(&mut ts_c.msac, eob_bin_cdf, 4 + 6)
        }
        // `tx2dszctx` is `cmp::min(_, 3) + cmp::min(_, 3)`, where `TX_32X32 as u8 == 3`,
        // and we cover `0..=6`.  `rustc` should eliminate this.
        _ => unreachable!(),
    };
    if dbg {
        println!(
            "Post-eob_bin_{}[{}][{}][{}]: r={}",
            16 << tx2dszctx,
            chroma,
            is_1d,
            eob_bin,
            ts_c.msac.rng,
        );
    }
    let eob = if eob_bin > 1 {
        let eob_hi_bit_cdf =
            &mut ts_c.cdf.coef.eob_hi_bit[t_dim.ctx as usize][chroma][eob_bin as usize];
        let eob_hi_bit = rav1d_msac_decode_bool_adapt(&mut ts_c.msac, eob_hi_bit_cdf) as u16;
        if dbg {
            println!(
                "Post-eob_hi_bit[{}][{}][{}][{}]: r={}",
                t_dim.ctx, chroma, eob_bin, eob_hi_bit, ts_c.msac.rng,
            );
        }
        let eob = ((eob_hi_bit | 2) << (eob_bin - 2))
            | rav1d_msac_decode_bools(&mut ts_c.msac, eob_bin - 2) as u16;
        if dbg {
            println!("Post-eob[{}]: r={}", eob, ts_c.msac.rng);
        }
        eob
    } else {
        eob_bin as u16
    };

    struct Cf<'a, BD: BitDepth>(&'a mut [BD::Coef]);

    impl<'a, BD: BitDepth> Cf<'a, BD> {
        fn index(&self, rc: u16) -> usize {
            let i = rc as usize & (self.0.len() - 1);
            // SAFETY: `self.0.len()` is either `cf_len` or `CF_LEN`,
            // both of which are powers of 2.
            // `cf_len` is a power of 2 since it's from `1 << n`, etc.
            // Thus, `& (self.0.len() - 1)` is the same as `% self.0.len()`.
            unsafe { assert_unchecked(i < self.0.len()) };
            i
        }

        #[cfg_attr(debug_assertions, track_caller)]
        pub fn get(&self, rc: u16) -> i32 {
            self.0[self.index(rc)].into()
        }

        #[cfg_attr(debug_assertions, track_caller)]
        pub fn set<T: ToPrimitive<BD::Coef>>(&mut self, rc: u16, value: T) {
            self.0[self.index(rc)] = value.as_();
        }
    }

    let cf = t_cf.select_mut::<BD>();
    let mut cf = Cf::<BD>(cf);

    // rav1g: the transform block goes to the sink, which says where its coefficients go.
    let dst = c.sink.txb(
        f.sink_frame,
        ts_idx,
        &ParseTxb {
            plane: plane as u8,
            tx,
            txtp: *txtp,
            eob,
            x4: pos.0,
            y4: pos.1,
        },
    );
    if dst.is_null() {
        return Err(());
    }
    let inverse = inverse_scan(tx, tx_class);
    // The coefficient at `rc` of the task's block: written to the sink's buffer, zero in the block.
    let put = |cf: &mut Cf<BD>, rc: u16, value: c_int| {
        cf.set(rc, 0);
        // SAFETY: the sink's buffer has room for the coefficients up to `eob` (`ParseSink::txb`), and
        // the coefficient at `rc` is one of them: `inverse` has the scan index of every position.
        unsafe { dst.add(inverse[rc as usize & (inverse.len() - 1)] as usize).write(value) };
    };

    // base tokens
    let mut rc;
    let mut dc_tok;

    #[inline]
    fn decode_coefs_class<const TX_CLASS: usize, BD: BitDepth>(
        ts_c: &mut Rav1dTileStateContext,
        t_dim: &TxfmInfo,
        chroma: usize,
        scratch: &mut TaskContextScratch,
        eob: u16,
        tx: TxfmSize,
        dbg: bool,
        cf: &mut Cf<BD>,
    ) -> (u16, u32) {
        let tx_class = const { TxClass::from_repr(TX_CLASS) }.unwrap();

        let eob_cdf = &mut ts_c.cdf.coef.eob_base_tok[t_dim.ctx as usize][chroma];
        let hi_cdf = &mut ts_c.cdf.coef.br_tok[cmp::min(t_dim.ctx, 3) as usize][chroma];

        let lo_cdf = &mut ts_c.cdf.coef.base_tok[t_dim.ctx as usize][chroma];
        let levels = scratch.inter_intra_mut().levels_pal.levels_mut();

        let slw = cmp::min(t_dim.lw, TxfmSize::S32x32 as u8);
        let slh = cmp::min(t_dim.lh, TxfmSize::S32x32 as u8);
        let tx2dszctx = slw + slh;

        // eob
        let mut ctx =
            1 + (eob > (2 << tx2dszctx) as u16) as u8 + (eob > (4 << tx2dszctx) as u16) as u8;
        let eob_tok =
            rav1d_msac_decode_symbol_adapt4(&mut ts_c.msac, &mut eob_cdf[ctx as usize], 2);
        let mut tok = eob_tok + 1;
        let mut level_tok = tok * 0x41;
        let mut mag = 0;

        let lo_ctx_offsets;
        let scan;
        let stride;
        match tx_class {
            TxClass::TwoD => {
                let is_rect = tx.is_rect() as usize;
                lo_ctx_offsets = Some(&DAV1D_LO_CTX_OFFSETS[is_rect + (tx as usize & is_rect)]);
                scan = DAV1D_SCANS[tx as usize];
                stride = 4 << slh;
            }
            TxClass::H | TxClass::V => {
                lo_ctx_offsets = None;
                scan = &[];
                stride = 16;
            }
        }

        let shift;
        let shift2;
        let mask;
        let end: usize;
        match tx_class {
            TxClass::TwoD => {
                shift = slh + 2;
                shift2 = 0;
                mask = (4 << slh) - 1;
                // We need to zero part of `levels`. We want to calulate the end
                // of the slice to zero in a way that rust can elide the bounds
                // check. The size of the `levels` array is 1088 bytes.
                //
                // The original calculation here:
                // `end = stride as usize * ((4 << slw) as usize + 2)`
                //
                // We know:
                //  - `stride = 4 << slh`, where `slh <= 3`.
                //    Therefore, `stride <= 32`.
                //
                //  - `slw <= 3`. Therefore, `4 << slw <= 32`
                //
                // Therefore, we have the maximum `end` value as
                //  `32 * (32 + 2) = 1088`
                // Therefore, no bounds check should be required to slice `levels`.
                //
                // However, as of nightly 2025-05-01, rustc had a lot of trouble
                // proving that to itself. Here we give it a hand. First, we
                // rearrange:
                //    `stride * ((4 << slw) + 2)`
                //  `= (4 << slh) * ((4 << slw) + 2)`
                // This gets a lot easier if we shift from << to *2^x
                //  `= (4 * 2^slh) * (4 * 2^slw + 2)`
                //  `= (2^(slh+2)) * (2^(slw+2) + 2)`
                //  `= 2^(slh+2) * 2(slw + 2) + 2^(slh+2)*2`
                //  `= 2^(slh+slw+4) + 2^(slh+3)`
                //  `= 16 * 2^(slh+slw) + 8*2^slh`
                //  `= 16 << (slh + slw) + 8 << slh`
                //
                // With suitable asserts, rustc can handle this.
                //
                // Interestingly, regardless of `slw`, these two terms don't have overlapping bits,
                // so we can replace the + with a bitwise or
                //  `= 16 << (slh + slw) | 8 << slh`
                // However, this confuses rustc again! So we just use an add!
                let term1: usize = 16 << (slw + slh);
                assert!(term1 <= 1024); // Elided
                let term2: usize = 8 << slh;
                assert!(term2 <= 64); // Elided
                end = term1 + term2;
            }
            TxClass::H => {
                shift = slh + 2;
                shift2 = 0;
                mask = (4 << slh) - 1;
                end = stride as usize * ((4 << slh) as usize + 2);
            }
            TxClass::V => {
                shift = slw + 2;
                shift2 = slh + 2;
                mask = (4 << slw) - 1;
                end = stride as usize * ((4 << slw) as usize + 2);
            }
        }

        // `.fill(0)` will become a call the the platform C library's `bzero` or `memset` function.
        // That is likely to be a version optimised for the specific CPU with appropriate vectoring.
        //
        // The value of `end` is known statically to not exceed the size of `levels`,
        // so there's also no bounds check.
        levels[..end].fill(0);

        let mut rc;
        let mut x;
        let mut y;
        match tx_class {
            TxClass::TwoD => {
                rc = scan[eob as usize].get();
                x = (rc >> shift) as u8;
                y = rc as u8 & mask;
            }
            TxClass::H => {
                // Transposing reduces the stride and padding requirements.
                x = eob as u8 & mask;
                y = (eob >> shift) as u8;
                rc = eob as u16;
            }
            TxClass::V => {
                x = eob as u8 & mask;
                y = (eob >> shift) as u8;
                rc = (x as u16) << shift2 | y as u16;
            }
        }
        if dbg {
            println!(
                "Post-lo_tok[{}][{}][{}][{}={}={}]: r={}",
                t_dim.ctx, chroma, ctx, eob, rc, tok, ts_c.msac.rng,
            );
        }
        if eob_tok == 2 {
            ctx = if if tx_class == TxClass::TwoD {
                (x | y) > 1
            } else {
                y != 0
            } {
                14
            } else {
                7
            };
            tok = rav1d_msac_decode_hi_tok(&mut ts_c.msac, &mut hi_cdf[ctx as usize]);
            level_tok = tok + (3 << 6);
            if dbg {
                println!(
                    "Post-hi_tok[{}][{}][{}][{}={}={}]: r={}",
                    cmp::min(t_dim.ctx, 3),
                    chroma,
                    ctx,
                    eob,
                    rc,
                    tok,
                    ts_c.msac.rng,
                );
            }
        }
        cf.set(rc, tok.to::<i16>() << 11);
        let level_off = if tx_class == TxClass::TwoD {
            rc as usize
        } else {
            x as usize * stride as usize + y as usize
        };
        levels[level_off] = level_tok as u8;

        for i in (1..eob).rev() {
            // ac
            let rc_i;
            match tx_class {
                TxClass::TwoD => {
                    rc_i = scan[i as usize].get();
                    x = (rc_i >> shift) as u8;
                    y = rc_i as u8 & mask;
                }
                TxClass::H => {
                    x = i as u8 & mask;
                    y = (i >> shift) as u8;
                    rc_i = i as u16;
                }
                TxClass::V => {
                    x = i as u8 & mask;
                    y = (i >> shift) as u8;
                    rc_i = (x as u16) << shift2 | y as u16;
                }
            }
            debug_assert!(x < 32 && y < 32);
            x %= 32;
            y %= 32;
            // The caller of `get_lo_ctx` must ensure that there are at least
            // 65 elements in `level` in the `TwoD` case, and 33 elements for `H | V`.
            // The size of `levels` is `32 * 34`, so `level_off` must be such that
            // - `H | V`: `32 * 34 - level_off >= 33` => `level_off <= 1055`
            // - `TwoD`: `32 * 34 - level_off >= 65` => `level_off <= 1023`
            let level_off = match tx_class {
                // `rc_i` is from `scan[i]`, which has type `Scan`,
                // which has a max of 1023 by virtue of the type definition.
                // That's the max from the calculation above.
                // Therefore, we are okay.
                TxClass::TwoD => rc_i as usize,

                // Max `x` and `y` are 31 and `stride = 16`.
                // Therefore, max `level_off` is 527, which is okay.
                TxClass::H | TxClass::V => x as usize * stride as usize + y as usize,
            };
            // At this point, we know that `get_lo_ctx` can elide the bounds check on `level`
            // because it is statically known that `level` has at least 65 elements.
            let level = &mut levels[level_off..];
            let lo_ctx = get_lo_ctx(level, tx_class, &mut mag, lo_ctx_offsets, x, y, stride);
            if tx_class == TxClass::TwoD {
                y |= x;
            }
            tok = rav1d_msac_decode_symbol_adapt4(&mut ts_c.msac, &mut lo_cdf[lo_ctx.get()], 3);
            if dbg {
                println!(
                    "Post-lo_tok[{}][{}][{}][{}={}={}]: r={}",
                    t_dim.ctx, chroma, ctx, i, rc_i, tok, ts_c.msac.rng,
                );
            }
            if tok == 3 {
                let mag = mag as u8 & 63;
                ctx = if y > (tx_class == TxClass::TwoD) as u8 {
                    14
                } else {
                    7
                } + if mag > 12 { 6 } else { (mag + 1) >> 1 };
                tok = rav1d_msac_decode_hi_tok(&mut ts_c.msac, &mut hi_cdf[ctx as usize]);
                if dbg {
                    println!(
                        "Post-hi_tok[{}][{}][{}][{}={}={}]: r={}",
                        cmp::min(t_dim.ctx, 3),
                        chroma,
                        ctx,
                        i,
                        rc_i,
                        tok,
                        ts_c.msac.rng,
                    );
                }
                level[0] = (tok + (3 << 6)) as u8;
                cf.set(rc_i, ((tok as u16) << 11) | rc);
                rc = rc_i;
            } else {
                // `0x1` for `tok`, `0x7ff` as bitmask for `rc`, `0x41` for `level_tok`.
                let tok = tok as u32 * 0x17ff41;
                level[0] = tok as u8;

                let tok_check = if tok != 0 {
                    ((tok as u16) << 11) | rc
                } else {
                    0
                };

                // This is optimized differently from C to avoid branches,
                // as simple branches are not always optimized to branchless `cmov`s.
                let mask = tok >> 9;
                let tok = mask & (rc as u32 + !0x7ff);
                let mask = mask as u16;
                rc = (rc_i & mask) | (rc & !mask);

                debug_assert!(tok == tok_check as u32);
                cf.set(rc_i, tok);
            }
        }
        // dc
        let lo_ctx = if tx_class == TxClass::TwoD {
            0
        } else {
            // The caller of `get_lo_ctx` must ensure that there are at least
            // 65 elements in level in the `TwoD` case, and 33 elements for `H | V`.
            // The size of `levels` is 32 * 34, and we are not offsetting into it,
            // so this is trivially true.
            get_lo_ctx(levels, tx_class, &mut mag, lo_ctx_offsets, 0, 0, stride).get()
        };
        let mut dc_tok =
            rav1d_msac_decode_symbol_adapt4(&mut ts_c.msac, &mut lo_cdf[lo_ctx], 3) as c_uint;
        if dbg {
            println!(
                "Post-dc_lo_tok[{}][{}][{}][{}]: r={}",
                t_dim.ctx, chroma, ctx, dc_tok, ts_c.msac.rng,
            );
        }
        if dc_tok == 3 {
            if tx_class == TxClass::TwoD {
                mag = levels[0 * stride as usize + 1] as c_uint
                    + levels[1 * stride as usize + 0] as c_uint
                    + levels[1 * stride as usize + 1] as c_uint;
            }
            let mag = mag as u8 & 63;
            ctx = if mag > 12 { 6 } else { (mag + 1) >> 1 };
            dc_tok = rav1d_msac_decode_hi_tok(&mut ts_c.msac, &mut hi_cdf[ctx as usize]) as c_uint;
            if dbg {
                println!(
                    "Post-dc_hi_tok[{}][{}][0][{}]: r={}",
                    cmp::min(t_dim.ctx, 3),
                    chroma,
                    dc_tok,
                    ts_c.msac.rng,
                );
            }
        }

        (rc, dc_tok)
    }

    if eob != 0 {
        let cf = &mut cf;
        (rc, dc_tok) = match tx_class {
            TxClass::TwoD => decode_coefs_class::<{ TxClass::TwoD as _ }, BD>(
                ts_c, t_dim, chroma, scratch, eob, tx, dbg, cf,
            ),
            TxClass::H => decode_coefs_class::<{ TxClass::H as _ }, BD>(
                ts_c, t_dim, chroma, scratch, eob, tx, dbg, cf,
            ),
            TxClass::V => decode_coefs_class::<{ TxClass::V as _ }, BD>(
                ts_c, t_dim, chroma, scratch, eob, tx, dbg, cf,
            ),
        };
    } else {
        let eob_cdf = &mut ts_c.cdf.coef.eob_base_tok[t_dim.ctx as usize][chroma];
        let hi_cdf = &mut ts_c.cdf.coef.br_tok[cmp::min(t_dim.ctx, 3) as usize][chroma];

        // dc-only
        let tok_br = rav1d_msac_decode_symbol_adapt4(&mut ts_c.msac, &mut eob_cdf[0], 2) as c_uint;
        dc_tok = 1 + tok_br;
        if dbg {
            println!(
                "Post-dc_lo_tok[{}][{}][{}][{}]: r={}",
                t_dim.ctx, chroma, 0, dc_tok, ts_c.msac.rng,
            );
        }
        if tok_br == 2 {
            dc_tok = rav1d_msac_decode_hi_tok(&mut ts_c.msac, &mut hi_cdf[0]) as c_uint;
            if dbg {
                println!(
                    "Post-dc_hi_tok[{}][{}][0][{}]: r={}",
                    cmp::min(t_dim.ctx, 3),
                    chroma,
                    dc_tok,
                    ts_c.msac.rng,
                );
            }
        }
        rc = 0;
    }

    // residual and sign
    let dq = match ts.dq.get() {
        TileStateRef::Frame => &f.dq,
        TileStateRef::Local => &ts.dqmem,
    };
    let dq_tbl = &dq[b.seg_id.get()][plane];
    let qm_tbl = if *txtp < IDTX {
        f.qm[tx as usize][plane]
    } else {
        None
    };
    let dq_shift = cmp::max(0, t_dim.ctx as c_int - 2);
    let cf_max = !(!127u32
        << (match BD::BPC {
            BPC::BPC8 => 8,
            BPC::BPC16 => f.cur.p.bpc,
        })) as i32;
    let mut cul_level: c_uint;
    let dc_sign_level: c_uint;

    enum Ac<'a> {
        Qm(&'a [u8]),
        NoQm,
    }

    let ac;
    if dc_tok == 0 {
        cul_level = 0;
        dc_sign_level = 1 << 6;
        ac = Some(match qm_tbl {
            Some(qm_tbl) => Ac::Qm(qm_tbl),
            None => Ac::NoQm,
        });
    } else {
        dc_sign_ctx = get_dc_sign_ctx(tx, a, l) as c_int;
        let dc_sign_cdf = &mut ts_c.cdf.coef.dc_sign[chroma][dc_sign_ctx as usize];
        dc_sign = rav1d_msac_decode_bool_adapt(&mut ts_c.msac, dc_sign_cdf) as c_int;
        if dbg {
            println!(
                "Post-dc_sign[{}][{}][{}]: r={}",
                chroma, dc_sign_ctx, dc_sign, ts_c.msac.rng,
            );
        }

        dc_dq = dq_tbl[0].get() as c_int;
        dc_sign_level = (dc_sign - 1 & 2 << 6) as c_uint;

        if let Some(qm_tbl) = qm_tbl {
            dc_dq = dc_dq * qm_tbl[0] as c_int + 16 >> 5;

            if dc_tok == 15 {
                dc_tok = (read_golomb(&mut ts_c.msac)).wrapping_add(15);
                if dbg {
                    println!(
                        "Post-dc_residual[{}->{}]: r={}",
                        dc_tok.wrapping_sub(15),
                        dc_tok,
                        ts_c.msac.rng,
                    );
                }

                dc_tok &= 0xfffff;
                dc_dq = ((dc_dq as c_uint).wrapping_mul(dc_tok as c_uint) & 0xffffff) as c_int;
            } else {
                dc_dq = (dc_dq as c_uint).wrapping_mul(dc_tok as c_uint) as c_int;
                assert!(dc_dq <= 0xffffff);
            }
            cul_level = dc_tok;
            dc_dq >>= dq_shift;
            put(&mut cf, 0, if dc_sign != 0 { -dc_dq } else { dc_dq });

            ac = if rc != 0 { Some(Ac::Qm(qm_tbl)) } else { None };
        } else {
            // non-qmatrix is the common case and allows for additional optimizations
            if dc_tok == 15 {
                dc_tok = (read_golomb(&mut ts_c.msac)).wrapping_add(15);
                if dbg {
                    println!(
                        "Post-dc_residual[{}->{}]: r={}",
                        dc_tok.wrapping_sub(15),
                        dc_tok,
                        ts_c.msac.rng,
                    );
                }

                dc_tok &= 0xfffff;
                dc_dq = (((dc_dq as c_uint).wrapping_mul(dc_tok as c_uint)
                    & 0xffffff as c_int as c_uint)
                    >> dq_shift) as c_int;
            } else {
                dc_dq = ((dc_dq as c_uint).wrapping_mul(dc_tok as c_uint) >> dq_shift) as c_int;
                assert!(dc_dq <= cf_max);
            }
            cul_level = dc_tok;
            put(&mut cf, 0, if dc_sign != 0 { -dc_dq } else { dc_dq });

            ac = if rc != 0 { Some(Ac::NoQm) } else { None };
        }
    }
    match ac {
        Some(Ac::Qm(qm_tbl)) => {
            let ac_dq: c_uint = dq_tbl[1].get() as c_uint;
            loop {
                let sign = rav1d_msac_decode_bool_equi(&mut ts_c.msac);
                if dbg {
                    println!("Post-sign[{}={}]: r={}", rc, sign, ts_c.msac.rng);
                }
                let rc_tok = cf.get(rc) as u32;
                let mut tok;
                let mut dq: c_uint = ac_dq
                    .wrapping_mul(qm_tbl[rc as usize] as c_uint)
                    .wrapping_add(16)
                    >> 5;
                let dq_sat;

                if rc_tok >= 15 << 11 {
                    tok = (read_golomb(&mut ts_c.msac)).wrapping_add(15);
                    if dbg {
                        println!(
                            "Post-residual[{}={}->{}]: r={}",
                            rc,
                            tok.wrapping_sub(15),
                            tok,
                            ts_c.msac.rng,
                        );
                    }

                    tok &= 0xfffff;
                    dq = dq.wrapping_mul(tok) & 0xffffff;
                } else {
                    tok = rc_tok >> 11;
                    dq = dq.wrapping_mul(tok);
                    assert!(dq <= 0xffffff); // Optimized out.
                }
                cul_level = cul_level.wrapping_add(tok);
                dq >>= dq_shift;
                dq_sat = dq as c_int;
                put(&mut cf, rc, if sign { -dq_sat } else { dq_sat });

                rc = rc_tok as u16 & 0x3ff;
                if !(rc != 0) {
                    break;
                }
            }
        }
        Some(Ac::NoQm) => {
            let ac_dq: c_uint = dq_tbl[1].get() as c_uint;
            loop {
                let sign = rav1d_msac_decode_bool_equi(&mut ts_c.msac) as c_int;
                if dbg {
                    println!("Post-sign[{}={}]: r={}", rc, sign, ts_c.msac.rng);
                }
                let rc_tok = cf.get(rc) as u32;
                let mut tok: c_uint;
                let mut dq;

                // residual
                if rc_tok >= 15 << 11 {
                    tok = (read_golomb(&mut ts_c.msac)).wrapping_add(15);
                    if dbg {
                        println!(
                            "Post-residual[{}={}->{}]: r={}",
                            rc,
                            tok.wrapping_sub(15),
                            tok,
                            ts_c.msac.rng,
                        );
                    }

                    // coefficient parsing, see 5.11.39
                    tok &= 0xfffff;

                    // dequant, see 7.12.3
                    dq = ((ac_dq.wrapping_mul(tok) & 0xffffff) >> dq_shift) as c_int;
                } else {
                    // cannot exceed `cf_max`, so we can avoid the clipping
                    tok = rc_tok >> 11;
                    dq = (ac_dq.wrapping_mul(tok) >> dq_shift) as c_int;
                    assert!(dq <= cf_max);
                }
                cul_level = cul_level.wrapping_add(tok);
                put(&mut cf, rc, if sign != 0 { -dq } else { dq });

                rc = rc_tok as u16 & 0x3ff; // next non-zero `rc`, zero if `eob`
                if !(rc != 0) {
                    break;
                }
            }
        }
        None => {}
    }

    // context
    *res_ctx = (cmp::min(cul_level, 63) | dc_sign_level) as u8;

    Ok(eob as i32)
}

fn read_coef_tree<BD: BitDepth>(
    c: &Rav1dContext,
    f: &Rav1dFrameData,
    t: &mut Rav1dTaskContext,
    ts_c: &mut Rav1dTileStateContext,
    bs: BlockSize,
    b: &Av1Block,
    ytx: TxfmSize,
    depth: usize,
    tx_split: [u16; 2],
    x_off: c_int,
    y_off: c_int,
) -> Result<(), ()> {
    let t_dim = &DAV1D_TXFM_DIMENSIONS[ytx as usize];
    let txw = t_dim.w;
    let txh = t_dim.h;

    // `y_off` can be larger than 3 since lossless blocks
    // use `TX_4X4` but can't be splitted.
    // Avoids an undefined left shift.
    if depth < 2 && tx_split[depth] != 0 && tx_split[depth] & 1 << y_off * 4 + x_off != 0 {
        let sub = t_dim.sub;
        let sub_t_dim = &DAV1D_TXFM_DIMENSIONS[sub as usize];
        let txsw = sub_t_dim.w;
        let txsh = sub_t_dim.h;

        read_coef_tree::<BD>(
            c,
            f,
            t,
            ts_c,
            bs,
            b,
            sub,
            depth + 1,
            tx_split,
            x_off * 2 + 0,
            y_off * 2 + 0,
        )?;
        t.b.x += txsw as c_int;
        if txw >= txh && t.b.x < f.bw {
            read_coef_tree::<BD>(
                c,
                f,
                t,
                ts_c,
                bs,
                b,
                sub,
                depth + 1,
                tx_split,
                x_off * 2 + 1,
                y_off * 2 + 0,
            )?;
        }
        t.b.x -= txsw as c_int;
        t.b.y += txsh as c_int;
        if txh >= txw && t.b.y < f.bh {
            read_coef_tree::<BD>(
                c,
                f,
                t,
                ts_c,
                bs,
                b,
                sub,
                depth + 1,
                tx_split,
                x_off * 2 + 0,
                y_off * 2 + 1,
            )?;
            t.b.x += txsw as c_int;
            if txw >= txh && t.b.x < f.bw {
                read_coef_tree::<BD>(
                    c,
                    f,
                    t,
                    ts_c,
                    bs,
                    b,
                    sub,
                    depth + 1,
                    tx_split,
                    x_off * 2 + 1,
                    y_off * 2 + 1,
                )?;
            }
            t.b.x -= txsw as c_int;
        }
        t.b.y -= txsh as c_int;
    } else {
        let bx4 = t.b.x as usize & 31;
        let by4 = t.b.y as usize & 31;
        let mut txtp = DCT_DCT;
        let mut cf_ctx = 0;

        // rav1g: the parse of upstream's first pass; the coefficients go to the sink.
        let eob = decode_coefs::<BD>(
            c,
            (t.b.x, t.b.y),
            f,
            t.ts,
            ts_c,
            debug_block_info!(f, t.b),
            &mut t.scratch,
            &mut t.cf,
            &mut f.a[t.a].lcoef.index_mut(bx4..bx4 + txw as usize),
            &mut t.l.lcoef.index_mut(by4..by4 + txh as usize),
            ytx,
            bs,
            b,
            0,
            &mut txtp,
            &mut cf_ctx,
        )?;
        if debug_block_info!(f, t.b) {
            println!(
                "Post-y-cf-blk[tx={:?},txtp={},eob={}]: r={}",
                ytx, txtp, eob, ts_c.msac.rng,
            );
        }
        CaseSet::<16, true>::many(
            [&t.l.lcoef, &f.a[t.a].lcoef],
            [
                cmp::min(txh as c_int, f.bh - t.b.y) as usize,
                cmp::min(txw as c_int, f.bw - t.b.x) as usize,
            ],
            [by4, bx4],
            |case, dir| {
                case.set_disjoint(dir, cf_ctx);
            },
        );
        let txtp_map =
            &mut t.scratch.inter_intra_mut().ac_txtp_map.txtp_map_mut()[by4 * 32 + bx4..];
        CaseSet::<16, false>::one((), txw as usize, 0, |case, ()| {
            for txtp_map in txtp_map.chunks_mut(32).take(txh as usize) {
                case.set(txtp_map, txtp);
            }
        });
    };
    Ok(())
}

pub(crate) fn rav1d_read_coef_blocks<BD: BitDepth>(
    c: &Rav1dContext,
    f: &Rav1dFrameData,
    t: &mut Rav1dTaskContext,
    ts_c: &mut Rav1dTileStateContext,
    bs: BlockSize,
    b: &Av1Block,
) -> Result<(), ()> {
    let ss_ver = (f.cur.p.layout == Rav1dPixelLayout::I420) as u8;
    let ss_hor = (f.cur.p.layout != Rav1dPixelLayout::I444) as u8;
    let bx4 = t.b.x as usize & 31;
    let by4 = t.b.y as usize & 31;
    let cbx4 = bx4 >> ss_hor;
    let cby4 = by4 >> ss_ver;
    let b_dim = bs.dimensions();
    let bw4 = b_dim[0];
    let bh4 = b_dim[1];
    let cbw4 = bw4 + ss_hor >> ss_hor;
    let cbh4 = bh4 + ss_ver >> ss_ver;
    let has_chroma = f.cur.p.layout != Rav1dPixelLayout::I400
        && (bw4 > ss_hor || t.b.x & 1 != 0)
        && (bh4 > ss_ver || t.b.y & 1 != 0);

    if b.skip != 0 {
        CaseSet::<32, false>::many(
            [&t.l, &f.a[t.a]],
            [bh4 as usize, bw4 as usize],
            [by4, bx4],
            |case, dir| {
                case.set_disjoint(&dir.lcoef, 0x40);
            },
        );
        if has_chroma {
            CaseSet::<32, false>::many(
                [&t.l, &f.a[t.a]],
                [cbh4 as usize, cbw4 as usize],
                [cby4, cbx4],
                |case, dir| {
                    for ccoef in &dir.ccoef {
                        case.set_disjoint(ccoef, 0x40)
                    }
                },
            );
        }
        return Ok(());
    }

    let w4 = cmp::min(bw4 as c_int, f.bw - t.b.x) as u8;
    let h4 = cmp::min(bh4 as c_int, f.bh - t.b.y) as u8;
    let cw4 = w4 + ss_hor >> ss_hor;
    let ch4 = h4 + ss_ver >> ss_ver;
    assert!(b.skip == 0);
    let uv_t_dim = &DAV1D_TXFM_DIMENSIONS[b.uvtx as usize];
    let t_dim = &DAV1D_TXFM_DIMENSIONS[match &b.ii {
        Av1BlockIntraInter::Intra(intra) => intra.tx,
        Av1BlockIntraInter::Inter(inter) => inter.max_ytx,
    } as usize];

    for init_y in (0..h4).step_by(16) {
        let sub_h4 = cmp::min(h4, 16 + init_y);
        for init_x in (0..w4).step_by(16) {
            let sub_w4 = cmp::min(w4, init_x + 16);
            let mut y_off = (init_y != 0) as c_int;
            let mut y;
            let mut x;
            y = init_y;
            t.b.y += init_y as c_int;
            while y < sub_h4 {
                let mut x_off = (init_x != 0) as c_int;
                x = init_x;
                t.b.x += init_x as c_int;
                while x < sub_w4 {
                    match &b.ii {
                        Av1BlockIntraInter::Inter(inter) => {
                            let tx_split = [inter.tx_split0 as u16, inter.tx_split1];
                            read_coef_tree::<BD>(
                                c,
                                f,
                                t,
                                ts_c,
                                bs,
                                b,
                                inter.max_ytx,
                                0,
                                tx_split,
                                x_off,
                                y_off,
                            )?;
                        }
                        Av1BlockIntraInter::Intra(intra) => {
                            let mut cf_ctx = 0x40;
                            let mut txtp = DCT_DCT;
                            let a_start = bx4 + x as usize;
                            let a_len = t_dim.w as usize;
                            let l_start = by4 + y as usize;
                            let l_len = t_dim.h as usize;
                            let eob = decode_coefs::<BD>(
                                c,
                                (t.b.x, t.b.y),
                                f,
                                t.ts,
                                ts_c,
                                debug_block_info!(f, t.b),
                                &mut t.scratch,
                                &mut t.cf,
                                &mut f.a[t.a].lcoef.index_mut((a_start.., ..a_len)),
                                &mut t.l.lcoef.index_mut((l_start.., ..l_len)),
                                intra.tx,
                                bs,
                                b,
                                0,
                                &mut txtp,
                                &mut cf_ctx,
                            )?;
                            if debug_block_info!(f, t.b) {
                                println!(
                                    "Post-y-cf-blk[tx={:?},txtp={},eob={}]: r={}",
                                    intra.tx, txtp, eob, ts_c.msac.rng,
                                );
                            }
                            CaseSet::<16, true>::many(
                                [&t.l.lcoef, &f.a[t.a].lcoef],
                                [
                                    cmp::min(t_dim.h as i32, f.bh - t.b.y) as usize,
                                    cmp::min(t_dim.w as i32, f.bw - t.b.x) as usize,
                                ],
                                [by4 + y as usize, bx4 + x as usize],
                                |case, dir| {
                                    case.set_disjoint(dir, cf_ctx);
                                },
                            );
                        }
                    }
                    x += t_dim.w;
                    t.b.x += t_dim.w as c_int;
                    x_off += 1;
                }
                t.b.x -= x as c_int;
                y += t_dim.h;
                t.b.y += t_dim.h as c_int;
                y_off += 1;
            }
            t.b.y -= y as c_int;

            if !has_chroma {
                continue;
            }
            let sub_ch4 = cmp::min(ch4, init_y + 16 >> ss_ver);
            let sub_cw4 = cmp::min(cw4, init_x + 16 >> ss_hor);
            let mut pl = 0;
            while pl < 2 {
                y = init_y >> ss_ver;
                t.b.y += init_y as c_int;
                while y < sub_ch4 {
                    x = init_x >> ss_hor;
                    t.b.x += init_x as c_int;
                    while x < sub_cw4 {
                        let mut cf_ctx = 0x40;
                        let mut txtp = match b.ii {
                            Av1BlockIntraInter::Intra(_) => DCT_DCT,
                            Av1BlockIntraInter::Inter(_) => {
                                t.scratch.inter_intra().ac_txtp_map.txtp_map()[(by4 as usize
                                    + (y << ss_ver) as usize)
                                    * 32
                                    + bx4
                                    + (x << ss_hor) as usize]
                            }
                        };
                        let a_start = cbx4 + x as usize;
                        let a_len = uv_t_dim.w as usize;
                        let a_ccoef = &f.a[t.a].ccoef[pl];
                        let l_start = cby4 + y as usize;
                        let l_len = uv_t_dim.h as usize;
                        let l_ccoef = &t.l.ccoef[pl];
                        let eob = decode_coefs::<BD>(
                            c,
                            (t.b.x >> ss_hor, t.b.y >> ss_ver),
                            f,
                            t.ts,
                            ts_c,
                            debug_block_info!(f, t.b),
                            &mut t.scratch,
                            &mut t.cf,
                            &mut a_ccoef.index_mut((a_start.., ..a_len)),
                            &mut l_ccoef.index_mut((l_start.., ..l_len)),
                            b.uvtx,
                            bs,
                            b,
                            1 + pl,
                            &mut txtp,
                            &mut cf_ctx,
                        )?;
                        if debug_block_info!(f, t.b) {
                            println!(
                                "Post-uv-cf-blk[pl={},tx={:?},txtp={},eob={}]: r={}",
                                pl, b.uvtx, txtp, eob, ts_c.msac.rng,
                            );
                        }
                        CaseSet::<16, true>::many(
                            [l_ccoef, a_ccoef],
                            [
                                cmp::min(
                                    uv_t_dim.h as i32,
                                    f.bh - t.b.y + ss_ver as c_int >> ss_ver,
                                ) as usize,
                                cmp::min(
                                    uv_t_dim.w as i32,
                                    f.bw - t.b.x + ss_hor as c_int >> ss_hor,
                                ) as usize,
                            ],
                            [cby4 + y as usize, cbx4 as usize + x as usize],
                            |case, dir| {
                                case.set_disjoint(dir, cf_ctx);
                            },
                        );
                        x += uv_t_dim.w;
                        t.b.x += (uv_t_dim.w as c_int) << ss_hor;
                    }
                    t.b.x -= (x << ss_hor) as c_int;
                    y += uv_t_dim.h;
                    t.b.y += (uv_t_dim.h as c_int) << ss_ver;
                }
                t.b.y -= (y << ss_ver) as c_int;
                pl += 1;
            }
        }
    }
    Ok(())
}

pub(crate) fn rav1d_copy_pal_block_y<BD: BitDepth>(
    t: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    bx4: usize,
    by4: usize,
    bw4: usize,
    bh4: usize,
) {
    // rav1g: the colours are the task's; `f.frame_thread.pal` of upstream is not here.
    let _ = f;
    let pal = &t.scratch.inter_intra().interintra_edge_pal.pal.buf::<BD>()[0];
    let al_pal = t.al_pal.select_mut::<BD>();
    for al_pal in &mut al_pal[0][bx4..][..bw4] {
        al_pal[0] = *pal;
    }
    for al_pal in &mut al_pal[1][by4..][..bh4] {
        al_pal[0] = *pal;
    }
}

pub(crate) fn rav1d_copy_pal_block_uv<BD: BitDepth>(
    t: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    bx4: usize,
    by4: usize,
    bw4: usize,
    bh4: usize,
) {
    let _ = f;
    let pal = t.scratch.inter_intra().interintra_edge_pal.pal.buf::<BD>();
    // see aomedia bug 2183 for why we use luma coordinates here
    let al_pal = t.al_pal.select_mut::<BD>();
    for pl in 1..3 {
        for x in 0..bw4 {
            al_pal[0][bx4 + x][pl] = pal[pl];
        }
        for y in 0..bh4 {
            al_pal[1][by4 + y][pl] = pal[pl];
        }
    }
}

/// Return `pal_sz`.
pub(crate) fn rav1d_read_pal_plane<BD: BitDepth>(
    t: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    ts_c: &mut Rav1dTileStateContext,
    pl: bool,
    sz_ctx: u8,
    bx4: usize,
    by4: usize,
) -> u8 {
    let pli = pl as usize;
    let not_pl = !pl as u16;

    let pal_sz_u8 = rav1d_msac_decode_symbol_adapt8(
        &mut ts_c.msac,
        &mut ts_c.cdf.m.pal_sz[pli][sz_ctx as usize],
        6,
    ) + 2;
    let pal_sz = pal_sz_u8 as usize;
    let mut cache = [0.as_::<BD::Pixel>(); 16];
    let mut used_cache = [0.as_::<BD::Pixel>(); 8];
    let mut l_cache = if pl {
        t.pal_sz_uv[1][by4]
    } else {
        *t.l.pal_sz.index(by4)
    };
    let mut n_cache = 0;
    // don't reuse above palette outside SB64 boundaries
    let mut a_cache = if by4 & 15 != 0 {
        if pl {
            t.pal_sz_uv[0][bx4]
        } else {
            *f.a[t.a].pal_sz.index(bx4)
        }
    } else {
        0
    };
    let [a, l] = t.al_pal.select_mut::<BD>();
    let mut l = &l[by4][pli][..];
    let mut a = &a[bx4][pli][..];

    // fill/sort cache
    // TODO: This logic could be replaced with `itertools`' `.merge` and `.dedup`, which would elide bounds checks.
    while l_cache != 0 && a_cache != 0 {
        if l[0] < a[0] {
            if n_cache == 0 || cache[n_cache - 1] != l[0] {
                cache[n_cache] = l[0];
                n_cache += 1;
            }
            l = &l[1..];
            l_cache -= 1;
        } else {
            if a[0] == l[0] {
                l = &l[1..];
                l_cache -= 1;
            }
            if n_cache == 0 || cache[n_cache - 1] != a[0] {
                cache[n_cache] = a[0];
                n_cache += 1;
            }
            a = &a[1..];
            a_cache -= 1;
        }
    }
    if l_cache != 0 {
        loop {
            if n_cache == 0 || cache[n_cache - 1] != l[0] {
                cache[n_cache] = l[0];
                n_cache += 1;
            }
            l = &l[1..];
            l_cache -= 1;
            if !(l_cache > 0) {
                break;
            }
        }
    } else if a_cache != 0 {
        loop {
            if n_cache == 0 || cache[n_cache - 1] != a[0] {
                cache[n_cache] = a[0];
                n_cache += 1;
            }
            a = &a[1..];
            a_cache -= 1;
            if !(a_cache > 0) {
                break;
            }
        }
    }
    let cache = &cache[..n_cache];

    // find reused cache entries
    // TODO: Bounds checks could be elided with more complex iterators.
    let mut i = 0;
    for cache in cache {
        if !(i < pal_sz) {
            break;
        }
        if rav1d_msac_decode_bool_equi(&mut ts_c.msac) {
            used_cache[i] = *cache;
            i += 1;
        }
    }
    let used_cache = &used_cache[..i];

    // parse new entries
    let pal = &mut t
        .scratch
        .inter_intra_mut()
        .interintra_edge_pal
        .pal
        .buf_mut::<BD>()[pli];
    let pal = &mut pal[..pal_sz];
    if i < pal.len() {
        let mut prev = rav1d_msac_decode_bools(&mut ts_c.msac, f.cur.p.bpc) as u16;
        pal[i] = prev.as_::<BD::Pixel>();
        i += 1;

        if i < pal.len() {
            let mut bits = f.cur.p.bpc + rav1d_msac_decode_bools(&mut ts_c.msac, 2) as u8 - 3;
            let max = (1 << f.cur.p.bpc) - 1;

            loop {
                let delta = rav1d_msac_decode_bools(&mut ts_c.msac, bits) as u16;
                prev = cmp::min(prev + delta + not_pl, max);
                pal[i] = prev.as_::<BD::Pixel>();
                i += 1;
                if prev + not_pl >= max {
                    pal[i..].fill(max.as_::<BD::Pixel>());
                    break;
                } else {
                    bits = cmp::min(bits, 1 + ulog2((max - prev - not_pl) as u32) as u8);
                    if !(i < pal.len()) {
                        break;
                    }
                }
            }
        }

        // merge cache+new entries
        let mut n = 0;
        let mut m = used_cache.len();
        for i in 0..pal.len() {
            if n < used_cache.len() && (m >= pal.len() || used_cache[n] <= pal[m]) {
                pal[i] = used_cache[n];
                n += 1;
            } else {
                pal[i] = pal[m];
                m += 1;
            }
        }
    } else {
        pal[..used_cache.len()].copy_from_slice(&used_cache);
    }

    if debug_block_info!(f, t.b) {
        print!(
            "Post-pal[pl={},sz={},cache_size={},used_cache={}]: r={}, cache=",
            pli,
            pal_sz,
            cache.len(),
            used_cache.len(),
            ts_c.msac.rng
        );
        for (n, cache) in cache.iter().enumerate() {
            print!(
                "{}{:02x}",
                if n != 0 { ' ' } else { '[' },
                (*cache).as_::<c_int>()
            );
        }
        print!("{}, pal=", if cache.len() != 0 { "]" } else { "[]" });
        for (n, pal) in pal.iter().enumerate() {
            print!(
                "{}{:02x}",
                if n != 0 { ' ' } else { '[' },
                (*pal).as_::<c_int>()
            );
        }
        println!("]");
    }

    pal_sz_u8
}

/// Return `pal_sz[1]`.
pub(crate) fn rav1d_read_pal_uv<BD: BitDepth>(
    t: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    ts_c: &mut Rav1dTileStateContext,
    sz_ctx: u8,
    bx4: usize,
    by4: usize,
) -> u8 {
    let pal_sz = rav1d_read_pal_plane::<BD>(t, f, ts_c, true, sz_ctx, bx4, by4);

    // V pal coding
    let pal = &mut t
        .scratch
        .inter_intra_mut()
        .interintra_edge_pal
        .pal
        .buf_mut::<BD>()[2];
    let pal = &mut pal[..pal_sz as usize];
    if rav1d_msac_decode_bool_equi(&mut ts_c.msac) {
        let bits = f.cur.p.bpc + rav1d_msac_decode_bools(&mut ts_c.msac, 2) as u8 - 4;
        let mut prev = rav1d_msac_decode_bools(&mut ts_c.msac, f.cur.p.bpc) as u16;
        pal[0] = prev.as_::<BD::Pixel>();
        let max = (1 << f.cur.p.bpc) - 1;
        for pal in &mut pal[1..] {
            let mut delta = rav1d_msac_decode_bools(&mut ts_c.msac, bits) as i16;
            if delta != 0 && rav1d_msac_decode_bool_equi(&mut ts_c.msac) {
                delta = -delta;
            }
            prev = (prev as i16 + delta) as u16 & max;
            *pal = prev.as_::<BD::Pixel>();
        }
    } else {
        pal.fill_with(|| rav1d_msac_decode_bools(&mut ts_c.msac, f.cur.p.bpc).as_::<BD::Pixel>());
    }
    if debug_block_info!(f, t.b) {
        print!("Post-pal[pl=2]: r={} ", ts_c.msac.rng);
        for (n, pal) in pal.iter().enumerate() {
            print!(
                "{}{:02x}",
                if n != 0 { ' ' } else { '[' },
                (*pal).as_::<c_int>()
            );
        }
        println!("]");
    }

    pal_sz
}

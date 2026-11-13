use std::ffi::c_uint;

use strum::EnumCount;

use crate::align::{Align16, Align4, Align64, Align8};
use crate::const_fn::const_for;
use crate::enum_map::DefaultValue;
use crate::in_range::InRange;
use crate::include::dav1d::headers::{
    Rav1dFilterMode, Rav1dWarpedMotionParams, Rav1dWarpedMotionType,
};
use crate::levels::{
    BlockLevel, BlockPartition, BlockSize, Filter2d, InterPredMode, TxClass, TxfmSize, TxfmType,
    ADST_ADST, ADST_DCT, ADST_FLIPADST, DCT_ADST, DCT_DCT, DCT_FLIPADST, DC_PRED,
    DIAG_DOWN_LEFT_PRED, DIAG_DOWN_RIGHT_PRED, FLIPADST_ADST, FLIPADST_DCT, FLIPADST_FLIPADST,
    GLOBALMV, GLOBALMV_GLOBALMV, HOR_DOWN_PRED, HOR_PRED, HOR_UP_PRED, H_ADST, H_DCT, H_FLIPADST,
    IDTX, NEARESTMV, NEARESTMV_NEARESTMV, NEARESTMV_NEWMV, NEARMV, NEARMV_NEARMV, NEARMV_NEWMV,
    NEWMV, NEWMV_NEARESTMV, NEWMV_NEARMV, NEWMV_NEWMV, N_COMP_INTER_PRED_MODES, N_INTRA_PRED_MODES,
    N_TX_TYPES_PLUS_LL, N_UV_INTRA_PRED_MODES, PAETH_PRED, SMOOTH_H_PRED, SMOOTH_PRED,
    SMOOTH_V_PRED, VERT_LEFT_PRED, VERT_PRED, VERT_RIGHT_PRED, V_ADST, V_DCT, V_FLIPADST,
};

#[repr(C)]
pub struct TxfmInfo {
    pub w: u8,
    pub h: u8,
    pub lw: u8,
    pub lh: u8,
    pub min: u8,
    pub max: u8,
    pub sub: TxfmSize,
    pub ctx: u8,
}

pub static DAV1D_AL_PART_CTX: [[[u8; BlockPartition::COUNT]; BlockLevel::COUNT]; 2] = [
    [
        [0x00, 0x00, 0x10, 0xff, 0x00, 0x10, 0x10, 0x10, 0xff, 0xff],
        [0x10, 0x10, 0x18, 0xff, 0x10, 0x18, 0x18, 0x18, 0x10, 0x1c],
        [0x18, 0x18, 0x1c, 0xff, 0x18, 0x1c, 0x1c, 0x1c, 0x18, 0x1e],
        [0x1c, 0x1c, 0x1e, 0xff, 0x1c, 0x1e, 0x1e, 0x1e, 0x1c, 0x1f],
        [0x1e, 0x1e, 0x1f, 0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ],
    [
        [0x00, 0x10, 0x00, 0xff, 0x10, 0x10, 0x00, 0x10, 0xff, 0xff],
        [0x10, 0x18, 0x10, 0xff, 0x18, 0x18, 0x10, 0x18, 0x1c, 0x10],
        [0x18, 0x1c, 0x18, 0xff, 0x1c, 0x1c, 0x18, 0x1c, 0x1e, 0x18],
        [0x1c, 0x1e, 0x1c, 0xff, 0x1e, 0x1e, 0x1c, 0x1e, 0x1f, 0x1c],
        [0x1e, 0x1f, 0x1e, 0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ],
];

pub static DAV1D_BLOCK_SIZES: [[[BlockSize; 2]; BlockPartition::COUNT]; BlockLevel::COUNT] = {
    use BlockSize::*;

    const DEFAULT: BlockSize = BlockSize::Bs128x128;

    [
        [
            [Bs128x128, DEFAULT],
            [Bs128x64, DEFAULT],
            [Bs64x128, DEFAULT],
            [DEFAULT; 2],
            [Bs64x64, Bs128x64],
            [Bs128x64, Bs64x64],
            [Bs64x64, Bs64x128],
            [Bs64x128, Bs64x64],
            [DEFAULT; 2],
            [DEFAULT; 2],
        ],
        [
            [Bs64x64, DEFAULT],
            [Bs64x32, DEFAULT],
            [Bs32x64, DEFAULT],
            [DEFAULT; 2],
            [Bs32x32, Bs64x32],
            [Bs64x32, Bs32x32],
            [Bs32x32, Bs32x64],
            [Bs32x64, Bs32x32],
            [Bs64x16, DEFAULT],
            [Bs16x64, DEFAULT],
        ],
        [
            [Bs32x32, DEFAULT],
            [Bs32x16, DEFAULT],
            [Bs16x32, DEFAULT],
            [DEFAULT; 2],
            [Bs16x16, Bs32x16],
            [Bs32x16, Bs16x16],
            [Bs16x16, Bs16x32],
            [Bs16x32, Bs16x16],
            [Bs32x8, DEFAULT],
            [Bs8x32, DEFAULT],
        ],
        [
            [Bs16x16, DEFAULT],
            [Bs16x8, DEFAULT],
            [Bs8x16, DEFAULT],
            [DEFAULT; 2],
            [Bs8x8, Bs16x8],
            [Bs16x8, Bs8x8],
            [Bs8x8, Bs8x16],
            [Bs8x16, Bs8x8],
            [Bs16x4, DEFAULT],
            [Bs4x16, DEFAULT],
        ],
        [
            [Bs8x8, DEFAULT],
            [Bs8x4, DEFAULT],
            [Bs4x8, DEFAULT],
            [Bs4x4, DEFAULT],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
            [DEFAULT; 2],
        ],
    ]
};

static DAV1D_BLOCK_DIMENSIONS: [[u8; 4]; BlockSize::COUNT] = [
    [32, 32, 5, 5],
    [32, 16, 5, 4],
    [16, 32, 4, 5],
    [16, 16, 4, 4],
    [16, 8, 4, 3],
    [16, 4, 4, 2],
    [8, 16, 3, 4],
    [8, 8, 3, 3],
    [8, 4, 3, 2],
    [8, 2, 3, 1],
    [4, 16, 2, 4],
    [4, 8, 2, 3],
    [4, 4, 2, 2],
    [4, 2, 2, 1],
    [4, 1, 2, 0],
    [2, 8, 1, 3],
    [2, 4, 1, 2],
    [2, 2, 1, 1],
    [2, 1, 1, 0],
    [1, 4, 0, 2],
    [1, 2, 0, 1],
    [1, 1, 0, 0],
];

impl BlockSize {
    #[inline]
    pub fn dimensions(self) -> &'static [u8; 4] {
        &DAV1D_BLOCK_DIMENSIONS[self as usize]
    }
}

pub static DAV1D_TXFM_DIMENSIONS: [TxfmInfo; TxfmSize::COUNT] = {
    use TxfmSize::*;
    [
        TxfmInfo {
            w: 1,
            h: 1,
            lw: 0,
            lh: 0,
            min: 0,
            max: 0,
            sub: DefaultValue::DEFAULT,
            ctx: 0,
        },
        TxfmInfo {
            w: 2,
            h: 2,
            lw: 1,
            lh: 1,
            min: 1,
            max: 1,
            sub: S4x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 4,
            h: 4,
            lw: 2,
            lh: 2,
            min: 2,
            max: 2,
            sub: S8x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 8,
            h: 8,
            lw: 3,
            lh: 3,
            min: 3,
            max: 3,
            sub: S16x16,
            ctx: 3,
        },
        TxfmInfo {
            w: 16,
            h: 16,
            lw: 4,
            lh: 4,
            min: 4,
            max: 4,
            sub: S32x32,
            ctx: 4,
        },
        TxfmInfo {
            w: 1,
            h: 2,
            lw: 0,
            lh: 1,
            min: 0,
            max: 1,
            sub: S4x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 2,
            h: 1,
            lw: 1,
            lh: 0,
            min: 0,
            max: 1,
            sub: S4x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 2,
            h: 4,
            lw: 1,
            lh: 2,
            min: 1,
            max: 2,
            sub: S8x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 4,
            h: 2,
            lw: 2,
            lh: 1,
            min: 1,
            max: 2,
            sub: S8x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 4,
            h: 8,
            lw: 2,
            lh: 3,
            min: 2,
            max: 3,
            sub: S16x16,
            ctx: 3,
        },
        TxfmInfo {
            w: 8,
            h: 4,
            lw: 3,
            lh: 2,
            min: 2,
            max: 3,
            sub: S16x16,
            ctx: 3,
        },
        TxfmInfo {
            w: 8,
            h: 16,
            lw: 3,
            lh: 4,
            min: 3,
            max: 4,
            sub: S32x32,
            ctx: 4,
        },
        TxfmInfo {
            w: 16,
            h: 8,
            lw: 4,
            lh: 3,
            min: 3,
            max: 4,
            sub: S32x32,
            ctx: 4,
        },
        TxfmInfo {
            w: 1,
            h: 4,
            lw: 0,
            lh: 2,
            min: 0,
            max: 2,
            sub: R4x8,
            ctx: 1,
        },
        TxfmInfo {
            w: 4,
            h: 1,
            lw: 2,
            lh: 0,
            min: 0,
            max: 2,
            sub: R8x4,
            ctx: 1,
        },
        TxfmInfo {
            w: 2,
            h: 8,
            lw: 1,
            lh: 3,
            min: 1,
            max: 3,
            sub: R8x16,
            ctx: 2,
        },
        TxfmInfo {
            w: 8,
            h: 2,
            lw: 3,
            lh: 1,
            min: 1,
            max: 3,
            sub: R16x8,
            ctx: 2,
        },
        TxfmInfo {
            w: 4,
            h: 16,
            lw: 2,
            lh: 4,
            min: 2,
            max: 4,
            sub: R16x32,
            ctx: 3,
        },
        TxfmInfo {
            w: 16,
            h: 4,
            lw: 4,
            lh: 2,
            min: 2,
            max: 4,
            sub: R32x16,
            ctx: 3,
        },
    ]
};

pub static DAV1D_MAX_TXFM_SIZE_FOR_BS: [[TxfmSize; 4]; BlockSize::COUNT] = {
    use TxfmSize::*;
    const DEFAULT: TxfmSize = DefaultValue::DEFAULT;
    [
        [S64x64, S32x32, S32x32, S32x32],
        [S64x64, S32x32, S32x32, S32x32],
        [S64x64, S32x32, DEFAULT, S32x32],
        [S64x64, S32x32, S32x32, S32x32],
        [R64x32, R32x16, S32x32, S32x32],
        [R64x16, R32x8, R32x16, R32x16],
        [R32x64, R16x32, DEFAULT, S32x32],
        [S32x32, S16x16, R16x32, S32x32],
        [R32x16, R16x8, S16x16, R32x16],
        [R32x8, R16x4, R16x8, R32x8],
        [R16x64, R8x32, DEFAULT, R16x32],
        [R16x32, R8x16, DEFAULT, R16x32],
        [S16x16, S8x8, R8x16, S16x16],
        [R16x8, R8x4, S8x8, R16x8],
        [R16x4, R8x4, R8x4, R16x4],
        [R8x32, R4x16, DEFAULT, R8x32],
        [R8x16, R4x8, DEFAULT, R8x16],
        [S8x8, S4x4, R4x8, S8x8],
        [R8x4, S4x4, S4x4, R8x4],
        [R4x16, R4x8, DEFAULT, R4x16],
        [R4x8, S4x4, DEFAULT, R4x8],
        [S4x4, S4x4, S4x4, S4x4],
    ]
};

pub static DAV1D_TXTP_FROM_UVMODE: [TxfmType; N_UV_INTRA_PRED_MODES] = {
    let mut tbl = [0; N_UV_INTRA_PRED_MODES];
    tbl[DC_PRED as usize] = DCT_DCT;
    tbl[VERT_PRED as usize] = ADST_DCT;
    tbl[HOR_PRED as usize] = DCT_ADST;
    tbl[DIAG_DOWN_LEFT_PRED as usize] = DCT_DCT;
    tbl[DIAG_DOWN_RIGHT_PRED as usize] = ADST_ADST;
    tbl[VERT_RIGHT_PRED as usize] = ADST_DCT;
    tbl[HOR_DOWN_PRED as usize] = DCT_ADST;
    tbl[HOR_UP_PRED as usize] = DCT_ADST;
    tbl[VERT_LEFT_PRED as usize] = ADST_DCT;
    tbl[SMOOTH_PRED as usize] = ADST_ADST;
    tbl[SMOOTH_V_PRED as usize] = ADST_DCT;
    tbl[SMOOTH_H_PRED as usize] = DCT_ADST;
    tbl[PAETH_PRED as usize] = ADST_ADST;
    tbl
};

pub static DAV1D_COMP_INTER_PRED_MODES: [[InterPredMode; 2]; N_COMP_INTER_PRED_MODES] = {
    let mut tbl = [[0; 2]; 8];
    tbl[NEARESTMV_NEARESTMV as usize] = [NEARESTMV, NEARESTMV];
    tbl[NEARMV_NEARMV as usize] = [NEARMV, NEARMV];
    tbl[NEWMV_NEWMV as usize] = [NEWMV, NEWMV];
    tbl[GLOBALMV_GLOBALMV as usize] = [GLOBALMV, GLOBALMV];
    tbl[NEWMV_NEARESTMV as usize] = [NEWMV, NEARESTMV];
    tbl[NEWMV_NEARMV as usize] = [NEWMV, NEARMV];
    tbl[NEARESTMV_NEWMV as usize] = [NEARESTMV, NEWMV];
    tbl[NEARMV_NEWMV as usize] = [NEARMV, NEWMV];
    tbl
};

pub static DAV1D_PARTITION_TYPE_COUNT: [u8; BlockLevel::COUNT] = [
    BlockPartition::COUNT as u8 - 3,
    BlockPartition::COUNT as u8 - 1,
    BlockPartition::COUNT as u8 - 1,
    BlockPartition::COUNT as u8 - 1,
    BlockPartition::N_SUB8X8_PARTITIONS as u8 - 1,
];

pub static DAV1D_TX_TYPES_PER_SET: [u8; 40] = [
    IDTX as u8,
    DCT_DCT as u8,
    ADST_ADST as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    IDTX as u8,
    DCT_DCT as u8,
    V_DCT as u8,
    H_DCT as u8,
    ADST_ADST as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    IDTX as u8,
    V_DCT as u8,
    H_DCT as u8,
    DCT_DCT as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    FLIPADST_DCT as u8,
    DCT_FLIPADST as u8,
    ADST_ADST as u8,
    FLIPADST_FLIPADST as u8,
    ADST_FLIPADST as u8,
    FLIPADST_ADST as u8,
    IDTX as u8,
    V_DCT as u8,
    H_DCT as u8,
    V_ADST as u8,
    H_ADST as u8,
    V_FLIPADST as u8,
    H_FLIPADST as u8,
    DCT_DCT as u8,
    ADST_DCT as u8,
    DCT_ADST as u8,
    FLIPADST_DCT as u8,
    DCT_FLIPADST as u8,
    ADST_ADST as u8,
    FLIPADST_FLIPADST as u8,
    ADST_FLIPADST as u8,
    FLIPADST_ADST as u8,
];

pub static DAV1D_YMODE_SIZE_CONTEXT: [u8; BlockSize::COUNT] = [
    3, 3, 3, 3, 3, 2, 3, 3, 2, 1, 2, 2, 2, 1, 0, 1, 1, 1, 0, 0, 0, 0,
];

/// Extend `a` from length `M` to length `N`, repeating the last element.
/// That is, `a[cmp::min(i, M - 1)] = extend_array(a)[cmp::min(i, N - 1)]`.
const fn extend_array<T: DefaultValue + Copy, const M: usize, const N: usize>(a: [T; M]) -> [T; N] {
    let mut b = [DefaultValue::DEFAULT; N];
    const_for!(i in 0..M => {
        b[i] = a[i];
    });
    const_for!(i in M..N => {
        b[i] = a[M - 1];
    });
    b
}

pub type LoCtxOffset = InRange<u8, 0, 21>;

/// The core of each of these 3 tables is a 5x5 table of offsets.
/// However, to use that 5x5 table in [`get_lo_ctx`],
/// we first have to clamp the `x` and `y` values from `[0, 32)` to `[0, 5)`.
/// That involves two `min`s, which LLVM makes into conditional moves that have to
/// happen in series before we can actually load the element we want.  These add latency.
/// Simply unroll the array: duplicate everything from the 5th row and 5th column out
/// so that we get the same effect as clamping without the latency.
/// Obviously, we pay for it in cache efficiency and binary size,
/// but the tradeoff seems worth it on older x86_64 systems, and not harmful elsewhere.
pub static DAV1D_LO_CTX_OFFSETS: [[[LoCtxOffset; 32]; 32]; 3] = {
    const fn extend_offsets(a: [[u8; 5]; 5]) -> [[LoCtxOffset; 32]; 32] {
        extend_array([
            extend_array(LoCtxOffset::new_array(a[0])),
            extend_array(LoCtxOffset::new_array(a[1])),
            extend_array(LoCtxOffset::new_array(a[2])),
            extend_array(LoCtxOffset::new_array(a[3])),
            extend_array(LoCtxOffset::new_array(a[4])),
        ])
    }

    [
        extend_offsets([
            [0, 1, 6, 6, 21],
            [1, 6, 6, 21, 21],
            [6, 6, 21, 21, 21],
            [6, 21, 21, 21, 21],
            [21, 21, 21, 21, 21],
        ]),
        extend_offsets([
            [0, 16, 6, 6, 21],
            [16, 16, 6, 21, 21],
            [16, 16, 21, 21, 21],
            [16, 16, 21, 21, 21],
            [16, 16, 21, 21, 21],
        ]),
        extend_offsets([
            [0, 11, 11, 11, 11],
            [11, 11, 11, 11, 11],
            [6, 6, 21, 21, 21],
            [6, 21, 21, 21, 21],
            [21, 21, 21, 21, 21],
        ]),
    ]
};

pub static DAV1D_SKIP_CTX: [[u8; 5]; 5] = [
    [1, 2, 2, 2, 3],
    [2, 4, 4, 4, 5],
    [2, 4, 4, 4, 5],
    [2, 4, 4, 4, 5],
    [3, 5, 5, 5, 6],
];

pub static DAV1D_TX_TYPE_CLASS: [TxClass; N_TX_TYPES_PLUS_LL] = [
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::TwoD,
    TxClass::V,
    TxClass::H,
    TxClass::V,
    TxClass::H,
    TxClass::V,
    TxClass::H,
    TxClass::TwoD,
];

pub const DAV1D_FILTER_2D: [[Filter2d; Rav1dFilterMode::N_FILTERS]; Rav1dFilterMode::N_FILTERS] = {
    use Filter2d::*;

    const DEFAULT: Filter2d = Filter2d::Regular8Tap;

    [
        [Regular8Tap, RegularSmooth8Tap, RegularSharp8Tap, DEFAULT],
        [SmoothRegular8Tap, Smooth8Tap, SmoothSharp8Tap, DEFAULT],
        [SharpRegular8Tap, SharpSmooth8Tap, Sharp8Tap, DEFAULT],
        [DEFAULT, DEFAULT, DEFAULT, Bilinear],
    ]
};

pub const DAV1D_FILTER_DIR: [[Rav1dFilterMode; 2]; Filter2d::COUNT] = [
    [Rav1dFilterMode::Regular8Tap, Rav1dFilterMode::Regular8Tap],
    [Rav1dFilterMode::Smooth8Tap, Rav1dFilterMode::Regular8Tap],
    [Rav1dFilterMode::Sharp8Tap, Rav1dFilterMode::Regular8Tap],
    [Rav1dFilterMode::Regular8Tap, Rav1dFilterMode::Sharp8Tap],
    [Rav1dFilterMode::Smooth8Tap, Rav1dFilterMode::Sharp8Tap],
    [Rav1dFilterMode::Sharp8Tap, Rav1dFilterMode::Sharp8Tap],
    [Rav1dFilterMode::Regular8Tap, Rav1dFilterMode::Smooth8Tap],
    [Rav1dFilterMode::Smooth8Tap, Rav1dFilterMode::Smooth8Tap],
    [Rav1dFilterMode::Sharp8Tap, Rav1dFilterMode::Smooth8Tap],
    [Rav1dFilterMode::Bilinear, Rav1dFilterMode::Bilinear],
];

pub static DAV1D_FILTER_MODE_TO_Y_MODE: [u8; 5] = [
    DC_PRED as u8,
    VERT_PRED as u8,
    HOR_PRED as u8,
    HOR_DOWN_PRED as u8,
    DC_PRED as u8,
];

pub static DAV1D_INTRA_MODE_CONTEXT: [u8; N_INTRA_PRED_MODES] =
    [0, 1, 2, 3, 4, 4, 4, 4, 3, 0, 1, 2, 0];

pub static DAV1D_WEDGE_CTX_LUT: [u8; BlockSize::COUNT] = [
    0, 0, 0, 0, 0, 0, 0, 6, 5, 8, 0, 4, 3, 2, 0, 7, 1, 0, 0, 0, 0, 0,
];

pub const CFL_ALLOWED_MASK: c_uint = {
    use BlockSize::*;

    1 << Bs32x32 as u8
        | 1 << Bs32x16 as u8
        | 1 << Bs32x8 as u8
        | 1 << Bs16x32 as u8
        | 1 << Bs16x16 as u8
        | 1 << Bs16x8 as u8
        | 1 << Bs16x4 as u8
        | 1 << Bs8x32 as u8
        | 1 << Bs8x16 as u8
        | 1 << Bs8x8 as u8
        | 1 << Bs8x4 as u8
        | 1 << Bs4x16 as u8
        | 1 << Bs4x8 as u8
        | 1 << Bs4x4 as u8
};

pub const WEDGE_ALLOWED_MASK: c_uint = {
    use BlockSize::*;

    1 << Bs32x32 as u8
        | 1 << Bs32x16 as u8
        | 1 << Bs32x8 as u8
        | 1 << Bs16x32 as u8
        | 1 << Bs16x16 as u8
        | 1 << Bs16x8 as u8
        | 1 << Bs8x32 as u8
        | 1 << Bs8x16 as u8
        | 1 << Bs8x8 as u8
};

pub const INTERINTRA_ALLOWED_MASK: c_uint = {
    use BlockSize::*;

    1 << Bs32x32 as u8
        | 1 << Bs32x16 as u8
        | 1 << Bs16x32 as u8
        | 1 << Bs16x16 as u8
        | 1 << Bs16x8 as u8
        | 1 << Bs8x16 as u8
        | 1 << Bs8x8 as u8
};

impl Default for Rav1dWarpedMotionParams {
    fn default() -> Self {
        Self {
            r#type: Rav1dWarpedMotionType::Identity,
            matrix: [0, 0, 1 << 16, 0, 0, 1 << 16],
            abcd: Default::default(),
        }
    }
}

pub static DAV1D_CDEF_DIRECTIONS: [[i8; 2]; 12] = [
    [1 * 12 + 0, 2 * 12 + 0],
    [1 * 12 + 0, 2 * 12 - 1],
    [-1 * 12 + 1, -2 * 12 + 2],
    [0 * 12 + 1, -1 * 12 + 2],
    [0 * 12 + 1, 0 * 12 + 2],
    [0 * 12 + 1, 1 * 12 + 2],
    [1 * 12 + 1, 2 * 12 + 2],
    [1 * 12 + 0, 2 * 12 + 1],
    [1 * 12 + 0, 2 * 12 + 0],
    [1 * 12 + 0, 2 * 12 - 1],
    [-1 * 12 + 1, -2 * 12 + 2],
    [0 * 12 + 1, -1 * 12 + 2],
];

pub static DAV1D_SGR_PARAMS: Align4<[[u16; 2]; 16]> = Align4([
    [140, 3236],
    [112, 2158],
    [93, 1618],
    [80, 1438],
    [70, 1295],
    [58, 1177],
    [47, 1079],
    [37, 996],
    [30, 925],
    [25, 863],
    [0, 2589],
    [0, 1618],
    [0, 1177],
    [0, 925],
    [56, 0],
    [22, 0],
]);

pub const FLT_INCR: usize = if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
    2
} else {
    1
};

const FILTER_INDICES: [usize; 7] = if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
    [0, 1, 16, 17, 32, 33, 48]
} else {
    [0, 8, 16, 24, 32, 40, 48]
};

pub fn filter_fn(flt_ptr: &[i8], p: [i32; 7]) -> i32 {
    let flt_ptr = &flt_ptr[..48 + 1];
    let mut sum = 0;
    for i in 0..7 {
        sum += flt_ptr[FILTER_INDICES[i]] as i32 * p[i] as i32;
    }
    sum
}

const fn gen_filter(mut a: [i8; 64], idx: usize, f: [i8; 7]) -> [i8; 64] {
    let mut i = 0;
    while i < 7 {
        a[FLT_INCR * idx + FILTER_INDICES[i]] = f[i];
        i += 1;
    }
    a
}

const fn gen_filters(f: [[i8; 7]; 8]) -> Align64<[i8; 64]> {
    let mut a = [0; 64];

    let mut i = 0;
    while i < 8 {
        a = gen_filter(a, i, f[i]);
        i += 1;
    }

    Align64(a)
}

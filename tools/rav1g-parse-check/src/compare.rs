// SPDX-License-Identifier: BSD-2-Clause

//! Holds a frame as the run that is checked left it against the same frame of the reference (rav1d's
//! parse pass and the sink on both sides, see `main.rs`): every buffer of the two arenas, whole.
//!
//! - the tiles' cursors (all of `av1_tile_data` but the addresses),
//! - the mode info pool, record by record and field by field,
//! - the grid, as the index of the record each 4x4 unit points to,
//! - the transform block records and the coefficients,
//! - the block index list, the block maps and the wavefront grids of intra prediction,
//! - the palette blocks, and the loop restoration units.
//!
//! A buffer is compared over its full size, so what one run wrote and the other did not is a
//! difference wherever it lies. No field of a record is left out.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::plain::{Arena, Buffer, COEF_PATTERN};
use crate::sys::{MB_MODE_INFO, MI_GRID_BASE, av1_tile_data, tx_block_info_gpu};

/// A field of the mode info record: its name, where it lies and how long it is.
struct Field {
    name: String,
    offset: usize,
    size: usize,
}

fn size_of_pointee<T>(_: *const T) -> usize {
    size_of::<T>()
}

macro_rules! fields {
    ($($($path:ident).+),+ $(,)?) => {{
        let record = std::mem::MaybeUninit::<MB_MODE_INFO>::uninit();
        let base = record.as_ptr();
        // SAFETY: addresses of fields of a record that is not read.
        unsafe {
            vec![$({
                let field = &raw const (*base).$($path).+;
                Field {
                    name: String::from(stringify!($($path).+)).replace(' ', ""),
                    offset: field as usize - base as usize,
                    size: size_of_pointee(field),
                }
            }),+]
        }
    }};
}

/// The fields of the record, and the bytes between them (padding) as fields of their own, in the order
/// they lie in.
fn record_fields() -> Vec<Field> {
    let mut fields = fields![
        wmmat,
        warp_angles,
        mv,
        mi_row,
        mi_col,
        index_base,
        sb_type,
        partition,
        ref_frame,
        mode,
        uv_mode,
        interintra_mode,
        motion_mode,
        use_wedge_interintra,
        use_intrabc,
        angle_delta,
        tx_size,
        skip,
        cdef_strength,
        segment_id,
        inter_tx_size,
        delta_lf,
        delta_lf_from_base,
        cfl_alpha_idx,
        cfl_alpha_signs,
        interintra_wedge_index,
        palette_size,
        filter_intra_mode,
        flags,
        compound_type,
        wedge_index,
        interp_filters,
    ];
    fields.sort_by_key(|field| field.offset);
    let mut all = Vec::new();
    let mut at = 0;
    for field in fields {
        assert!(field.offset >= at, "the fields of the record do not overlap");
        if field.offset > at {
            all.push(Field { name: format!("padding@{at}"), offset: at, size: field.offset - at });
        }
        at = field.offset + field.size;
        all.push(field);
    }
    if at < size_of::<MB_MODE_INFO>() {
        all.push(Field { name: format!("padding@{at}"), offset: at, size: size_of::<MB_MODE_INFO>() - at });
    }
    all
}

/// What the comparison of a stream found.
#[derive(Default)]
pub struct Report {
    pub frames: u64,
    pub records: u64,
    pub tx_blocks: u64,
    pub coefs: u64,
    pub palettes: u64,
    pub lr_units: u64,
    /// What the blocks that were compared are: how many of each kind (of the reference's records), so that
    /// "equal" says over what.
    pub coverage: BTreeMap<&'static str, u64>,
    /// The differences by kind (the name of a record's field, of a buffer), and how many of each.
    pub differences: BTreeMap<String, u64>,
    /// The first differences in words.
    pub details: Vec<String>,
    pub detail_limit: usize,
}

impl Report {
    pub fn new(detail_limit: usize) -> Self {
        Self { detail_limit, ..Default::default() }
    }

    pub fn equal(&self) -> bool {
        self.differences.is_empty()
    }

    pub fn difference(&mut self, kind: &str, detail: impl FnOnce() -> String) {
        *self.differences.entry(String::from(kind)).or_default() += 1;
        if self.details.len() < self.detail_limit {
            let detail = detail();
            self.details.push(detail);
        }
    }

    pub fn add(&mut self, other: &Report) {
        self.frames += other.frames;
        self.records += other.records;
        self.tx_blocks += other.tx_blocks;
        self.coefs += other.coefs;
        self.palettes += other.palettes;
        self.lr_units += other.lr_units;
        for (kind, count) in &other.differences {
            *self.differences.entry(kind.clone()).or_default() += count;
        }
        for (kind, count) in &other.coverage {
            *self.coverage.entry(kind).or_default() += count;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::new();
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// A record in a few words, for a difference.
fn describe(mi: &MB_MODE_INFO) -> String {
    format!(
        "block at row {} col {} size {} ref [{}, {}] mode {} skip {} intrabc {} motion {} segment {} tx {}",
        mi.mi_row,
        mi.mi_col,
        mi.sb_type,
        mi.ref_frame[0],
        mi.ref_frame[1],
        mi.mode,
        mi.skip,
        mi.use_intrabc,
        mi.motion_mode,
        mi.segment_id,
        mi.tx_size
    )
}

/// Counts the kinds of blocks among the records of a frame of the reference.
fn coverage(report: &mut Report, c: &Arena) {
    const GLOBALMV: u8 = 15;
    const GLOBAL_GLOBALMV: u8 = 23;
    const UV_CFL_PRED: u8 = 13;
    let mut count = |kind: &'static str, yes: bool| {
        if yes {
            *report.coverage.entry(kind).or_default() += 1;
        }
    };
    for tile in 0..c.tiles.len() {
        // SAFETY: a tile of a parsed frame and the records of its part of the pool.
        let records = unsafe {
            let data = &*c.tiles.ptr().add(tile);
            std::slice::from_raw_parts(c.mi_pool.ptr().add(data.mi_offset as usize), data.mi_count as usize)
        };
        for mi in records {
            let wide = 1 << crate::host::tables::MI_SIZE_WIDE_LOG2[mi.sb_type as usize];
            let high = 1 << crate::host::tables::MI_SIZE_HIGH_LOG2[mi.sb_type as usize];
            let inter = mi.ref_frame[0] > 0;
            let global = inter && (mi.mode == GLOBALMV || mi.mode == GLOBAL_GLOBALMV);
            count("intra", !inter && mi.use_intrabc == 0);
            count("intra block copy", mi.use_intrabc != 0);
            count("inter", inter);
            count("inter, skipped", inter && mi.skip != 0);
            count(
                "inter, transform sizes not all one",
                inter && mi.inter_tx_size.iter().any(|tx| *tx != mi.inter_tx_size[0]),
            );
            count("compound", inter && mi.ref_frame[1] > 0);
            count("compound, wedge or difference mask", inter && mi.ref_frame[1] > 0 && mi.compound_type >= 2);
            count("inter-intra", inter && mi.ref_frame[1] == 0);
            count("overlapped motion", inter && mi.motion_mode == 1);
            count("local warp", inter && mi.motion_mode == 2);
            count(
                "local warp, model not valid",
                inter && mi.motion_mode == 2 && mi.flags & MB_MODE_INFO::WARP_INVALID != 0,
            );
            count("global motion mode", global);
            count("global motion mode, warped", global && c.gm_warp[mi.ref_frame[0] as usize] != 0);
            count(
                "crosses the frame edge",
                i32::from(mi.mi_col) + wide > c.size.mi_cols || i32::from(mi.mi_row) + high > c.size.mi_rows,
            );
            count("4 wide or high (chroma from the neighbours)", wide == 1 || high == 1);
            count("palette", mi.palette_size != [0, 0]);
            count("filter intra", mi.flags & MB_MODE_INFO::USE_FILTER_INTRA != 0);
            count("chroma from luma", !inter && mi.uv_mode == UV_CFL_PRED);
            count("segment not 0", mi.segment_id != 0);
            count("loop filter delta", mi.delta_lf_from_base != 0 || mi.delta_lf != [0; 4]);
        }
    }
}

/// Two buffers of numbers, whole: counts the entries that differ and describes the first.
fn compare_numbers<T: PartialEq + std::fmt::Debug>(
    report: &mut Report,
    frame: u64,
    kind: &str,
    c: &Buffer<T>,
    r: &Buffer<T>,
) {
    // SAFETY: both runs have finished the frame.
    let (c, r) = unsafe { (c.values(), r.values()) };
    if c.len() != r.len() {
        report.difference(kind, || {
            format!("frame {frame}: {kind}: {} entries in the reference, {} checked", c.len(), r.len())
        });
        return;
    }
    // SAFETY: as above; the bytes of the same values.
    let same = unsafe {
        std::slice::from_raw_parts(c.as_ptr().cast::<u8>(), size_of_val(c))
            == std::slice::from_raw_parts(r.as_ptr().cast::<u8>(), size_of_val(r))
    };
    if same {
        return;
    }
    for (index, (a, b)) in c.iter().zip(r).enumerate() {
        if a != b {
            report.difference(kind, || format!("frame {frame}: {kind}[{index}]: reference {a:?}, checked {b:?}"));
        }
    }
}

/// The grid of a frame whose sink wrote none (`-parseopt gpugrid`), from the frame's records, as the
/// pass of the GPU builds it in the decoder (`mi_grid_build.hlsl`): the entry of every record at the
/// 4x4 units of its block that are inside the frame. [`frame`] then holds it against the grid the
/// reference's sink wrote, so the records of such a parse give the same grid.
pub fn build_grid(arena: &Arena) {
    let stride = arena.size.mi_stride();
    for tile in 0..arena.tiles.len() {
        // SAFETY: a tile of a parsed frame, the records of its part of the pool, and the units of their
        // blocks inside the frame, which are units of the arena's grid.
        unsafe {
            let data = &*arena.tiles.ptr().add(tile);
            let first = arena.mi_pool.ptr().add(data.mi_offset as usize);
            for index in 0..data.mi_count as usize {
                let mi = first.add(index);
                let wide = 1 << crate::host::tables::MI_SIZE_WIDE_LOG2[(*mi).sb_type as usize];
                let high = 1 << crate::host::tables::MI_SIZE_HIGH_LOG2[(*mi).sb_type as usize];
                let (row, col) = (i32::from((*mi).mi_row), i32::from((*mi).mi_col));
                let entry = crate::sys::mi_grid_entry(arena.mi_pool.ptr(), mi);
                for y in row..(row + high).min(arena.size.mi_rows) {
                    for x in col..(col + wide).min(arena.size.mi_cols) {
                        arena.mi_grid.ptr().add((y * stride + x) as usize).write(entry);
                    }
                }
            }
        }
    }
}

/// Holds the frame of the run that is checked (`r`) against that of the reference (`c`).
pub fn frame(report: &mut Report, number: u64, c: &Arena, r: &Arena) {
    report.frames += 1;
    if c.size != r.size {
        report.difference("frame size", || format!("frame {number}: reference {:?}, checked {:?}", c.size, r.size));
        return;
    }
    coverage(report, c);
    if c.gm_warp != r.gm_warp {
        report.difference("gm_warp", || {
            format!("frame {number}: gm_warp: reference {:?}, checked {:?}", c.gm_warp, r.gm_warp)
        });
    }

    // The tiles' cursors, without the addresses.
    let cursors = |arena: &Arena, index: usize| -> av1_tile_data {
        // SAFETY: a tile of the frame, which is parsed.
        let mut tile = unsafe { *arena.tiles.ptr().add(index) };
        tile.idct_blocks_host = std::ptr::null_mut();
        tile.dq_buffer_base = std::ptr::null_mut();
        tile.gen_indexes = std::ptr::null_mut();
        tile.gen_block_map = std::ptr::null_mut();
        tile.gen_block_map_wrp = std::ptr::null_mut();
        tile.gen_block_map_fast = std::ptr::null_mut();
        tile.wave_edges = std::ptr::null_mut();
        tile.frame_data = std::ptr::null_mut();
        tile.perf_inter = std::ptr::null_mut();
        tile
    };
    for index in 0..c.tiles.len() {
        let (tc, tr) = (cursors(c, index), cursors(r, index));
        report.records += tc.mi_count as u64;
        report.tx_blocks += tc.idct_blocks_ptr as u64;
        report.coefs += (tc.dq_buffer_ptr - tc.dq_buffer_offset) as u64;
        // SAFETY: two values of a C struct of numbers, the padding of which is zero (the arena's memory
        // was zeroed and the struct is written field by field).
        let same = unsafe {
            std::slice::from_raw_parts((&raw const tc).cast::<u8>(), size_of::<av1_tile_data>())
                == std::slice::from_raw_parts((&raw const tr).cast::<u8>(), size_of::<av1_tile_data>())
        };
        if !same {
            report.difference("tile data", || {
                format!(
                    "frame {number} tile {index}: reference records {} transform blocks {} coefficients to {} index list {} \
                     blocks {} warp {} iterations {}/{} (max {}/{}) inter {} sizes {:?}; checked records {} transform \
                     blocks {} coefficients to {} index list {} blocks {} warp {} iterations {}/{} (max {}/{}) inter {} \
                     sizes {:?}",
                    tc.mi_count,
                    tc.idct_blocks_ptr,
                    tc.dq_buffer_ptr,
                    tc.gen_index_ptr,
                    tc.block_count,
                    tc.block_count_wrp,
                    tc.gen_intra_iter_y,
                    tc.gen_intra_iter_uv,
                    tc.intra_iter_max,
                    tc.intra_iter_max_uv,
                    tc.have_inter,
                    tc.idct_blocks_sizes,
                    tr.mi_count,
                    tr.idct_blocks_ptr,
                    tr.dq_buffer_ptr,
                    tr.gen_index_ptr,
                    tr.block_count,
                    tr.block_count_wrp,
                    tr.gen_intra_iter_y,
                    tr.gen_intra_iter_uv,
                    tr.intra_iter_max,
                    tr.intra_iter_max_uv,
                    tr.have_inter,
                    tr.idct_blocks_sizes
                )
            });
        }
    }

    // The mode info pool.
    // SAFETY: both runs have finished the frame, for all the buffers read below.
    let (pool_c, pool_r) = unsafe { (c.mi_pool.bytes(), r.mi_pool.bytes()) };
    if pool_c != pool_r {
        let fields = record_fields();
        let size = size_of::<MB_MODE_INFO>();
        for (index, (a, b)) in pool_c.chunks_exact(size).zip(pool_r.chunks_exact(size)).enumerate() {
            if a == b {
                continue;
            }
            for field in &fields {
                let range = field.offset..field.offset + field.size;
                if a[range.clone()] == b[range.clone()] {
                    continue;
                }
                report.difference(&format!("record.{}", field.name), || {
                    // SAFETY: a record of each pool.
                    let (mc, mr) = unsafe { (&*c.mi_pool.ptr().add(index), &*r.mi_pool.ptr().add(index)) };
                    format!(
                        "frame {number} record {index}: {}: reference {}, checked {}\n      reference: {}\n      checked:   {}",
                        field.name,
                        hex(&a[range.clone()]),
                        hex(&b[range.clone()]),
                        describe(mc),
                        describe(mr)
                    )
                });
            }
        }
    }

    // The grid: which record every 4x4 unit points to.
    {
        // SAFETY: as above.
        let (grid_c, grid_r) = unsafe { (c.mi_grid.values(), r.mi_grid.values()) };
        // An entry is the place of its record in the pool of its arena (`mi_grid_entry`), 0 for none.
        let index = |entry: u32| -> i64 {
            if entry == 0 { -1 } else { i64::from(entry - MI_GRID_BASE) / size_of::<MB_MODE_INFO>() as i64 }
        };
        let stride = c.size.mi_stride() as usize;
        for (unit, (a, b)) in grid_c.iter().zip(grid_r).enumerate() {
            let (a, b) = (index(*a), index(*b));
            if a != b {
                report.difference("grid", || {
                    format!(
                        "frame {number}: grid row {} col {}: record {a} in the reference, {b} checked",
                        unit / stride,
                        unit % stride
                    )
                });
            }
        }
    }

    // The transform block records.
    {
        // SAFETY: as above.
        let (tx_c, tx_r) = unsafe { (c.tx_blocks.values(), r.tx_blocks.values()) };
        let words = |block: &tx_block_info_gpu| [block.flags, block.input_offset, block.output_pos, block.sorting_idx];
        for (index, (a, b)) in tx_c.iter().zip(tx_r).enumerate() {
            let (a, b) = (words(a), words(b));
            if a == b {
                continue;
            }
            for (name, (x, y)) in ["flags", "input_offset", "output_pos", "sorting_idx"].iter().zip(a.iter().zip(&b)) {
                if x != y {
                    report.difference(&format!("transform block.{name}"), || {
                        format!(
                            "frame {number} transform block {index}: {name}: reference {x:#x}, checked {y:#x} (reference flags {:#x} \
                             offset {} position {:#x} sorting {:#x}; checked flags {:#x} offset {} position {:#x} sorting {:#x})",
                            a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]
                        )
                    });
                }
            }
        }
    }

    // The coefficients.
    {
        // SAFETY: as above.
        let (coef_c, coef_r) = unsafe { (c.coefs.values(), r.coefs.values()) };
        if coef_c != coef_r {
            // The transform block of the reference a coefficient belongs to, for the message.
            // SAFETY: as above.
            let tx_c = unsafe { c.tx_blocks.values() };
            let owner = |at: usize| -> String {
                tx_c.iter()
                    .enumerate()
                    .find(|(_, block)| {
                        let count = (block.flags & 0x7FF) as usize * 4;
                        count != 0 && (block.input_offset as usize..block.input_offset as usize + count).contains(&at)
                    })
                    .map_or_else(
                        || String::from("in no transform block of the reference"),
                        |(index, block)| {
                            format!(
                                "coefficient {} of transform block {index} (flags {:#x}, position {:#x}, sorting {:#x})",
                                at - block.input_offset as usize,
                                block.flags,
                                block.output_pos,
                                block.sorting_idx
                            )
                        },
                    )
            };
            let show = |value: i32| if value == COEF_PATTERN { String::from("not written") } else { value.to_string() };
            for (at, (a, b)) in coef_c.iter().zip(coef_r).enumerate() {
                if a != b {
                    report.difference("coefficient", || {
                        format!(
                            "frame {number}: coefficient buffer [{at}]: reference {}, checked {}; {}",
                            show(*a),
                            show(*b),
                            owner(at)
                        )
                    });
                }
            }
        }
    }

    compare_numbers(report, number, "block index list", &c.gen_indexes, &r.gen_indexes);
    compare_numbers(report, number, "block map", &c.block_map, &r.block_map);
    if c.wave_edges == r.wave_edges {
        compare_numbers(report, number, "wavefront grid", &c.iter_grid, &r.iter_grid);
    } else {
        // -wave check: one side keeps the wavefront in the grid, the other in the edges of its tiles and
        // writes of the grid the one entry per transform block the shader reads. The lists above say that
        // every block has the same iteration; here every entry the edges' side wrote has to be the grid's.
        let (grid, edges) = if c.wave_edges { (r, c) } else { (c, r) };
        // SAFETY: both runs have finished the frame.
        let (grid, edges) = unsafe { (grid.iter_grid.values(), edges.iter_grid.values()) };
        for (index, (a, b)) in grid.iter().zip(edges).enumerate() {
            if *b != -1 && a != b {
                report.difference("wavefront entry", || {
                    format!("frame {number}: wavefront grid[{index}]: the grid has {a}, the edges wrote {b}")
                });
            }
        }
    }

    // The palette blocks, tile by tile in the order they were read.
    for (tile, (pc, pr)) in c.palettes.iter().zip(&r.palettes).enumerate() {
        let (pc, pr) = (pc.lock().expect("no thread panics with it"), pr.lock().expect("no thread panics with it"));
        report.palettes += pc.len() as u64;
        if pc.len() != pr.len() {
            report.difference("palette count", || {
                format!(
                    "frame {number} tile {tile}: {} palette planes in the reference, {} checked",
                    pc.len(),
                    pr.len()
                )
            });
        }
        for (index, (a, b)) in pc.iter().zip(pr.iter()).enumerate() {
            if a == b {
                continue;
            }
            let kind = if (a.mi_row, a.mi_col, a.plane) != (b.mi_row, b.mi_col, b.plane) {
                "palette block"
            } else if a.colors != b.colors {
                "palette colours"
            } else if (a.width, a.height) != (b.width, b.height) {
                "palette size"
            } else {
                "palette map"
            };
            report.difference(kind, || {
                let first = a.map.iter().zip(&b.map).position(|(x, y)| x != y);
                format!(
                    "frame {number} tile {tile} palette {index}: {kind}: reference row {} col {} plane {} {}x{} colours {:?}; \
                     checked row {} col {} plane {} {}x{} colours {:?}; first map difference at {first:?}",
                    a.mi_row, a.mi_col, a.plane, a.width, a.height, a.colors, b.mi_row, b.mi_col, b.plane, b.width, b.height,
                    b.colors
                )
            });
        }
    }

    // The loop restoration units.
    {
        let mut lc = c.lr_units.lock().expect("no thread panics with it").clone();
        let mut lr = r.lr_units.lock().expect("no thread panics with it").clone();
        lc.sort();
        lr.sort();
        report.lr_units += lc.len() as u64;
        if lc != lr {
            report.difference("loop restoration units", || {
                let first = lc.iter().zip(&lr).find(|(a, b)| a != b);
                format!(
                    "frame {number}: loop restoration: {} units in the reference, {} checked; first difference {first:?}",
                    lc.len(),
                    lr.len()
                )
            });
        }
    }
}

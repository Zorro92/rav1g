#!/usr/bin/env python3
# Copyright (c) 2020, Alliance for Open Media. All rights reserved
#
# This source code is subject to the terms of the BSD 2 Clause License and
# the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
# was not distributed with this source code in the LICENSE file, you can
# obtain it at www.aomedia.org/license/software. If the Alliance for Open
# Media Patent License 1.0 was not distributed with this source code in the
# PATENTS file, you can obtain it at www.aomedia.org/license/patent.
"""Reads the stage dumps of the test player and of warp_decoder (-dumpstages) and compares two runs.

  stage_dump.py list <file or directory>...
      what is in the dumps: one line per dump (frame, stage, layout) and the frame's info line

  stage_dump.py compare <A> <B> [--all] [--samples N]
      A and B: two .stages files, or two directories (the files of the same frame number are paired; with
      several streams in a directory, the files of the same name). Either side may be a digest. Per frame
      and stage: equal or not, and for the first stage that differs the pattern of the differences in
      every plane (how many, how large, rows and columns, where in the 8x8 and 64x64 grid, which byte of a
      32-bit word, runs). --all prints the pattern for every stage that differs. The data buffers of a
      dump (the mode info, the prediction block records, the loop filter constants and edge lists, the
      CDEF and loop restoration lists: what the stages read besides the frame) are compared too, and where
      they differ the entries are printed decoded, both sides: for an edge list the block, its place in
      the plane and every edge (position, level, filter, step); for the mode info the position and the
      fields. Exit code 1 when a stage or a buffer differs or a frame is missing on one side.

  stage_dump.py edges <file.stages> [--dir vert|horz] [--plane 0|1|2] [--x X] [--y Y]
      the loop filter edge list of the block that holds sample (X, Y) of the plane, decoded; without
      --x/--y the first blocks of the list

  stage_dump.py mi <file.stages> <mi column> <mi row>
      the mode info record of a 4x4 unit, field by field

  stage_dump.py digest <file.stages or directory> <output directory>
      writes <name>.digest: the same dumps with the samples replaced by a hash per row and a CRC per 8x8
      block. A digest has no samples (it is not media) and is small; compare takes it in place of a dump
      and then names the rows and blocks that differ, but not by how much. Of the data buffers a digest
      keeps the loop filter, CDEF and loop restoration lists as they are, and has a CRC per 4x4 unit of
      the mode info and per prediction block record.

  stage_dump.py extract <file.stages> <stage> <out.yuv> [--occurrence N]
      the visible samples of one dump, planar (Y, U, V; one byte a sample for 8-bit, else two, low first)

  stage_dump.py compare-yuv <file.stages> <stage> <ref.yuv> [--frame N] [--width W --height H]
      compares one dump with frame N of a planar 4:2:0 file of the same sample size (for example a
      reference decoder's output; --width/--height when the file's frames are not of the dump's size)

Frame numbers are the decoder's: frames in decoding order from 0, hidden frames included; a
show_existing_frame is not a frame of its own (its output dump is appended to the file of the frame it
shows, marked sef).

No dependencies beyond the Python standard library.
"""

import argparse
import hashlib
import os
import re
import struct
import sys
import zlib

MAGIC_DUMPS = (b"RAV1STG1", b"RAV1STG2")  # format 2: data buffers (kind 2) with a layout text
MAGIC_DIGESTS = (b"RAV1DIG1", b"RAV1DIG2")
MAGIC_DIGEST = b"RAV1DIG2"
HEADER_SIZE = 384
PLANE_NAMES = ("Y", "U", "V")
KIND_DATA = 2
# Data buffers a digest keeps as they are: small lists and constants. Of the others it keeps CRCs.
DIGEST_KEEPS = ("lf_params", "lf_blocks", "cdef_info", "lr_info")


class Dump:
    """One dump: its header, and either the buffer (data) or, for a digest, the hashes."""

    def __init__(self):
        self.frame = 0
        self.index = 0
        self.kind = 0
        self.bytes_per_sample = 1
        self.bit_depth = 8
        self.is_sef = 0
        self.planes = []  # (offset, stride, width, height)
        self.stage = ""
        self.info = ""
        self.data = None  # bytes of the buffer; None for a digest
        self.layout = ""  # kind 2: the text that describes the data
        self.fields = {}  # kind 2: the fields of the first line of the layout
        self.parts = []  # kind 2: (first word, fields) of the other lines
        self.crcs = None  # kind 2 in a digest that keeps CRCs: the list of CRCs
        self.row_hashes = None  # per plane: list of 8-byte hashes, one per row
        self.block_crcs = None  # per plane: list of CRCs, one per 8x8 block, row-major
        self.path = ""

    def label(self):
        return "%s%s" % (self.stage, " (sef)" if self.is_sef else "")

    def has_plane(self, p):
        return self.planes[p][2] > 0 and self.planes[p][3] > 0

    def is_data(self):
        return self.kind == KIND_DATA

    def set_layout(self, text):
        self.layout = text
        self.fields, self.parts = {}, []
        for n, line in enumerate(text.splitlines()):
            words = line.split()
            fields = dict(w.split("=", 1) for w in words if "=" in w)
            if n == 0:
                self.fields = fields
            elif words:
                self.parts.append((words[0], fields))

    def num(self, key, default=0):
        try:
            return int(self.fields.get(key, default))
        except ValueError:
            return default

    def row(self, p, y):
        offset, stride, width, _ = self.planes[p]
        start = offset + y * stride
        return self.data[start:start + width * self.bytes_per_sample]

    def rows(self, p):
        return [self.row(p, y) for y in range(self.planes[p][3])]


def plane_hashes(rows, width, bps):
    """Row hashes and 8x8 block CRCs of a plane given as a list of row byte strings."""
    row_hashes = [hashlib.md5(r).digest()[:8] for r in rows]
    crcs = []
    step = 8 * bps
    for by in range(0, len(rows), 8):
        band = rows[by:by + 8]
        for bx in range(0, width * bps, step):
            c = 0
            for r in band:
                c = zlib.crc32(r[bx:bx + step], c)
            crcs.append(c)
    return row_hashes, crcs


def read_file(path):
    """All dumps of a .stages or .digest file, in file order."""
    dumps = []
    with open(path, "rb") as f:
        while True:
            header = f.read(HEADER_SIZE)
            if not header:
                break
            if len(header) < HEADER_SIZE or header[:8] not in MAGIC_DUMPS + MAGIC_DIGESTS:
                raise ValueError("%s: not a stage dump at offset %d" % (path, f.tell() - len(header)))
            (size,) = struct.unpack_from("<I", header, 8)
            if size != HEADER_SIZE:
                raise ValueError("%s: header of %d bytes, this script reads %d" % (path, size, HEADER_SIZE))
            d = Dump()
            d.path = path
            d.frame, d.index, d.kind, d.bytes_per_sample, d.bit_depth, d.is_sef = struct.unpack_from("<6i", header, 12)
            d.planes = [struct.unpack_from("<4I", header, 40 + 16 * p) for p in range(3)]
            (data_size,) = struct.unpack_from("<Q", header, 88)
            d.stage = header[96:128].split(b"\0")[0].decode("ascii", "replace")
            d.info = header[128:384].split(b"\0")[0].decode("ascii", "replace")
            data = f.read(data_size)
            if len(data) != data_size:
                raise ValueError("%s: dump %d (%s) is cut short" % (path, d.index, d.stage))
            (layout_size,) = struct.unpack_from("<I", header, 36)
            if d.kind == KIND_DATA:
                d.set_layout(data[:layout_size].decode("ascii", "replace"))
                data = data[layout_size:]
                if header[:8] in MAGIC_DUMPS or d.fields.get("digest", "raw") == "raw":
                    d.data = data
                else:
                    d.crcs = list(struct.unpack_from("<%dI" % (len(data) // 4), data, 0))
            elif header[:8] in MAGIC_DUMPS:
                d.data = data
            else:
                d.row_hashes, d.block_crcs = [], []
                pos = 0
                for p in range(3):
                    _, _, width, height = d.planes[p]
                    d.row_hashes.append([data[pos + 8 * i:pos + 8 * i + 8] for i in range(height)])
                    pos += 8 * height
                    blocks = ((width + 7) // 8) * ((height + 7) // 8)
                    d.block_crcs.append(list(struct.unpack_from("<%dI" % blocks, data, pos)))
                    pos += 4 * blocks
            dumps.append(d)
    return dumps


def hashes_of(d, p):
    if d.data is None:
        return d.row_hashes[p], d.block_crcs[p]
    return plane_hashes(d.rows(p), d.planes[p][2], d.bytes_per_sample)


def write_digest(dumps, path):
    with open(path, "wb") as f:
        for d in dumps:
            body = b""
            layout = b""
            if d.is_data():
                body, layout = data_digest(d, dumps)
            for p in range(3):
                if d.has_plane(p):
                    row_hashes, crcs = hashes_of(d, p)
                    body += b"".join(row_hashes) + struct.pack("<%dI" % len(crcs), *crcs)
            body = layout + body
            header = bytearray(HEADER_SIZE)
            header[:8] = MAGIC_DIGEST
            struct.pack_into("<I", header, 8, HEADER_SIZE)
            struct.pack_into("<6i", header, 12, d.frame, d.index, d.kind, d.bytes_per_sample, d.bit_depth, d.is_sef)
            struct.pack_into("<I", header, 36, len(layout))
            for p in range(3):
                struct.pack_into("<4I", header, 40 + 16 * p, *d.planes[p])
            struct.pack_into("<Q", header, 88, len(body))
            header[96:96 + min(31, len(d.stage))] = d.stage.encode("ascii", "replace")[:31]
            header[128:128 + min(255, len(d.info))] = d.info.encode("ascii", "replace")[:255]
            f.write(bytes(header))
            f.write(body)


# ---------------------------------------------------------------------------------------------------------
# Data buffers (kind 2): what the stages read besides the frame. The layouts are described in
# testing/common/stage_dump.h.
# ---------------------------------------------------------------------------------------------------------

# MB_MODE_INFO of libav1/dx/shaders/mode_info.h: (name, offset, bytes, signed; bytes 0: shown as hex of the
# given length in |raw|).
MI_FIELDS = (
    ("palette", 0, 52, None), ("wm_params", 52, 44, None), ("interinter_comp", 96, 16, None),
    ("filter_intra", 112, 4, False), ("mv0", 116, 4, None), ("mv1", 120, 4, None), ("interp_filters", 124, 4, None),
    ("interintra_wedge_index", 128, 4, True), ("interintra_wedge_sign", 132, 4, True),
    ("current_qindex", 136, 4, True), ("delta_lf_from_base", 140, 4, True), ("delta_lf", 144, 16, None),
    ("mi_row", 160, 4, True), ("mi_col", 164, 4, True), ("index_base", 168, 4, True),
    ("num_proj_ref", 172, 4, True), ("cfl_alpha_idx", 176, 4, True), ("cfl_alpha_signs", 180, 4, True),
    ("comp_group_idx", 184, 2, True), ("compound_idx", 186, 2, True), ("sb_type", 188, 1, False),
    ("partition", 189, 1, False), ("ref_frame0", 190, 1, True), ("ref_frame1", 191, 1, True),
    ("mode", 192, 1, False), ("uv_mode", 193, 1, False), ("interintra_mode", 194, 1, False),
    ("motion_mode", 195, 1, False), ("use_wedge_interintra", 196, 1, True), ("use_intrabc", 197, 1, False),
    ("angle_delta_y", 198, 1, True), ("angle_delta_uv", 199, 1, True), ("tx_size", 200, 1, False),
    ("skip", 201, 1, True), ("skip_mode", 202, 1, True), ("segment_id", 203, 1, True),
    ("inter_tx_size", 204, 16, None), ("seg_id_predicted", 220, 1, True), ("cdef_strength", 221, 1, True),
    ("ref_mv_idx", 222, 1, False), ("reserved", 223, 1, True),
)
# The fields gen_lf_blocks_vert/hor read of a record (the last two only with delta_lf_present).
MI_LF_FIELDS = ("sb_type", "ref_frame0", "mode", "use_intrabc", "tx_size", "skip", "segment_id", "inter_tx_size",
                "delta_lf_from_base", "delta_lf")
FILTER_TAPS = (4, 6, 8, 14)


def field_value(record, field):
    name, offset, size, signed = field
    raw = record[offset:offset + size]
    if signed is None:
        return raw.hex()
    return int.from_bytes(raw, "little", signed=signed)


def words_of(data, offset, count):
    return struct.unpack_from("<%di" % count, data, offset)


def sibling(dumps, stage):
    for d in dumps:
        if d.stage == stage and d.is_data():
            return d
    return None


class ModeInfo:
    """The mode info of a frame: mi_grid and mi_pool of one file."""

    def __init__(self, grid, pool):
        self.grid, self.pool = grid, pool
        self.stride = grid.num("mi_stride")
        self.cols = grid.num("mi_cols")
        self.rows = grid.num("mi_rows")
        self.base = grid.num("pool_base")
        self.record = grid.num("record", 224)
        self.runs = []  # (first pool index, count, offset in the data)
        pos = 0
        for word, fields in (pool.parts if pool else []):
            if word == "run":
                first, count = int(fields["first"]), int(fields["count"])
                self.runs.append((first, count, pos))
                pos += count * self.record
        self.cache = {}

    def index(self, col, row):
        """Pool index of the record of a unit; None when the grid has no record there."""
        pos = (row * self.stride + col) * 8
        if pos + 8 > len(self.grid.data):
            return None
        low, high = struct.unpack_from("<II", self.grid.data, pos)
        if low == 0 and high == 0:
            return None
        return ((low - self.base) & 0xffffffff) // self.record

    def record_at(self, index):
        """The record with a pool index; None when no run of the dump has it."""
        if index is None:
            return None
        if index not in self.cache:
            found = None
            for first, count, pos in self.runs:
                if first <= index < first + count:
                    start = pos + (index - first) * self.record
                    found = self.pool.data[start:start + self.record]
                    break
            self.cache[index] = found
        return self.cache[index]


def mode_info_of(dumps):
    grid, pool = sibling(dumps, "mi_grid"), sibling(dumps, "mi_pool")
    if grid is None or grid.data is None or pool is None or pool.data is None:
        return None
    return ModeInfo(grid, pool)


def data_digest(d, dumps):
    """Body and layout of a data buffer in a digest."""
    def with_mode(mode):
        lines = d.layout.splitlines() or [""]
        if "digest=" not in lines[0]:
            lines[0] += " digest=%s" % mode
        return ("\n".join(lines) + "\n").encode("ascii", "replace")

    if d.data is None:  # digest of a digest
        return struct.pack("<%dI" % len(d.crcs), *d.crcs), with_mode("crc")
    if d.stage in DIGEST_KEEPS:
        return d.data, with_mode("raw")
    if d.stage == "mi_grid":
        mi = mode_info_of(dumps)
        crcs = []
        if mi:
            by_index = {}
            for row in range(mi.rows):
                for col in range(mi.cols):
                    index = mi.index(col, row)
                    if index not in by_index:
                        record = mi.record_at(index)
                        by_index[index] = zlib.crc32(record) if record else 0
                    crcs.append(by_index[index])
        return struct.pack("<%dI" % len(crcs), *crcs), with_mode("crc")
    if d.stage == "mi_pool":
        return b"", with_mode("crc")  # its records are in the CRCs of mi_grid
    record = max(4, d.num("record", 32))
    crcs = [zlib.crc32(d.data[i:i + record]) for i in range(0, len(d.data), record)]
    return struct.pack("<%dI" % len(crcs), *crcs), with_mode("crc")


def lf_lists(d):
    """The lists of an lf_blocks dump: dicts with dir, plane, first, cols, count, mi_cols, mi_rows, on."""
    out = []
    for word, fields in d.parts:
        if word == "list":
            item = dict((k, v if k == "dir" else int(v)) for k, v in fields.items())
            out.append(item)
    return out


def lf_list_name(item):
    return "%s %s edges" % (PLANE_NAMES[item["plane"]], "vertical" if item["dir"] == "vert" else "horizontal")


def lf_block_place(item, block):
    """Where a block of a list is: (text, first edge position, name of the edge coordinate)."""
    bx, by = block % item["cols"], block // item["cols"]
    if item["dir"] == "vert":
        return ("columns %d..%d, rows %d..%d" % (bx * 64, bx * 64 + 63, by * 4, by * 4 + 3), bx * 64, "x")
    return ("columns %d..%d, rows %d..%d" % (bx * 4, bx * 4 + 3, by * 64, by * 64 + 63), by * 64, "y")


def lf_block_edges(data, item, block):
    """The edges of a block as the filter walks them: [(entry, level, filter, step)], and the entries behind
    the end of the walk that are not 0 (the filter does not read them)."""
    entries = struct.unpack_from("<16H", data, (item["first"] + block) * 32)
    edges = []
    reached = set()
    i = 0
    while i < 16:
        e = entries[i]
        if not e:
            break
        reached.add(i)
        edges.append((i, e & 63, (e >> 6) & 3, e >> 8))
        if not e >> 8:
            break
        i += e >> 8
    stray = [(i, entries[i]) for i in range(16) if entries[i] and i not in reached]
    return edges, stray


def lf_edges_text(item, block, edges, stray):
    _, origin, axis = lf_block_place(item, block)
    parts = []
    for entry, level, filt, step in edges:
        parts.append("%s=%d %s step %d" % (axis, origin + entry * 4,
                                          ("level %d %d-tap" % (level, FILTER_TAPS[filt])) if level else "not filtered",
                                          step))
    text = "; ".join(parts) if parts else "no edge"
    if stray:
        text += " | entries the filter does not reach: " + " ".join("%d:0x%04x" % e for e in stray)
    return text


def compare_lf_blocks(da, db, limit, out):
    la, lb = lf_lists(da), lf_lists(db)
    if [dict(i) for i in la] != [dict(i) for i in lb]:
        out("      the lists are laid out differently:")
        for name, lists in (("A", la), ("B", lb)):
            for item in lists:
                out("        %s %s: first %d, %d blocks in rows of %d, on=%d" % (name, lf_list_name(item), item["first"],
                                                                              item["count"], item["cols"], item["on"]))
        return False
    equal = True
    for item in la:
        name = lf_list_name(item)
        if not item["on"]:
            out("      %s: the plane is not filtered in this frame (list not written, not compared)" % name)
            continue
        bad = []
        stray_only = 0
        for block in range(item["count"]):
            pos = (item["first"] + block) * 32
            if da.data[pos:pos + 32] == db.data[pos:pos + 32]:
                continue
            ea, eb = lf_block_edges(da.data, item, block), lf_block_edges(db.data, item, block)
            if ea[0] == eb[0]:
                stray_only += 1
            bad.append((block, ea, eb))
        if not bad:
            out("      %s: equal (%d blocks)" % (name, item["count"]))
            continue
        equal = False
        xs = [b % item["cols"] for b, _, _ in bad]
        ys = [b // item["cols"] for b, _, _ in bad]
        out("      %s: %d of %d blocks DIFFER (block columns %d..%d of %d, block rows %d..%d of %d)%s"
            % (name, len(bad), item["count"], min(xs), max(xs), item["cols"], min(ys), max(ys),
               (item["count"] + item["cols"] - 1) // max(item["cols"], 1),
               "; %d of them only in entries the filter does not reach" % stray_only if stray_only else ""))
        for block, ea, eb in bad[:limit]:
            place, _, _ = lf_block_place(item, block)
            out("        block %d (block column %d, block row %d: %s)" % (block, block % item["cols"],
                                                                         block // item["cols"], place))
            out("          A: " + lf_edges_text(item, block, *ea))
            out("          B: " + lf_edges_text(item, block, *eb))
        if len(bad) > limit:
            out("        ... and %d more blocks (--samples N lists more)" % (len(bad) - limit))
    return equal


def lf_params_fields(d):
    """(name, value) of the loop filter constants that the shaders read."""
    gen_size, lf_size = d.num("gen_size"), d.num("lf_size")
    out = []
    if len(d.data) < gen_size + lf_size or gen_size < 4 * 1876 or lf_size < 4 * 268:
        return out
    g = words_of(d.data, 0, gen_size // 4)
    out += [("mi_stride", g[0]), ("delta_lf_present", g[2]), ("delta_lf_multi", g[3]),
            ("mode_ref_delta_enabled", g[4]), ("filter_level[0] (vertical edges)", g[8]),
            ("filter_level[1] (horizontal edges)", g[9]), ("filter_level_u", g[10]), ("filter_level_v", g[11])]
    out += [("mode_deltas[%d]" % i, g[12 + 4 * i]) for i in range(2)]
    out += [("ref_deltas[%d]" % i, g[20 + 4 * i]) for i in range(8)]
    for i in range(8):
        out += [("segment %d lossless" % i, g[52 + 4 * i]), ("segment %d feature mask" % i, g[53 + 4 * i])]
    out += [("segment %d feature %d data" % (i, j), g[84 + 4 * (8 * i + j)]) for i in range(8) for j in range(8)]
    for p in range(3):
        for seg in range(8):
            for ref in range(8):
                for mode in range(2):
                    base = 340 + 4 * (((p * 8 + seg) * 8 + ref) * 2 + mode)
                    for direction in range(2):
                        out.append(("level %s segment %d ref %d mode %d %s edges"
                                    % (PLANE_NAMES[p], seg, ref, mode, ("vertical", "horizontal")[direction]),
                                    g[base + direction]))
    f = words_of(d.data, gen_size, lf_size // 4)
    out += [("%s stride" % PLANE_NAMES[p], f[4 * p]) for p in range(3)]
    for level in range(64):
        out += [("level %d lim" % level, f[12 + 4 * level]), ("level %d mblim" % level, f[13 + 4 * level]),
                ("level %d hev_thr" % level, f[14 + 4 * level])]
    return out


def compare_lf_params(da, db, limit, out):
    fa, fb = lf_params_fields(da), lf_params_fields(db)
    if not fa or len(fa) != len(fb):
        out("      the constants cannot be read (sizes: A %d bytes, B %d bytes)" % (len(da.data), len(db.data)))
        return da.data == db.data
    bad = [(fa[i][0], fa[i][1], fb[i][1]) for i in range(len(fa)) if fa[i][1] != fb[i][1]]
    if not bad:
        out("      equal (levels %d,%d,%d,%d; %d constants the shaders read)" % (fa[4][1], fa[5][1], fa[6][1], fa[7][1],
                                                                              len(fa)))
        return True
    out("      %d of %d constants DIFFER (name: A -> B)" % (len(bad), len(fa)))
    for name, a, b in bad[:max(limit, 24)]:
        out("        %s: %d -> %d" % (name, a, b))
    if len(bad) > max(limit, 24):
        out("        ... and %d more" % (len(bad) - max(limit, 24)))
    return False


def compare_mode_info(da, db, dumps_a, dumps_b, limit, out):
    if da.crcs is not None or db.crcs is not None:
        # One side is a digest: a CRC per 4x4 unit.
        ca = da.crcs if da.crcs is not None else data_crcs(da, dumps_a)
        cb = db.crcs if db.crcs is not None else data_crcs(db, dumps_b)
        cols = da.num("mi_cols")
        bad = [i for i in range(min(len(ca), len(cb))) if ca[i] != cb[i]]
        if not bad and len(ca) == len(cb):
            out("      equal (%d units, by CRC)" % len(ca))
            return True
        out("      %d of %d units DIFFER (one side is a digest: which units, not which fields)" % (len(bad), len(ca)))
        if cols:
            out("        first (mi column,row): " + " ".join("%d,%d" % (i % cols, i // cols) for i in bad[:limit]))
        return False
    ma, mb = mode_info_of(dumps_a), mode_info_of(dumps_b)
    if ma is None or mb is None:
        out("      not compared: the records (mi_pool) are missing on one side")
        return True
    if (ma.cols, ma.rows) != (mb.cols, mb.rows):
        out("      sizes differ: A %dx%d units, B %dx%d" % (ma.cols, ma.rows, mb.cols, mb.rows))
        return False
    pair_fields = {}
    per_field = {}
    first = {}
    bad_units = lf_units = missing = 0
    for row in range(ma.rows):
        for col in range(ma.cols):
            ia, ib = ma.index(col, row), mb.index(col, row)
            key = (ia, ib)
            if key not in pair_fields:
                ra, rb = ma.record_at(ia), mb.record_at(ib)
                if ra is None or rb is None:
                    pair_fields[key] = None if (ra is None) != (rb is None) or ia != ib and (ia is None or ib is None) \
                        else ()
                elif ra == rb:
                    pair_fields[key] = ()
                else:
                    pair_fields[key] = tuple(f for f in MI_FIELDS if field_value(ra, f) != field_value(rb, f))
            fields = pair_fields[key]
            if fields is None:
                missing += 1
                bad_units += 1
                if len(first.setdefault("(no record on one side)", [])) < limit:
                    first["(no record on one side)"].append((col, row, "record" if ia is not None else "none",
                                                           "record" if ib is not None else "none"))
                continue
            if not fields:
                continue
            bad_units += 1
            if any(f[0] in MI_LF_FIELDS for f in fields):
                lf_units += 1
            ra, rb = ma.record_at(ia), mb.record_at(ib)
            for f in fields:
                per_field[f[0]] = per_field.get(f[0], 0) + 1
                if len(first.setdefault(f[0], [])) < limit:
                    va, vb = field_value(ra, f), field_value(rb, f)
                    if f[3] is None and f[2] > 16:
                        # A long field: the bytes from the first to the last that differ.
                        diff = [i for i in range(f[2]) if ra[f[1] + i] != rb[f[1] + i]]
                        lo, hi = diff[0], min(diff[-1] + 1, diff[0] + 16)
                        va, vb = "[byte %d]%s" % (lo, va[2 * lo:2 * hi]), vb[2 * lo:2 * hi]
                    first[f[0]].append((col, row, va, vb))
    total = ma.cols * ma.rows
    if not bad_units:
        out("      equal (%d 4x4 units of %dx%d, %d and %d records)" % (total, ma.cols, ma.rows, len(ma.cache),
                                                                       len(mb.cache)))
        return True
    out("      %d of %d 4x4 units have a record that DIFFERS; %d of them in a field the loop filter lists read%s"
        % (bad_units, total, lf_units, "; %d have a record on one side only" % missing if missing else ""))
    out("      per field: units that differ, then the first (mi column,row: A -> B); * = read by gen_lf_blocks")
    for name in sorted(first, key=lambda n: -per_field.get(n, missing)):
        out("        %s%s: %d units; %s" % ("*" if name in MI_LF_FIELDS else "", name, per_field.get(name, missing),
                                         " ".join("%d,%d:%s->%s" % v for v in first[name])))
    return False


def data_crcs(d, dumps):
    body, _ = data_digest(d, dumps)
    return list(struct.unpack_from("<%dI" % (len(body) // 4), body, 0))


def compare_records(da, db, dumps_a, dumps_b, limit, out):
    """Buffers without a decoder of their own: records of the layout's size, shown as words."""
    record = max(4, da.num("record", 32))
    if da.crcs is not None or db.crcs is not None:
        ca = da.crcs if da.crcs is not None else data_crcs(da, dumps_a)
        cb = db.crcs if db.crcs is not None else data_crcs(db, dumps_b)
        bad = [i for i in range(min(len(ca), len(cb))) if ca[i] != cb[i]]
        if not bad and len(ca) == len(cb):
            out("      equal (%d records, by CRC)" % len(ca))
            return True
        out("      %d of %d records DIFFER (A has %d, B %d; one side is a digest); first: %s"
            % (len(bad), min(len(ca), len(cb)), len(ca), len(cb), " ".join(str(i) for i in bad[:limit])))
        return False
    na, nb = len(da.data) // record, len(db.data) // record
    if da.data == db.data:
        out("      equal (%d records of %d bytes)" % (na, record))
        return True
    bad = [i for i in range(min(na, nb))
           if da.data[i * record:(i + 1) * record] != db.data[i * record:(i + 1) * record]]
    out("      %d of %d records of %d bytes DIFFER (A has %d, B %d)" % (len(bad), min(na, nb), record, na, nb))
    for i in bad[:limit]:
        wa = struct.unpack_from("<%dI" % (record // 4), da.data, i * record)
        wb = struct.unpack_from("<%dI" % (record // 4), db.data, i * record)
        out("        record %d  A: %s" % (i, " ".join("%08x" % w for w in wa)))
        out("        %s  B: %s" % (" " * len("record %d" % i), " ".join("%08x" % w for w in wb)))
    if len(bad) > limit:
        out("        ... and %d more" % (len(bad) - limit))
    return False


def compare_cdef_info(da, db, limit, out):
    keys = ("index_size", "index_stride", "index_rows", "skips_stride", "skip_cols", "skip_rows")
    if [da.num(k) for k in keys] != [db.num(k) for k in keys]:
        out("      laid out differently: A %s; B %s" % (da.layout.splitlines()[0], db.layout.splitlines()[0]))
        return False
    index_size, stride, rows = da.num("index_size"), da.num("index_stride"), da.num("index_rows")
    ia, ib = words_of(da.data, 0, index_size // 4), words_of(db.data, 0, index_size // 4)
    bad = [i for i in range(len(ia)) if ia[i] != ib[i]]
    equal = not bad
    if bad:
        out("      strength index: %d of %d 64x64 blocks DIFFER (block column,row: A -> B): %s"
            % (len(bad), len(ia), " ".join("%d,%d:%d->%d" % (i % stride, i // stride, ia[i], ib[i]) for i in bad[:limit])))
    else:
        out("      strength index: equal (%d 64x64 blocks)" % len(ia))
    sstride, scols, srows = da.num("skips_stride"), da.num("skip_cols"), da.num("skip_rows")
    count = (min(len(da.data), len(db.data)) - index_size) // 4
    sa, sb = words_of(da.data, index_size, count), words_of(db.data, index_size, count)
    inside, outside = [], 0
    for i in range(count):
        if sa[i] != sb[i]:
            if i % sstride < scols and i // sstride < srows:
                inside.append(i)
            else:
                outside += 1
    if inside:
        equal = False
        out("      skip flags: %d of %d 8x8 blocks DIFFER (block column,row: A -> B): %s"
            % (len(inside), scols * srows, " ".join("%d,%d:%d->%d" % (i % sstride, i // sstride, sa[i], sb[i])
                                                    for i in inside[:limit])))
    else:
        out("      skip flags: equal (%d by %d 8x8 blocks)" % (scols, srows))
    if outside:
        out("      NOTE %d skip entries differ outside the %d by %d blocks that are written for the frame (rows of %d):"
            " they hold what earlier frames left there" % (outside, scols, srows, sstride))
    return equal


def compare_lr_info(da, db, limit, out):
    units = da.num("units")
    if units != db.num("units") or da.parts != db.parts:
        out("      laid out differently: A %s; B %s" % (" / ".join(da.layout.splitlines()), " / ".join(db.layout.splitlines())))
        return False
    names = ("none", "Wiener", "self guided", "switchable")
    bad = []
    for u in range(units):
        ta, tb = words_of(da.data, u * 16, 4), words_of(db.data, u * 16, 4)
        wa, wb = words_of(da.data, units * 16 + u * 64, 16), words_of(db.data, units * 16 + u * 64, 16)
        what = []
        if ta[0] != tb[0]:
            what.append("type %s -> %s" % (names[ta[0] & 3], names[tb[0] & 3]))
        elif ta[0] == 2 and ta[1:] != tb[1:]:
            what.append("self guided weights/set %s -> %s" % (ta[1:], tb[1:]))
        elif ta[0] == 1 and wa != wb:
            what.append("Wiener filters %s -> %s" % (wa, wb))
        if what:
            bad.append((u, what[0]))
    if not bad:
        out("      equal (%d units; the fields of a unit's own type)" % units)
        return True
    out("      %d of %d units DIFFER" % (len(bad), units))
    for u, what in bad[:limit]:
        out("        unit %d: %s" % (u, what))
    return False


def compare_data(da, db, dumps_a, dumps_b, limit, out):
    """Compares two data buffers of the same name. Returns True when they are equal."""
    if da.stage == "mi_pool":
        out("      the records are compared through mi_grid, unit by unit")
        return True
    if da.stage == "mi_grid":
        return compare_mode_info(da, db, dumps_a, dumps_b, limit, out)
    if da.data is not None and db.data is not None:
        if da.stage == "lf_blocks":
            return compare_lf_blocks(da, db, limit, out)
        if da.stage == "lf_params":
            return compare_lf_params(da, db, limit, out)
        if da.stage == "cdef_info":
            return compare_cdef_info(da, db, limit, out)
        if da.stage == "lr_info":
            return compare_lr_info(da, db, limit, out)
    return compare_records(da, db, dumps_a, dumps_b, limit, out)


def stage_files(path):
    """The .stages and .digest files of a directory (or the file itself), keyed by (stream, frame)."""
    if os.path.isfile(path):
        names = [path]
    else:
        names = [os.path.join(path, n) for n in sorted(os.listdir(path)) if n.endswith((".stages", ".digest"))]
    out = {}
    for n in names:
        # <stream>_f<frame> and, behind the n-th new sequence of the stream, <stream>.seq<n>_f<frame>.
        m = re.match(r"^(.*?)(?:\.seq(\d+))?_f(\d+)\.(stages|digest)$", os.path.basename(n))
        key = (m.group(1), (int(m.group(2) or 0), int(m.group(3)))) if m else (os.path.basename(n), (0, -1))
        # A dump beats its digest when a directory has both.
        if key not in out or n.endswith(".stages"):
            out[key] = n
    return out


def samples(row, bps, signed):
    if bps == 1:
        return row
    return struct.unpack("<%d%s" % (len(row) // 2, "h" if signed else "H"), row)


def histogram_text(counts, limit=None):
    items = sorted(counts.items())
    if limit and len(items) > limit:
        items = sorted(sorted(counts.items(), key=lambda kv: -kv[1])[:limit])
        return " ".join("%s:%d" % kv for kv in items) + " (the %d most frequent of %d)" % (limit, len(counts))
    return " ".join("%s:%d" % kv for kv in items)


def diff_pattern(rows_a, rows_b, width, bps, signed, max_samples, out):
    """Prints the pattern of the differences between two planes given as lists of row byte strings."""
    height = min(len(rows_a), len(rows_b))
    count = 0
    plus = minus = 0
    max_abs = 0
    mags = {}
    bad_rows = []
    cols = set()
    mod_word = {}
    mod8x = {}
    mod8y = {}
    mod64x = {}
    mod64y = {}
    blocks8 = set()
    blocks64 = {}
    hruns = {}
    first = []
    col_hits = {}
    samples_per_word = 4 // bps
    for y in range(height):
        ra, rb = rows_a[y], rows_b[y]
        if ra == rb:
            continue
        bad_rows.append(y)
        sa, sb = samples(ra, bps, signed), samples(rb, bps, signed)
        run = 0
        for x in range(min(len(sa), len(sb))):
            d = sb[x] - sa[x]
            if d == 0:
                if run:
                    hruns[run] = hruns.get(run, 0) + 1
                    run = 0
                continue
            run += 1
            count += 1
            if d > 0:
                plus += 1
            else:
                minus += 1
            a = abs(d)
            if a > max_abs:
                max_abs = a
            bucket = a if a <= 4 else (8 if a <= 8 else 16 if a <= 16 else 64 if a <= 64 else 256 if a <= 256 else 65536)
            mags[bucket] = mags.get(bucket, 0) + 1
            cols.add(x)
            col_hits[x] = col_hits.get(x, 0) + 1
            mod_word[x % samples_per_word] = mod_word.get(x % samples_per_word, 0) + 1
            mod8x[x & 7] = mod8x.get(x & 7, 0) + 1
            mod8y[y & 7] = mod8y.get(y & 7, 0) + 1
            mod64x[x & 63] = mod64x.get(x & 63, 0) + 1
            mod64y[y & 63] = mod64y.get(y & 63, 0) + 1
            blocks8.add((x >> 3, y >> 3))
            key = (x >> 6, y >> 6)
            blocks64[key] = blocks64.get(key, 0) + 1
            if len(first) < max_samples:
                first.append((x, y, sa[x], sb[x]))
        if run:
            hruns[run] = hruns.get(run, 0) + 1
    total = width * height
    if not count:
        out("      no sample differs")
        return 0
    out("      %d of %d samples differ (%.3f%%): B above A %d, B below A %d, largest difference %d"
        % (count, total, 100.0 * count / total, plus, minus, max_abs))
    names = {1: "1", 2: "2", 3: "3", 4: "4", 8: "5-8", 16: "9-16", 64: "17-64", 256: "65-256", 65536: ">256"}
    out("      size of the difference: " + " ".join("%s:%d" % (names[k], v) for k, v in sorted(mags.items())))
    out("      rows %d..%d (%d rows with a difference), columns %d..%d (%d columns)"
        % (bad_rows[0], bad_rows[-1], len(bad_rows), min(cols), max(cols), len(cols)))
    out("      8x8 blocks with a difference: %d; 64x64 blocks: %d of %d"
        % (len(blocks8), len(blocks64), ((width + 63) // 64) * ((height + 63) // 64)))
    worst = sorted(blocks64.items(), key=lambda kv: -kv[1])[:8]
    out("      64x64 blocks with the most differences (column,row of the block: samples): "
        + " ".join("%d,%d:%d" % (k[0], k[1], v) for k, v in worst))
    out("      sample within its 32-bit word (x mod %d): %s" % (samples_per_word, histogram_text(mod_word)))
    out("      x mod 8: %s" % histogram_text(mod8x))
    out("      y mod 8: %s" % histogram_text(mod8y))
    out("      x mod 64: %s" % histogram_text(mod64x, 24))
    out("      y mod 64: %s" % histogram_text(mod64y, 24))
    out("      horizontal runs of differing samples (length:count): %s" % histogram_text(hruns, 16))
    busiest = sorted(col_hits.items(), key=lambda kv: -kv[1])[:8]
    out("      columns with the most differences (x:samples): " + " ".join("%d:%d" % kv for kv in sorted(busiest)))
    out("      first differences (x,y: A -> B): " + " ".join("%d,%d:%d->%d" % s for s in first))
    return count


def hash_pattern(da, db, p, out):
    """Compares a plane through row hashes and 8x8 block CRCs (one side is a digest)."""
    rows_a, crcs_a = hashes_of(da, p)
    rows_b, crcs_b = hashes_of(db, p)
    width, height = da.planes[p][2], da.planes[p][3]
    bad_rows = [y for y in range(min(len(rows_a), len(rows_b))) if rows_a[y] != rows_b[y]]
    if not bad_rows:
        return 0
    per_row = (width + 7) // 8
    bad = [i for i in range(min(len(crcs_a), len(crcs_b))) if crcs_a[i] != crcs_b[i]]
    blocks64 = {}
    mod8x = {}
    mod8y = {}
    for i in bad:
        bx, by = i % per_row, i // per_row
        blocks64[(bx >> 3, by >> 3)] = blocks64.get((bx >> 3, by >> 3), 0) + 1
        mod8x[bx & 7] = mod8x.get(bx & 7, 0) + 1
        mod8y[by & 7] = mod8y.get(by & 7, 0) + 1
    out("      %d of %d rows differ: rows %d..%d" % (len(bad_rows), height, bad_rows[0], bad_rows[-1]))
    out("      8x8 blocks with a difference: %d of %d; 64x64 blocks: %d of %d"
        % (len(bad), len(crcs_a), len(blocks64), ((width + 63) // 64) * ((height + 63) // 64)))
    if bad:
        xs = [i % per_row for i in bad]
        out("      8x8 block columns %d..%d, 8x8 block rows %d..%d" % (min(xs), max(xs), bad[0] // per_row, bad[-1] // per_row))
        out("      8x8 block within its 64x64 block, column: %s" % histogram_text(mod8x))
        out("      8x8 block within its 64x64 block, row: %s" % histogram_text(mod8y))
        worst = sorted(blocks64.items(), key=lambda kv: -kv[1])[:8]
        out("      64x64 blocks with the most 8x8 blocks that differ (column,row: blocks): "
            + " ".join("%d,%d:%d" % (k[0], k[1], v) for k, v in worst))
        out("      first 8x8 blocks that differ (x,y of the block's first sample): "
            + " ".join("%d,%d" % ((i % per_row) * 8, (i // per_row) * 8) for i in bad[:12]))
    out("      (one side is a digest: which rows and blocks differ, not by how much)")
    return len(bad_rows)


def compare_dumps(da, db, detail, max_samples, out):
    """Compares two dumps of the same stage. Returns True when every plane is equal."""
    equal = True
    lines = []
    for p in range(3):
        if not da.has_plane(p) and not db.has_plane(p):
            continue
        if da.planes[p][2:] != db.planes[p][2:] or da.bytes_per_sample != db.bytes_per_sample:
            lines.append("    %s: sizes differ, A %dx%d %d bytes a sample, B %dx%d %d bytes a sample"
                         % (PLANE_NAMES[p], da.planes[p][2], da.planes[p][3], da.bytes_per_sample, db.planes[p][2],
                            db.planes[p][3], db.bytes_per_sample))
            equal = False
            continue
        width, height = da.planes[p][2], da.planes[p][3]
        if da.data is not None and db.data is not None:
            rows_a, rows_b = da.rows(p), db.rows(p)
            if rows_a == rows_b:
                lines.append("    %s: equal" % PLANE_NAMES[p])
                continue
            equal = False
            bad_rows = sum(1 for y in range(height) if rows_a[y] != rows_b[y])
            lines.append("    %s: DIFFERS in %d of %d rows" % (PLANE_NAMES[p], bad_rows, height))
            if detail:
                diff_pattern(rows_a, rows_b, width, da.bytes_per_sample, da.kind == 1, max_samples, lines.append)
        else:
            sub = []
            if hash_pattern(da, db, p, sub.append):
                equal = False
                lines.append("    %s: DIFFERS" % PLANE_NAMES[p])
                if detail:
                    lines.extend(sub)
                else:
                    lines.append(sub[0])
            else:
                lines.append("    %s: equal" % PLANE_NAMES[p])
    for line in lines:
        out(line)
    return equal


def cmd_list(args):
    for path in args.paths:
        for key, name in sorted(stage_files(path).items(), key=lambda kv: (kv[0][0], kv[0][1])):
            dumps = read_file(name)
            print("%s: %d dumps" % (name, len(dumps)))
            shown = None
            for d in dumps:
                if d.info != shown:
                    print("  frame %d%s: %s" % (d.frame, " (sef)" if d.is_sef else "", d.info))
                    shown = d.info
                if d.is_data():
                    size = len(d.data) if d.data is not None else 4 * len(d.crcs)
                    print("    %2d %-16s data, %d bytes%s: %s"
                          % (d.index, d.label(), size, " (digest)" if d.fields.get("digest") else "",
                             " ".join(w for w in d.layout.splitlines()[0].split() if not w.startswith("buffer="))))
                    for word, fields in d.parts:
                        if word != "run":
                            print("       %s %s" % (word, " ".join("%s=%s" % kv for kv in fields.items())))
                    runs = [f for w, f in d.parts if w == "run"]
                    if runs:
                        print("       %d runs of records, %d records" % (len(runs), sum(int(f["count"]) for f in runs)))
                    continue
                planes = " ".join("%s %dx%d stride %d at %d" % (PLANE_NAMES[p], d.planes[p][2], d.planes[p][3],
                                                               d.planes[p][1], d.planes[p][0])
                                  for p in range(3) if d.has_plane(p))
                print("    %2d %-13s %s, %d bytes a sample, %d-bit%s: %s"
                      % (d.index, d.label(), "residuals" if d.kind == 1 else "samples", d.bytes_per_sample,
                         d.bit_depth, " (digest)" if d.data is None else "", planes))
    return 0


def pair_files(a, b):
    fa, fb = stage_files(a), stage_files(b)
    streams_a = sorted(set(k[0] for k in fa))
    streams_b = sorted(set(k[0] for k in fb))
    pairs = []
    missing = []
    if len(streams_a) == 1 and len(streams_b) == 1:
        # One stream on each side: the two programs may call it differently, the frame number pairs the files.
        by_frame_b = dict((k[1], v) for k, v in fb.items())
        for k, v in sorted(fa.items(), key=lambda kv: kv[0][1]):
            if k[1] in by_frame_b:
                pairs.append((v, by_frame_b.pop(k[1])))
            else:
                missing.append("%s has no counterpart in B" % v)
        missing.extend("%s has no counterpart in A" % v for v in by_frame_b.values())
    else:
        for k, v in sorted(fa.items()):
            if k in fb:
                pairs.append((v, fb[k]))
            else:
                missing.append("%s has no counterpart in B" % v)
        missing.extend("%s has no counterpart in A" % v for k, v in sorted(fb.items()) if k not in fa)
    return pairs, missing


def cmd_compare(args):
    pairs, missing = pair_files(args.a, args.b)
    all_equal = not missing
    for line in missing:
        print("MISSING %s" % line)
    if not pairs:
        print("nothing to compare")
        return 2
    for file_a, file_b in pairs:
        dumps_a, dumps_b = read_file(file_a), read_file(file_b)
        print("A %s\nB %s" % (file_a, file_b))
        if dumps_a:
            print("  frame %d: %s" % (dumps_a[0].frame, dumps_a[0].info))
        if dumps_a and dumps_b and dumps_a[0].info != dumps_b[0].info:
            print("  NOTE the frame's info line differs in B: %s" % dumps_b[0].info)
            all_equal = False

        # Pair the dumps by stage name and by how often the name came before (two superres dumps, sef outputs).
        def keyed(dumps):
            seen = {}
            out = []
            for d in dumps:
                k = (d.stage, d.is_sef)
                seen[k] = seen.get(k, 0) + 1
                out.append(((d.stage, d.is_sef, seen[k]), d))
            return out

        ka, kb = keyed(dumps_a), keyed(dumps_b)
        map_b = dict(kb)
        first_bad = None
        data_bad = []
        records_listed = []
        for key, da in ka:
            db = map_b.pop(key, None)
            if db is None:
                if da.is_data():
                    print("  data  %-16s only in A (B was written without it: not compared)" % da.label())
                else:
                    print("  stage %-13s only in A" % da.label())
                    all_equal = False
                continue
            if da.is_data():
                # A buffer a stage reads besides the frame: always with its decoded differences.
                lines = []
                equal = compare_data(da, db, dumps_a, dumps_b, args.samples, lines.append)
                # --two-parsers: the records themselves are listed and do not decide; what the stages make
                # of them (pred_blocks, lf_blocks, cdef_info) is compared like every other buffer.
                listed = not equal and args.two_parsers and da.stage in ("mi_grid", "mi_pool")
                print("  data  %-16s %s" % (da.label(), "equal" if equal else
                                           "differs in fields no stage reads of these blocks" if listed else "DIFFERS"))
                if listed:
                    records_listed.append(da.label())
                elif not equal:
                    all_equal = False
                    data_bad.append(da.label())
                for line in lines:
                    print(line)
                continue
            detail = args.all or first_bad is None
            lines = []
            equal = compare_dumps(da, db, detail, args.samples, lines.append)
            print("  stage %-13s %s" % (da.label(), "equal" if equal else "DIFFERS"))
            if not equal:
                all_equal = False
                if first_bad is None:
                    first_bad = da.label()
                    print("  ^ first stage that differs; its pattern:")
                for line in lines:
                    print(line)
        for key, db in kb:
            if key in map_b:
                if db.is_data():
                    print("  data  %-16s only in B (A was written without it: not compared)" % db.label())
                else:
                    print("  stage %-13s only in B" % db.label())
                    all_equal = False
        print("  frame %d: %s%s" % (dumps_a[0].frame if dumps_a else -1,
                                   "all stages equal" if first_bad is None else "first difference at stage " + first_bad,
                                   "; data that differs: " + ", ".join(data_bad) if data_bad else "") +
              ("; records that differ in fields no stage reads: " + ", ".join(records_listed) if records_listed else ""))
    print("RESULT %s" % ("equal" if all_equal else "DIFFERENT"))
    return 0 if all_equal else 1


def quiet_compare(a, b):
    """compare without its output, but with the lines of what differs; returns its exit code."""
    import contextlib
    import io
    text = io.StringIO()
    with contextlib.redirect_stdout(text):
        code = main_with(["compare", a, b])
    lines = text.getvalue().splitlines()
    if code:
        for line in lines:
            print("  " + line)
    else:
        print("  self-compare: %s" % (lines[-1] if lines else "nothing"))
    return code


def cmd_digest(args):
    os.makedirs(args.out, exist_ok=True)
    for key, name in sorted(stage_files(args.path).items()):
        if name.endswith(".digest"):
            continue
        dumps = read_file(name)
        target = os.path.join(args.out, os.path.basename(name)[:-len(".stages")] + ".digest")
        write_digest(dumps, target)
        print("%s: %d dumps, %d bytes -> %s, %d bytes" % (name, len(dumps), os.path.getsize(name), target,
                                                         os.path.getsize(target)))
    return 0


def find_dump(path, stage, occurrence):
    found = [d for d in read_file(path) if d.stage == stage and not d.is_data()]
    if occurrence >= len(found):
        raise SystemExit("%s has %d dumps of stage %s" % (path, len(found), stage))
    if found[occurrence].data is None:
        raise SystemExit("%s is a digest: it has no samples" % path)
    return found[occurrence]


def cmd_edges(args):
    dumps = read_file(args.file)
    d = sibling(dumps, "lf_blocks")
    if d is None or d.data is None:
        raise SystemExit("%s has no lf_blocks buffer (a frame without loop filter, a dump of format 1, or a digest"
                         " without it)" % args.file)
    params = sibling(dumps, "lf_params")
    if params is not None and params.data is not None:
        fields = lf_params_fields(params)
        if fields:
            print("levels: vertical %d, horizontal %d, U %d, V %d; mode_ref_delta_enabled %d, delta_lf_present %d"
                  % (fields[4][1], fields[5][1], fields[6][1], fields[7][1], fields[3][1], fields[1][1]))
    for item in lf_lists(d):
        if (args.dir and item["dir"] != args.dir) or (args.plane is not None and item["plane"] != args.plane):
            continue
        print("%s: %d blocks in rows of %d from record %d, plane of %dx%d 4x4 units%s"
              % (lf_list_name(item), item["count"], item["cols"], item["first"], item["mi_cols"], item["mi_rows"],
                 "" if item["on"] else " (not filtered in this frame: the list was not written)"))
        if args.x is not None or args.y is not None:
            x, y = args.x or 0, args.y or 0
            block = (y // 4) * item["cols"] + x // 64 if item["dir"] == "vert" else (y // 64) * item["cols"] + x // 4
            blocks = [block] if 0 <= block < item["count"] else []
        else:
            blocks = range(min(item["count"], args.count))
        for block in blocks:
            place, _, _ = lf_block_place(item, block)
            print("  block %d (%s): %s" % (block, place, lf_edges_text(item, block, *lf_block_edges(d.data, item, block))))
    return 0


def cmd_mi(args):
    mi = mode_info_of(read_file(args.file))
    if mi is None:
        raise SystemExit("%s has no mode info (mi_grid and mi_pool)" % args.file)
    index = mi.index(args.col, args.row)
    record = mi.record_at(index)
    print("%s: 4x4 unit column %d, row %d of %dx%d: %s" % (args.file, args.col, args.row, mi.cols, mi.rows,
                                                          "no record" if record is None else "pool index %d" % index))
    if record is not None:
        for f in MI_FIELDS:
            print("  %-24s %s%s" % (f[0], field_value(record, f), "  (read by gen_lf_blocks)" if f[0] in MI_LF_FIELDS else ""))
    return 0


def cmd_extract(args):
    d = find_dump(args.file, args.stage, args.occurrence)
    with open(args.out, "wb") as f:
        for p in range(3):
            if d.has_plane(p):
                for row in d.rows(p):
                    f.write(row)
    print("%s stage %s: %s -> %s" % (args.file, d.label(), " ".join(
        "%s %dx%d" % (PLANE_NAMES[p], d.planes[p][2], d.planes[p][3]) for p in range(3) if d.has_plane(p)), args.out))
    return 0


def cmd_compare_yuv(args):
    d = find_dump(args.file, args.stage, args.occurrence)
    bps = d.bytes_per_sample
    width = args.width or d.planes[0][2]
    height = args.height or d.planes[0][3]
    dims = [(width, height), ((width + 1) // 2, (height + 1) // 2), ((width + 1) // 2, (height + 1) // 2)]
    frame_bytes = sum(w * h for w, h in dims) * bps
    with open(args.yuv, "rb") as f:
        f.seek(frame_bytes * args.frame)
        ref = f.read(frame_bytes)
    if len(ref) != frame_bytes:
        raise SystemExit("%s has no frame %d of %dx%d, %d bytes a sample" % (args.yuv, args.frame, width, height, bps))
    print("A %s stage %s\nB %s frame %d\n  %s" % (args.file, d.label(), args.yuv, args.frame, d.info))
    equal = True
    pos = 0
    for p in range(3):
        w, h = dims[p]
        rows_b = [ref[pos + y * w * bps:pos + (y + 1) * w * bps] for y in range(h)]
        pos += w * h * bps
        if not d.has_plane(p):
            continue
        if (d.planes[p][2], d.planes[p][3]) != (w, h):
            print("    %s: sizes differ, dump %dx%d, file %dx%d" % (PLANE_NAMES[p], d.planes[p][2], d.planes[p][3], w, h))
            equal = False
            continue
        rows_a = d.rows(p)
        if rows_a == rows_b:
            print("    %s: equal" % PLANE_NAMES[p])
            continue
        equal = False
        print("    %s: DIFFERS in %d of %d rows" % (PLANE_NAMES[p], sum(1 for y in range(h) if rows_a[y] != rows_b[y]), h))
        diff_pattern(rows_a, rows_b, w, bps, False, args.samples, print)
    print("RESULT %s" % ("equal" if equal else "DIFFERENT"))
    return 0 if equal else 1


def main_with(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("list")
    p.add_argument("paths", nargs="+")
    p.set_defaults(func=cmd_list)
    p = sub.add_parser("compare")
    p.add_argument("a")
    p.add_argument("b")
    p.add_argument("--all", action="store_true", help="the pattern of every stage that differs, not only the first")
    p.add_argument("--two-parsers", action="store_true",
                   help="the dumps are of the two parsers of the decoder (docs/phase2-rav1d.md): a difference in "
                        "the mode info records is listed and does not make the result DIFFERENT. The entropy "
                        "decoder of libaom does not clear a record, so a field the block's kind does not have "
                        "holds what an earlier frame left there, and it keeps the number of warp samples; "
                        "rav1d's sink writes zero. No stage reads such a field: the buffers the stages make of "
                        "the records (pred_blocks, lf_blocks, cdef_info) and every frame buffer are compared as "
                        "always")
    p.add_argument("--samples", type=int, default=12,
                   help="how many differing samples, blocks, units or records to list (default 12)")
    p.set_defaults(func=cmd_compare)
    p = sub.add_parser("digest")
    p.add_argument("path")
    p.add_argument("out")
    p.set_defaults(func=cmd_digest)
    p = sub.add_parser("edges")
    p.add_argument("file")
    p.add_argument("--dir", choices=("vert", "horz"))
    p.add_argument("--plane", type=int, choices=(0, 1, 2))
    p.add_argument("--x", type=int, help="sample column in the plane")
    p.add_argument("--y", type=int, help="sample row in the plane")
    p.add_argument("--count", type=int, default=8, help="blocks to print per list without --x/--y (default 8)")
    p.set_defaults(func=cmd_edges)
    p = sub.add_parser("mi")
    p.add_argument("file")
    p.add_argument("col", type=int)
    p.add_argument("row", type=int)
    p.set_defaults(func=cmd_mi)
    p = sub.add_parser("extract")
    p.add_argument("file")
    p.add_argument("stage")
    p.add_argument("out")
    p.add_argument("--occurrence", type=int, default=0)
    p.set_defaults(func=cmd_extract)
    p = sub.add_parser("compare-yuv")
    p.add_argument("file")
    p.add_argument("stage")
    p.add_argument("yuv")
    p.add_argument("--frame", type=int, default=0)
    p.add_argument("--width", type=int, default=0)
    p.add_argument("--height", type=int, default=0)
    p.add_argument("--occurrence", type=int, default=0)
    p.add_argument("--samples", type=int, default=12)
    p.set_defaults(func=cmd_compare_yuv)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main_with(sys.argv[1:]))
